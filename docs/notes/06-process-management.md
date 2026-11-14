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

**Two divergences, pinned as divergences:** an image's and a private file mapping's line reports `dev` `00:00` and inode `0`, because there is no object left to name — and `00:00` is what this kernel's own `stat` reports for every filesystem, so the two files agree rather than disagreeing. And a *shared anonymous* mapping is named `/memfd: (deleted)`, which is Linux's own name for a nameless `memfd` mapping (measured: `memfd_create("", 0)` + `MAP_SHARED` reports exactly that), because that is what the object here is; Linux reaches the same shape through `/dev/zero (deleted)`, which is an artefact of its own implementation. `memfd_create` now *keeps* the name it used to only validate, as `/memfd:<name> (deleted)` — the spelling both of its readers expect, measured on Linux — so a `wl_shm` pool's mapping says which buffer it is instead of leaving the column blank. `numa_maps` and `smaps_rollup` **are** written (they were not when this paragraph was first drafted): the same accounting in two other spellings, measured and pinned — `t-numamaps` 20 claims and `t-smaps-rollup`, `docs/notes/15-boot-flow.md` §15.81. (`/proc/<pid>/smaps` *is* written now, and it is this file's records with a field block under each: item 54, `docs/uapi/` §5.1, §15.73.)

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

**The blocking wait that is left, measured (2026-11-04).** With the adopt-and-hold patch in the tree and one
print per `child_event` answer (the predicate every blocking wait reads), the last two lines before
`t-waitid`'s timeout are

```
mpos-dbg: CE me=6 id=7 stops=false cont=false -> false
mpos-dbg: CE me=6 id=7 stops=false cont=false -> false
ok:    and leaves the caller's rusage record untouched
```

- `stops=false, cont=false` means a plain `WEXITED` wait: the parent is waiting for child 7 to *exit*, and
  `child_event_in` says there is no event - so the wait parks, which is correct **if** the child is still
  alive. It is not: the child was already collected earlier by `and it still exits with its own code`
  (`waitid(..., WEXITED)` on the same id), so slot 7 is either a corpse with no retired record left or a
  reused slot whose `kind_visible`/`is_child` answers "a child" that will never produce an event.
- That is the last door: `handle_wait`'s blocking path asks `is_child` (still true) and then waits for an
  event that `take_retired_current` has already handed over once. Linux's answer for "the child is gone and
  collected" is `ECHILD`, and the shape that would give it here is: a blocking wait must re-check
  `take_retired_current`/`has_matching_children` inside its own park predicate, not only before it parks.
  **Measured false (2026-11-06).** The blocking second wait does answer `ECHILD` here
  (`build/scratch/reapwait/p.c` on a current host and the same probe on the machine): `second (WNOHANG)
  waitpid=-1 errno=10`, `second (blocking) waitpid=-1 errno=10`, and `wait4`/`waitid` the same - the
  `is_child` check already refuses the collected number before any park. The parked reading above was a
  *different* defect wearing the same clothes: the wait was on the collected child 7 at a moment when slot 7
  had been handed to another task, and what the blocking path then waited for was that **stranger's** exit.
  That is §6.89's defect, and its fix is not in the wait path at all.
- The adopt-and-hold patch itself is validated by this run: with it in, the stop/resume section passes and
  only this wait is left (`=== FAIL t-waitid timeout=120s`, one lane, `pass=256 fail=1`); `t-rename`'s
  earlier red did not recur without the debug prints.

### 6.87 The futex tests shared one map and raced each other — LANDED (2026-11-06)

`cargo xtask test` has spent several sessions reporting **"the kernel's host unit tests failed"** with no
failing test name in the captured tail: stage 1 runs in a thread beside the first arm's sweep, and
`run_captured` keeps only the last 40 lines of a combined stdout+stderr, so the tail that reached the gate
was compiler warnings plus `error: test failed, to rerun pass -p mpos --lib`. Running the gate's exact stage-1
command by hand was green every time (726 + 9 + 15 + … , 0 failed), which is how it stayed "unexplained".

It is `src/sched/futex.rs`'s own tests. `static WAITERS: Mutex<RefCell<Vec<Waiter>>>` (`src/sched/futex.rs:46`)
is one module global, and the addresses the tests use are **deliberately repeated**: `a_multi_wait_names_the_earliest_word_when_several_fire`
and `a_requeue_with_no_count_on_one_side_leaves_that_half_alone` are both about `0xa000`/`0xb000`. Two of them
running on different test threads therefore see each other's waiters:

```
thread 'sched::futex::tests::a_requeue_with_no_count_on_one_side_leaves_that_half_alone' panicked at
src/sched/futex.rs:608:9:
assertion `left == right` failed
  left: 1
right: 0
```

(line 608 is `assert_eq!(waiting_on(b), 0);`, the "the destination never hears of them" claim, in a test whose
own words are "the destination never hears of them").

**The counterfactual, 20 runs each way of `cargo test --lib --target x86_64-unknown-linux-gnu -p mpos futex::tests`:**
without the lock **13/20 green**; with it **20/20**. Nine parallel `cargo test … -p mpos -p mpos-err` runs also
failed twice (`par-3-2.log`, `par-2-3.log`) while the same command alone never did.

**Fix.** Every test in the module takes `crate::sched::test_lock::take()` as its first statement, and the module
carries a header saying why. This is not new policy: `AGENTS.md` §3 already says *a test that touches a module
global takes that module's test lock*, and it names this exact failure shape (the devpts option tests, 5 of 40
runs). The futex module was simply never converted.

**Why it hid for so long.** The module's tests are fast and mostly independent; only the pair that shares
`0xa000`/`0xb000` races, and only when the harness happens to schedule them on two threads at once. A single-run
green - which is what every hand check did, and what `run_captured`'s tail suggested - is not evidence that the
suite is stable.

## §6.88 A stop is a record, not a sleep: `trace::park` returned on the first wake of any kind (LANDED 2026-11-06)

§6.79 named the shape - a stop published by `trace::park`, and any wake ending it - and §6.86 recorded seven
attempts to pin it, every one of which either stayed green or hung `t-waitid`. This section is the one that
landed, and the reason the previous seven could not: **the fixture was asking a question a stop-state copy
cannot answer, and the pin had to be about whether the task ran.**

**The defect, in the shape that made it invisible.** `trace::park` published the stop record, woke the tracer
and the parent, and then called `t.sched.sleep(id, priority, u64::MAX)` exactly once. A parked task at that
moment has **two** claims on it that disagree: a `trace.stop` record saying "stopped", and an entry that the
scheduler's picker still pops from the ready queue - because `sleep` adds a heap entry but nothing removed the
task from ready. The picker therefore handed the task back, `sleep` returned immediately, and `park` fell
through with the stop record still installed. On the isolated fixtures this never showed: nothing else in those
programs woke the task inside the window. Under the sweep's six concurrent machines an unrelated wake (a timer,
a signal, any `sched.wake`) landed inside it, and the stopped task simply ran on - `t-ptrace-attach`'s detach
claim reads the corpse: `a detach leaves a stop the user asked for alone (state "", collected true)`.

**The fix, in two halves that have to move together.**

1. `src/sched/policy.rs` gained

   ```rust
   pub fn leave_ready(&mut self, id: TaskId)
   ```

   which drops the task from every ready band. Its doc comment states the invariant this defect violated: a
   parking task waits on a **record**, not on a condition, so it must not be reachable by the picker while the
   record stands. Without this, `sleep` is a suggestion, not a park.
2. `src/sched/runtime/trace.rs`'s `park` now calls `t.sched.leave_ready(id)` before `sleep`, and wraps the whole
   of `sleep` in a loop that re-parks while the stop record is still there. Two wakes legitimately do not end a
   stop and are named in the code: the tracee's own timer expiring before it stopped, and any signal that is not
   one of the four that end a group stop. The one exception is `SIGKILL`: a stopped task that is killed must
   die, so the loop checks the pending `SIGKILL` bit under the same table lock before deciding to re-park and
   takes `die_of_fatal_signal(SIGKILL)` when it is set. That check is what previous attempts were missing; it is
   why the earlier `hold`-style loops hung `t-waitid`, which kills a stopped child and then blocks in
   `waitid(WEXITED)`.

**The counterfactual, and the two pins that are not it.** Two fixtures were tried and rejected because the
unfixed kernel passed them:

- the 100-detach claim in `t-ptrace-attach` (`a detach leaves the user's stop alone, 100 times over`): green with
  and without the fix, because nothing in that fixture produces a wake that is not a resume. This is §6.78a's
  lesson in another costume - a poll window is not a state assertion, and a fixture that cannot aim the wake
  cannot see the defect.
- `PTRACE_SETREGS` on a stalled tracee (`a stop outlives a timer the tracee armed for itself (answer ...)`): also
  green without the fix, because the *record* survives anyway - `set_trace_regs` only asks whether `trace.stop`
  is `Some`, and the old `park` left it there even as the task ran. The register answer is not the observable.

What is observed is **whether the stopped task ran past the stop**. The shipped fixture is
`user/tests/src/bin/t-seccomp.rs`'s `claim_a_stop_outlives_the_tracees_own_timer`: the tracee arms a 50 ms
`ITIMER_REAL` immediately before a judged call that stops it under `SECCOMP_RET_TRACE`, the tracer sleeps 200 ms
(four times the timer), and the claim is

```rust
let mut probe = 0i32;
let collected = unsafe { libc::waitpid(child, &mut probe, libc::WNOHANG) };
r.check(
    &format!("and the tracee has not run past it (waitpid {collected})"),
    collected == 0,
);
```

Measured, one boot each way, kernel and loader both rebuilt (`boot/src/lib.rs:176` embeds the kernel - a `-p
mpos` rebuild alone measures the old one, §6.77b):

| kernel | reading |
|---|---|
| without `leave_ready` + the re-park loop | `ok: a stop outlives a timer ... (answer Some(Ok(0)))` then **`FAIL: and the tracee has not run past it (waitpid 6)`**, `=== t-seccomp exit=1` |
| with the fix | `ok: and the tracee has not run past it (waitpid 0)`, `ok: the traced timer's tracee then runs to its exit (status 0x0: exited=true)`, `PASS`, `=== t-seccomp exit=0` |

Linux answers the same way (`build/scratch/stoppedsig/t.c`): a stopped task's timer expiring does not release
the stop - six samples 100 ms apart all read state `T` and `waitpid` 0, and only `SIGCONT` ends it. The tracee's
pending `SIGALRM` after the resume is a signal-delivery stop the tracer must answer, which is why the fixture's
tail continues it while `WSTOPSIG(status) == SIGALRM` rather than assuming one `PTRACE_CONT` reaches the exit.

**Gate.** `build/gate-parkfinal.log`, `TMPDIR=$PWD/build/tmp cargo xtask test`, both arms: stage 1 `1224 tests, 0
failed`; x86_64 `pass=258 fail=0 (258 built)`; aarch64 `pass=257 fail=0 (257 built)`; `mpos-kunit 35 passed, 0
failed` on both. The one red this section exists to close - `t-ptrace-attach`'s detach claim, which had appeared
once on an aarch64 lane in `build/gate-futexlock.log` - is green on both arms in this run, and the 100-cycle
detach claim stays in `t-ptrace-attach` as the regression guard for the detach path it does cover.

**The rule.** A wait that is *decided by a record* must (a) make the task unreachable to the picker, and (b)
re-check the record after every wake, because the wake set is not the resume set. A park that does only (b), or
only (a), reads as correct in isolation and fails under load - which is why the fix needs the load to be
measured, not just the fixture run once.

## §6.89 A pid number is a promise on Linux; here it is the task slot (2026-11-06)

Found while re-measuring §6.86's "last door". That suspicion turned out to be false (see the correction in
§6.86), and chasing the same probe one step further produced a **real, reproducible divergence** that the
roadmap had only ever mentioned in passing.

**The shape.** I hold a number for a child I have already collected. Somebody else's process takes that slot.
What do `kill` and `/proc/<n>` answer?

Linux, `build/scratch/reapwait/kslot.c` on a current host (7.2.11), `musl-gcc -static`:

```text
my child 72463 collected
stranger (helper's child) 72464
kill(72463, 0) = -1 errno=3 (ESRCH)
the stale number: /proc/72463 -> ENOENT
kill(72463, SIGTERM) = -1 errno=3 (ESRCH)
after SIGTERM: /proc/72463 -> ENOENT
```

This machine, the same probe booted as `t-kslot` (`build/boot-kslot1.log`, `BOOT EXIT=0`):

```text
=== RUN t-kslot
my child 7 collected
stranger (helper's child) 7
kill(7, 0) = 0 errno=0 (NOT ESRCH)
the stale number: /proc/7 state=S
kill(7, SIGTERM) = 0 errno=0 (NOT ESRCH)
after SIGTERM: /proc/7 state=Z
NOTE: the stranger reused my number
=== t-kslot exit=0
```

The stale number did not merely resolve - `kill(<it>, SIGTERM)` **ended an unrelated process**, and
`/proc/<it>` read that process's state. Everything a caller does with a pid it remembered is wrong.

**Why the number came back.** A pid here *is* the task slot: `pidns::PidNs::number_of`/`task_of` are the
identity function at the root level (`crates/pidns/src/lib.rs:215-227`), `insert`/`forget` return immediately
there, and the slot allocator hands out the lowest free index (`src/sched/runtime/task.rs:2193`). Release a
child and the very next `fork` takes its number. Linux's answer to the same question is a refcounted
`struct pid` whose lifetime is its own, which is exactly what makes a number a promise; this crate's own
header says so and names this consequence instead of hiding it ("the *init* namespace still numbers by slot,
so slot reuse there is exactly what `docs/notes/` §7.1 records as an open problem, unmoved by this crate").

**Part of it is already safe, measured - and the measurement found a worse hole beside it.** `build/scratch/
reapwait/{stranger,slotowner,p}.c` booted as `t-stranger`, `t-slotowner` and `t-reapwait` all read
`waitpid(<stale>, WNOHANG) = -1 errno=10 (ECHILD)` on the machine, the same as Linux, because the number
named a **live** stranger and `handle_wait` asks `crate::sched::is_child(id, me, kind)` (around
`src/syscall/linux/proc.rs:568`) before it does anything with the slot. `t-waitid`'s *a second wait for a
collected child answers ECHILD, not a park* (`user/tests/src/bin/t-waitid.rs:435`) and `t-wait4`'s *and a
second wait for it is ECHILD* (`user/tests/src/bin/t-wait4.rs:403`) pin the collected-child half. The
blocking second wait is `ECHILD` too, which is what refutes §6.86's last-door reading: there was never a
park on a collected child, there was a *stranger's* exit being waited for.

**Correction (2026-11-06): that "safe" was measured on the live stranger only, and it was false one step
further on.** A death's **record** outlives its slot, and the old ownership test asked the *slot* - "is it
empty? then the pending record must be mine" - which is backwards, because a slot is empty precisely
*because* the record outlived its task. So a wait for a number whose stranger had **already exited and not
been collected** collected that death, and the real parent was left with `ECHILD` for its own child. Not a
pid-reuse problem at all, and it needed no slot reuse: the slot still held the zombie. The fix and its
measurement are §6.90 - the record now carries its owner, and every reader asks the record instead of the
table. This section's four doors are about a *live* tenant taking a number; they stay open.

**The doors that are open** are the three that resolve a number to a slot without asking whether the number
still means the task it meant:

| door | code | answer for a stale number |
|---|---|---|
| `kill`/`tkill`/`tgkill` | `named_process` / `named_thread`, `src/sched/runtime/child_signal.rs:1705`/`:1741` | the stranger's process |
| `/proc/<n>` | `resolve_pid`, `src/sched/runtime/pid.rs:178`, used by `src/vfs/procfs.rs:1276` | the stranger's directory |
| `/proc/<n>/limits`, `getrlimit(pid)` | `rlimits_of`, `src/sched/runtime/task.rs:588` (`TaskId(pid)` directly) | the stranger's table |
| `/proc/<n>` snapshots | `snapshot`, `src/sched/runtime/procfs.rs:466` (`t.tasks.get(pid)`) | the stranger's |

**Why this is not a patch, and what closing it costs.** The number has to *become spent* at release, and in
this kernel the number and the slot are the same object, so no amount of checking at the doors can help: at
the moment of the check the slot genuinely holds a live tenant and the number genuinely is that tenant's.
The fix is therefore at the allocation end - the init namespace stops numbering by slot and gets the
monotonic table every nested namespace already has (`Numbers { next, by_number, by_slot }` in
`crates/pidns/src/lib.rs`, which never reuses a number). That is a design change with a stated cost, not a
bug fix, and it is why this section names the gap instead of stubbing it (AGENTS.md §3 rule 5):

- `crates/pidns/src/lib.rs`'s header calls the identity map at the root **the load-bearing decision** in the
  file, and the host pin `the_init_namespace_numbers_by_slot()` asserts `root.number_of(7) == Some(7)`,
  `root.task_of(7) == Some(7)` and `root.task_of(63) == Some(63)`. Both move, with the measurement.
- every pid the machine reports changes value once (`getpid`, `wait`, `/proc/<n>`, `%P` in
  `/proc/self/mounts`, a VT's promised pid, a render task's recorded waiter) - the divergence has to be
  argued for rather than slipped in under a "no behaviour change" commit.
- the reason the identity map exists at all is byte-for-byte invariance for a machine that never unshares;
  that sentence stops being true and the header must say what replaces it.

The cheap half that is worth doing first, and is **not** done here either: the four doors in the table above
should share one resolver whose contract states what it can and cannot promise, so the next reader does not
have to rediscover that two of them bypass the namespace map entirely.

**Pinned as a measurement, not as a check.** The probe is `build/scratch/reapwait/kslot.c` and its two
siblings; they are booted by hand, because a check that asserts Linux's answer would be red on this machine
every run and the sweep must stay a statement about the kernel (AGENTS.md §5's group rule - a known
divergence measured is a `diagnostic`, not a check). The claim that *is* pinned is the safe half, already in
`t-waitid` and `t-wait4`.

## §6.90 A death belongs to the parent that had it — `Retired::parent` (LANDED 2026-11-06)

Found while booting §6.89's instrument for its machine reading. The instrument asked the natural follow-up
question of a stale number - what does `waitpid` answer - and got the stranger's exit status back.

**Measured, this machine** (`t-kslot`, `build/probe/oneshot/serial.log`, boot `BOOT EXIT=0`):

```
=== RUN t-kslot
my child 7 collected
stranger (helper's child) 7
kill(7, 0) = 0 errno=2 (NOT ESRCH)
the stale number: /proc/7 -> S
kill(7, SIGTERM) = 0 errno=2 (NOT ESRCH)
after SIGTERM: /proc/7 -> Z
NOTE: the stranger reused my number
waitpid(7, WNOHANG) = 7 errno=2 (NOT Linux's answer)
DONE
```

The `waitpid` line is the new fact. §6.89 had recorded the `wait` path as the safe half, and it was - for the
numbers it was measured with. Every one of those probes asked about a **live** stranger, and `is_child` is
enough to refuse a live task that is somebody else's child. The moment the stranger's own victim was a
**zombie**, a different door answered, and it did so before `is_child` was ever consulted.

**Measured, Linux** (`build/scratch/reapwait/steal.c`, musl-gcc -static -O1, this host's 7.2.11): a helper
forks a child that `_exit`s at once, leaves the record uncollected, and sleeps on a pipe; an unrelated
process waits for it:

```
the helper's child is 78455 (its record is pending, uncollected)
MY waitpid(78455, WNOHANG) = -1 errno=10 (ECHILD, Linux's answer)
parent of it: waitpid(78455) = 78455 errno=0 status=0x700
```

and the orphan case (`orphan.c`) - the helper exits without collecting, so the zombie belongs to init:

```
helper 78951 collected; its child 78952 is an orphan
grandparent waitpid(78952, WNOHANG) = -1 errno=10 status=0x0 (ECHILD)
```

Linux answers `ECHILD` to any process that is not the owner, and the owner still gets its status. Measured
here (same shape, `t-steal`): **`MY waitpid(7, WNOHANG) = 7 errno=0 (I TOOK SOMEONE ELSE'S DEATH)
status=0x700`**, and the helper's own collect then failed.

**Why the check could not be made where the number is.** The number was fine. The slot was fine - it still
held the zombie. What was wrong is that nothing in the system knew **whose** the pending death was. The
record (`src/sched/runtime/task.rs`'s `Retired`) carried the code, the numbers, the pgid, the ids, the times
and the tracer - everything except an owner. Asked "is this record mine", three readers answered it by
looking at the **slot**:

```rust
let free = t.tasks[id.0].is_none();
let ours = free || t.tasks[id.0].as_ref().is_some_and(|_| waitable(t, id, parent, kind));
```

in `any_child_code`, `scan_child_event` and `has_matching_children`, while `take_retired_for` used
`t.tasks[id.0].as_ref().is_none_or(|task| task.parent == Some(me))`. Every one of those reads `free` as
"mine" - and `free` is exactly the state a record creates by outliving its task. The inference is not merely
incomplete, it is inverted.

The fourth door was worse: `src/syscall/linux/proc.rs:498`'s `handle_wait` took the record
(`take_retired_current`, `:579`) **before** it asked `is_child` (`:588`). So even a correct ownership test
inside the record's own accessor would have been reached late, with the code already handed out.

**The fix: the record carries its owner.** `Retired` gained

```rust
/// Whose death this was, as a **group** - the parent the dying task had at
/// the moment it died, or `None` for a task that never had one (init, and
/// the boot task).
pub(crate) parent: Option<TaskId>,
```

- **Written** at `src/sched/runtime/child_exit.rs`'s `exit`, in the same `group_dead` arm that builds the
  record, from the dying task's own `parent` - resolved to a **group**, because ownership is a group fact
  (a child forked on a worker thread is the leader's to collect, `waitable`'s rule). A task with no parent
  records `None`, and `None` matches no caller's group.
- **Read** by all four doors, in place of the table: `take_retired_for` compares `retired.parent` with the
  caller's group and falls back to the tracer rule (a tracer still gets the code, but as a **release**: the
  record stays on file for the real parent). `any_child_code`, `scan_child_event` and `has_matching_children`
  replace their `free ||` clause with `retired.parent == my_group`.
- **Moved** when the reparenting walk moves the corpse. A child that died *before* its parent did has its
  record on file already, naming a group that is now gone; Linux's `forget_original_parent` reparents
  zombies with the children and the new parent collects them. The same walk in `exit` now retargets every
  record whose owner was the dying group to the adopter's group, so an orphan's death is collectable by
  exactly the process that adopted the corpse - and by nobody else, which is `orphan.c`'s reading.

**Counterfactual** (`build/boot-steal-cf.log`, kernel and loader both rebuilt with the fix stashed):

```
ok:    the helper's child 7 exists and is not ours
FAIL:  a wait for somebody else's uncollected child is ECHILD (waitpid 7, errno 2)
FAIL:  and its own parent still collects it (helper exit 1)
ok:    a number this program never forked answers ESRCH
ok:    this program's own child is still collectable (waitpid 6, exit 3)
ok:    and this program still reads its own /proc entry (state R)
FAILED
=== t-steal exit=1
```

With the fix (`build/boot-steal2.log`, same tree, same boot recipe) all six are `ok:` and the program ends
`PASS`, `=== t-steal exit=0`. The pin is `user/tests/src/bin/t-steal.rs` and it is a **check**, not a
reading: Linux's answer is the answer, and the host route (`MPOS_ON_HOST=1`) reads the same six `ok:` lines.

**What it does not fix.** §6.89's four doors stay open: they are about a **live** tenant taking a number, and
the record's ownership cannot help there - at the moment of the check the number really is the live
stranger's. This section closes the door that was about a **dead** tenant, which is the one that could be
answered without changing how numbers are allocated.

**The rule.** A fact about a death outlives the task, so it must be stored **with** the death. Reading it off
the task table is not a stale read, it is a read of a different subject: the table is the living, and the
record's whole reason to exist is that the living moved on.

---

## §6.91 A number handed to a syscall is read in the caller's namespace (LANDED 2026-11-06)

Found while costing Item 202 (§6.89), and it is not that defect. §6.89 is about a number that no longer names
*any* task - a recycled slot. This is about a number that is completely valid **in the namespace it was
written in**, handed to a door that indexes the task table with it. On a machine that never unshares, those
two numbers are the same integer, which is why it read as correct for the whole life of these doors.

The fixture is the shape a container runtime uses:

```
unshare(CLONE_NEWUSER)  →  write uid_map/gid_map  →  unshare(CLONE_NEWPID)  →  fork
```

`unshare(CLONE_NEWPID)` renumbers the caller's **children**, so the unsharing process stays where it was and
its next `fork` is the namespace's init. The init then forks `work`, which has number **2** in that namespace
and gives itself a distinctive `RLIMIT_NOFILE` of `123/456` and a nice of 7. Every call below is made by the
init with the number `2` - a number that names exactly one task, here.

**Measured on Linux first** (`build/scratch/nsnum/p.c`, `q.c`, `r.c`, musl-gcc -static -O1, 7.2.11):

```
the namespace's init has pid 1
its child has pid 2 there
prlimit64(2) = 0 errno=0 cur=123 max=456 (want 123/456)
sched_getparam(2) = 0 errno=0
sched_getscheduler(2) = 0 errno=0
getpriority(2) = 5 errno=0
setpriority(PRIO_PROCESS, 2, 7) = 0 errno=0
  read back via getpriority(2) = 7 errno=0
```

**Measured on this machine** (`build/boot-nsprlimit1.log`, `build/boot-nsdoors1.log`, `build/boot-nsset1.log`):

```
prlimit64(2) = 0 errno=0 cur=1024 max=1048576 (want 123/456)     ← a stranger's table, answered 0
sched_getparam(2) = -1 errno=3
sched_getscheduler(2) = -1 errno=3
getpriority(2) = 5 errno=0                                        ← right: it goes through pid_task
/proc/2/limits Max open files -> 123 456                          ← right: procfs goes through resolve_pid
setpriority(PRIO_PROCESS, 2, 7) = 0 errno=0
  read back via getpriority(2) = 0 errno=0                        ← wrote a stranger, reported success
```

Two of those are the worst answer this repo has a name for (`AGENTS.md` §3 rule 2): **success for a thing not
done**. `prlimit64` returned `0` and filled the caller's struct from a task the number did not name.
`setpriority` returned `0` and wrote its nice into that other task - so a set and a get through one number
were about two tasks, and the caller could not tell.

The pair disagreeing is the whole evidence. `getpriority` resolves the number (`super::proc::pid_task` ->
`resolve_pid`) and was right; `setpriority` took `who` as the slot and was wrong. `/proc/<n>/limits` was right
because procfs turns the number through `resolve_pid` before it builds a directory. Three doors into the same
concept, two of them resolving the name and one not.

**The doors that took a raw number as a slot.** `handle_prlimit64`'s cross-process half, through
`rlimits_of_pid` -> `crate::sched::rlimits_of(pid)` (`src/sched/runtime/task.rs`), which built
`TaskId(pid)` directly. `live_thread` (`src/syscall/linux/posix.rs`), through `crate::sched::snapshot(pid)`
- which is every one of `sched_getparam`, `sched_setparam`, `sched_getscheduler`, `sched_setscheduler`,
`rr_get_interval`, `sched_setattr` and `sched_getattr`. And `handle_setpriority`'s `PRIO_PROCESS` arm, which
handed `who` to `set_nice`.

**The fix.** Each resolves first, with the same function the correct siblings already used:
`crate::sched::resolve_pid(pid as i64)` in `live_thread` and in the `prlimit64` arm, and
`super::proc::pid_task(who as isize)` in `setpriority`. No new resolver was invented: `pid_task` and
`resolve_pid` are the two the kernel already routes every other pid through, and `resolve_pid`'s own doc
comment had been saying "the init namespace's map is an identity function and not a map" for as long as the
defect had existed.

**Why it looked correct on every machine that never unshares.** In the init namespace `task_of(n)` is
`Some(n)`, so `resolve_pid(n)` returns `TaskId(n)` and the two spellings agree. Every `t-*` program that
names another process - `t-kill-numbers`, `t-waitid`, a tracer's `PTRACE_SETREGS` on a pid - runs in the init
namespace, so a raw number and a resolved one were the same integer. The change therefore moved **no
existing reading**, which is also why it needs a namespace to be measured at all: the counterfactual below is
the only thing that can tell the two spellings apart.

**The pin** is `user/tests/src/bin/t-nsnumbers.rs`, a check, not a reading. It enters the namespace the way
the fixture above does, the child sets `RLIMIT_NOFILE` 123/456 and nice 7, and the init asks about it by the
number it has there. Two claims are made in the **parent** as well, from the init's exit status, because the
claims themselves are made in a child: `Report::failed()` was added for that (`user/tests/src/lib.rs`) - a
program whose claims are made below a `fork` cannot call `finish` there without taking the child out before
the parent reports.

**Counterfactual** (`build/boot-nsnumbers1.log`, the fix stashed out of `src/` and **both** the kernel and the
loader rebuilt - `boot/src/lib.rs` embeds the kernel, so rebuilding only `-p mpos` measures the old one,
§6.77b):

```
ok:    the namespace's init reads its own number as 1 (reads 1)
ok:    its child answers to the number 2 there (is 2)
FAIL:  prlimit64 names the task the number does, not another slot (1024 / 1048576)
FAIL:  setpriority writes the task the number names (set=0, read back=0)
ok:    the machine gives an unprivileged program a pid namespace (exit 1)
FAIL:  every reading in the namespace named the task it was asked about (exit 1)
FAILED
=== t-nsnumbers exit=1
```

With the fix (`build/probe/oneshot/serial.log`, same tree and same boot recipe) all six are `ok:`, `PASS`,
`=== t-nsnumbers exit=0`. `MPOS_ON_HOST=1` reads the same six.

**Two claims deliberately not in the pin.** `sched_getparam`'s *success* cannot be claimed on the host this
repo is developed on: the container's seccomp filter answers `ENOSYS`, which is the sandbox's answer and not
Linux's 0, so a pin asserting success would be measuring the sandbox. And `/proc/<n>/limits` is not a claim
either, because it is a *different* rule from the one this section is about: a procfs directory resolves its
number in the namespace the procfs was **mounted** in, while a syscall resolves it in the **caller's**. On the
host the mount is the host's namespace, so `/proc/2` reads `kthreadd` there - measured, and not a defect.
Both readings are the table above instead.

**The rule.** A pid number is meaningful only in a namespace, so a door may not take one and use it as a slot:
resolve it first, and let the resolver own the question of what "names a task" means. The tell is a pair of
siblings where one resolves and one does not - a set that reports success while its get reads the old value is
this defect, not a lost write.

---

## §6.92 An affinity mask is a request about a number, so the number comes first (LANDED 2026-11-06)

`sched_getaffinity`/`sched_setaffinity` were the last two pid-taking calls whose handler never looked at the
pid. Both bound the argument as `let (_pid, len, mask) = (args[0], args[1], args[2]);` and went straight to
the mask, so a number that named nothing - a collected child, a stranger's, `-1` - answered `0`, and a number
that named *somebody* was quietly answered about the caller. That is the shape §3's rule 2 forbids in its
worst form: it is not a refusal, it is a success about a different subject.

The rest of the family was wrong too, and this machine had no way to notice either: it has one CPU, so every
task's affinity is the same bit and the only visible part of the answer was the byte count.

**Linux, measured** (`build/scratch/aff/q2.c`, `r3.c`, `r4.c`; host kernel 7.2.14, 16 CPUs, `sizeof(long)` 8):

| call | Linux | this kernel, before |
|---|---|---|
| `set(<no such pid>, 8, bit 0)` | `ESRCH` | `0` |
| `get(<no such pid>, 8, mem)` | `ESRCH` | `8` |
| `get(-1, 8, mem)` | `ESRCH` (`-1` is not "the caller") | `8` |
| `set(self, 1, bit 0)` | `0` | `EINVAL` |
| `set(self, 4, bit 0)` | `0` | `EINVAL` |
| `get(self, 12, mem)` | `EINVAL` (whole words) | `8` |
| `get(self, 16, mem)` | `8`, bytes 8..16 left as the caller had them | `8`, **tail zeroed** |
| `set(<no such pid>, 0, NULL)` | `ESRCH` (length judged after the pid here) | `EINVAL` |
| `get(<no such pid>, 0, NULL)` | `EINVAL` (length judged *before* the pid here) | `EINVAL` |
| `set(<no such pid>, 8, NULL)` | `EFAULT` (the copy happens first) | `EFAULT` |

The two asymmetric rows are the interesting ones: the getter judges its **length** before its **pid**, and the
setter the other way round, except that a non-empty mask is copied before either. That is not a rule either
direction derives - it is what Linux's own ordering produces, and both are in the pin.

**Five probe iterations to get there.** The first readings contradicted each other because `printf` does not
guarantee argument evaluation order and one probe put a `set()` and a `get()` in the same call's arguments, so
the getter wrote bit 0 into the buffer the setter then read - which is also how "an empty mask is accepted"
appeared and disappeared across runs. `q2.c` measures one call per statement with a freshly `memset` buffer per
case; `r2.c`'s "the tail is untouched" was itself wrong because it prefilled with `0xff` and could not tell
"untouched" from "filled with ones". The lesson is the one this repo keeps re-learning: a probe that prints two
calls in one line is measuring the interaction, not the calls.

**The pin** is `user/tests/src/bin/t-affinity-rules.rs`, 15 claims. On the unfixed kernel nine are red
(`build/boot-affrules-pin1.log`); with the fix all fifteen are `ok:` and `PASS`
(`build/probe/oneshot/serial.log`), and `MPOS_ON_HOST=1` reads the same fifteen against real Linux. The two
authority claims took a second counterfactual, because they are a separate door: with only
`affinity_may_retarget` removed and the rest of the fix kept, `build/boot-affperm-cf.log` reads
`FAIL:  a dropped program cannot retarget its root parent (uid 65534)` beside
`ok:    and a getter asks for no authority at all (uid=65534 get=8 geterrno=3 set=-1 seterrno=1)`, and putting
it back reads `PASS` (`build/boot-affperm3.log`). Note what the dropped reading says about the *getter*: it asks
about a root task and gets `get=8 geterrno=3`, where `3` is the `errno` left over from the failed `setuid` - the
getter itself returned the eight bytes it filled.

**The fix**, `src/syscall/linux/abi.rs`: one private helper,
```rust
fn affinity_target(pid: usize) -> Option<()> {
    if pid == 0 { return Some(()); }
    crate::sched::resolve_pid(pid as i64).map(|_| ())
}
```
because `0` is the caller while every other number goes through the door that already knows what a number
means (§6.91), and `-1` is a number like any other here - Linux measured `ESRCH`, not "the caller" as `wait4`
would read it. The getter then judges `len` (a whole number of words, at least one), resolves the number,
copies the mask and writes **only the words it fills**; the setter copies what it was given (any length from
one byte), resolves the number, and reads one byte to see whether CPU 0 is named. The old tail-zeroing block
is gone: filling bytes the caller did not ask about is a write, and Linux does not make it.

**The authority half, and a sentence above that was wrong.** The paragraph that stood here said the setter's
`CAP_SYS_NICE` check and a getter-side `ptrace_may_access` check were both missing, and that neither could be
pinned because the payload runs as root. Re-measured (`build/scratch/affperm/s.c`, `t.c`, `u.c`, `v.c`; uid 1000,
empty `CapEff`, 16 CPUs), the rule is narrower and the second half does not exist:

| call | another user's task | the same user's task, anyone's child | own task |
|---|---|---|---|
| `sched_setaffinity` | `EPERM` | `0` | `0` |
| `sched_getaffinity` | `0` (16 CPUs) | `0` | `0` |

So the **getter asks for no authority at all** - the earlier sentence about a ptrace check was wrong, and it was
wrong in the direction that would have made this kernel refuse reads Linux serves. The setter needs
`CAP_SYS_NICE` only when the target belongs to another user; blood relation is not part of it, because a
same-uid stranger's child is retargetable. The refusal order is `EFAULT` (a non-empty mask is copied first) →
`ESRCH` → `EPERM` → `EINVAL` (the mask names no CPU), measured with `set(<pid 1>, 0, NULL) => EPERM` against
`set(<no such pid>, 8, NULL) => EFAULT`: authority sits **before** the mask judgement, so a caller without the
capability cannot probe the machine's CPU count one refusal shape at a time.

The fix is `affinity_may_retarget` in `src/syscall/linux/abi.rs`: the caller's own task is always served,
another task is compared with `crate::cred::kill_ok` (the four-id check `kill` already uses, so "same user"
means what it means everywhere else here), and only then is `CAP_SYS_NICE` consulted. That it can be pinned at
all is the other correction: the sweep's payload runs as root, but a *child* can `setgid`/`setuid` to `nobody`
first and then ask about its root parent - `t-proc-access` has done exactly that for its own access claims for
months. The pin's `authority_probe` is that child, and it reports the uid it ended up as rather than assuming
the drop happened, because a host run cannot drop and is then the same user as its parent (the allowed side of
the same rule).

**The rule.** A call that takes a number and acts on a *task* resolves the number first, and refuses with
`ESRCH` when it names nothing - even when the rest of the arguments are also bad, unless Linux's own ordering
says the other refusal comes first. A handler that binds `let (_pid, ...)` has already answered about the
caller, which is right only for the one caller that passed 0.

## §6.93 `setpriority` asks whose task, and says so with two different errnos (LANDED 2026-11-06)

`handle_setpriority` had no authority check in any of its three arms. A program running as one user could
lower the nice of *another* user's task and the call answered `0` - and wrote it, so a later `getpriority` on
that task read the intruder's value. The doc comment said why it was "not needed": *"the unprivileged-lowering
rule is moot for this single-rooted kernel, which accepts the whole range."* That sentence was inherited, never
measured, and wrong in three separate ways.

**Linux, measured** (`build/scratch/niceperm/{y,g2,s2,z,ns,w,x2,y2}.c`; musl static, host kernel 7.2.14,
uid 1000, empty `CapEff`, `RLIMIT_NICE` 0/0, every process at nice 19):

| call | Linux | this kernel, before |
|---|---|---|
| `set(PROCESS, <another user's task>, 19)` (a *lowering*) | `EPERM` | `0` |
| `set(PROCESS, <another user's task>, -5)` | `EPERM` | `0` |
| `set(PROCESS, 0, 0)` (raising itself) | `EACCES` | `0` |
| `set(PROCESS, 0, <its current nice>)` | `0` | `0` |
| `set(PROCESS, <same-uid stranger>, <its own nice>)` | `0` | `0` |
| `set(USER, <own uid>, 19)` | `EPERM` | `0` |
| `set(PGRP, <a group with any foreign member>, …)` | `EPERM` | `0` |
| `set(USER/PGRP/PROCESS, 424242, …)` | `ESRCH` | `ESRCH` |
| `get(anything)` | no authority check at all | no authority check |

**Two refusals, not one.** The measurement that took three iterations to get right is that `EPERM` and
`EACCES` are *different answers to different questions*:

- **`EPERM` is ownership.** The target is not the caller's, in either direction - and a lowering of a
  stranger's nice is refused just as a raising is. The first readings here mislabelled this: `EACCES` is `13`
  and `EPERM` is `1`, and an early probe printed `-1` with `errno 13` and called it "EPERM", which made the
  table look like a direction rule and nothing else.
- **`EACCES` is direction.** The target *is* the caller's, but the request raises its priority: Linux reaches
  this through `capable(CAP_SYS_NICE)` or the caller's `RLIMIT_NICE`. This kernel has no `RLIMIT_NICE` entry
  (`default_rlimits()` fills five of `RLIM_NLIMITS`' sixteen), so the capability is the whole of the rule
  here - and a caller without it may still lower any task it owns, which Linux never refuses. That is why the
  predicate is `prio < current` and not `prio != current`: an equal value is not a raise, and Linux answers `0`
  for it on a task the caller owns.

**Blood relation is not the rule**, which the name "may retarget" invites. A scan over every process this host
would show (`w.c`, `x2.c`, `y2.c`: 484 targets with an empty `CapPrm`) answered `0` for
`setpriority(PRIO_PROCESS, <pid>, <that process's own nice>)` on **every one of them** - parent, sibling,
stranger, child. The only refusals were the targets holding a capability (`pid 1330`, `CapPrm` = a wake-alarm
bit) or a different uid, which is the two branches of `set_one_prio`, not a family tree.

**`PRIO_USER` is stricter than `PRIO_PROCESS`, and that is upstream's own asymmetry**: the same uid and the
same equal value answer `0` through `PRIO_PROCESS` and `EPERM` through `PRIO_USER` (`g2.c`). So the two
collection arms get the ownership question per member, and any member the caller may not touch refuses the
**whole set** - nothing is written. Linux walks the group and stops at the first task `set_one_prio` refuses,
leaving the earlier members changed; that partial write's extent is the task list's iteration order, which is
not a promise to a caller, so refusing the set whole is the same answer with nothing half-done.

**Ordering.** Empty first (`ESRCH` beats `EPERM`: `set(USER, 1, 10)` = `ESRCH` on this host, not `EPERM`),
then ownership (`EPERM`), then direction (`EACCES`), then the write. The number has to resolve before any of it
- an unknown pid is `ESRCH` in every arm, never `EPERM`.

The fix is two predicates in `src/syscall/linux/posix.rs`: `nice_may_retarget` (the caller's own task, else
`crate::cred::kill_ok` on the four ids, else `CAP_SYS_NICE` - the same shape `affinity_may_retarget` uses, so
"same user" means the same thing across the syscall surface) and `nice_may_raise` (the capability, then
`prio >= current`). A task that has already exited passes both and `set_nice` answers `ESRCH`, which is the
answer the caller can act on.

**The pin, and how it can exist.** `user/tests/src/bin/t-nice-authority.rs` drops a child to `nobody` and has
it lower and raise its own nice and then lower its root parent's - the fixture `t-affinity-rules` established
(`setgroups(0, null)`, `setgid`, `setuid(NOBODY)`; the payload runs as root so the drop succeeds in a guest and
fails on the host, where the same-uid branch is then the allowed side of the same rule). Four claims, on the
unfixed kernel **two red**:

```
ok:    lowering a task's own nice is served (uid 65534)
FAIL:  and raising it again is refused (uid 65534, errno Some(2))
FAIL:  a program of another user cannot lower its root parent's nice (uid 65534)
ok:    and a getter asks for no authority at all (uid 65534, read … raise=0 raiseerrno=2 parent=0 parenterrno=2 …)
```

and with the fix all four `ok:`, `parent=-1 parenterrno=1` (the `EPERM`) and `raise=-1 raiseerrno=13` (the
`EACCES`). `t-priority`'s twenty existing claims stay green: they run as root, which holds the capability, so
every value they set is a raise it is allowed to make.

**The rule.** A call that changes another task asks two questions and they have **two errnos**: *is this task
mine* (`EPERM`) and *may I move it this way* (`EACCES`). A handler that answers a lowering with success for a
task it does not own has not checked anything - and a check written from a comment ("moot for this kernel")
instead of a measurement will be wrong in whichever direction the comment guessed.

## §6.94 `RLIMIT_NICE` and `RLIMIT_RTPRIO` are zero on Linux; infinity is a promise (LANDED 2026-11-07)

The row was found by following §6.93 to the sentence it had inherited: `nice_may_raise`'s doc comment said
"this kernel does not carry `RLIMIT_NICE`". A `getrlimit` walk over all sixteen rows shows that only half true -
the row exists, and it reads `infinity`.

```
Linux (host, 7.2.14, uid 1000)                 this kernel (guest, payload as root, slot 5)
  RLIMIT_NICE       cur=0        max=0           RLIMIT_NICE       cur=18446744073709551615 max=18446744073709551615
  RLIMIT_RTPRIO     cur=0        max=0           RLIMIT_RTPRIO     cur=18446744073709551615 max=18446744073709551615
setrlimit(NICE,{40,40})  rc=-1 errno=1 (EPERM) setrlimit(NICE,{40,40})  rc=0
setrlimit(NICE,{0,0})    rc=0                  setrlimit(NICE,{0,0})    rc=0
setrlimit(NICE,{20,20})  rc=-1 errno=1 (EPERM) setrlimit(NICE,{20,20})  rc=0, and the row is *written* to 20
NICE finally             cur=0 max=0           NICE finally             cur=20 max=20
```

Both rows sit past the ones `default_rlimits()` fills, so they were left at the table's `RLIM_INFINITY`
starting value (`crates/abi/src/lib.rs:280`). The `setrlimit` half differs for a second reason: `rlimit_admit`
asks `is_root`, and the payload runs as root, so nothing is refused. That is Linux's answer for root too
(`CAP_SYS_RESOURCE`), so the acceptance is not a divergence in itself - the **row's value** is.

**The zero is the rule, not a leftover.** `can_nice` is the kernel's check behind a priority-raise: a caller
with `CAP_SYS_NICE` may raise, and one without may raise only as far as `RLIMIT_NICE` allows (`rlim_cur >=
20 - nice`). Measured on the host through the only lever an unprivileged caller has: `0/0` refuses **every**
raise (`setpriority(self, 0)` from a stored nice of 19 is `EACCES`, and so is `setpriority(self, 10)`, because
`10 < 19` is still a raise), while an equal value is served (`setpriority(self, 19)` = `0`; the equal case
never asks `can_nice`, which is why it passes). A task that reads `RLIMIT_NICE = infinity` and then acts on it
is told it may raise its priority freely, and that is a promise this kernel does not keep - §3's rule 2, the
failure that matters most here, because the caller has no fallback to take after being trusted.

`RLIMIT_RTPRIO`'s zero is the same shape: it is the row that says "no realtime priority above zero without the
capability". This kernel grants `SCHED_FIFO` without consulting it (`sched_setscheduler` asks its own rule),
so the row was describing a budget nobody spends - but a caller reads it, and Linux's answer is `0`.

**The fix** is two lines in `default_rlimits()`, both at `0/0`, plus the doc comment that argued they were
absent. `t-prlimit64` gained the pin, two claims for the two rows, written the same raw-`prlimit64` way as its
neighbours so the same program reads both machines:

```
unfixed kernel                                            fixed kernel
FAIL:  RLIMIT_NICE reads 0/0, the row Linux answers       ok:    RLIMIT_NICE reads 0/0, the row Linux answers
FAIL:  RLIMIT_RTPRIO reads 0/0, the row Linux answers     ok:    RLIMIT_RTPRIO reads 0/0, the row Linux answers
FAILED  (=== t-prlimit64 exit=1)                          PASS    (=== t-prlimit64 exit=0)
```

(`build/boot-rlim-cf1.log` and `build/boot-rlim-fix1.log`: the first built from a stash of the two lines with
**both** the kernel and the loader rebuilt, since the loader carries the kernel as a deflate payload.)

**What is not here, and why it is a named gap by shape.** The row's *reading* half - calling `can_nice` inside
`setpriority`, which is what the zero is for - is not implemented. `nice_may_raise` still asks for
`CAP_SYS_NICE` and nothing else. That is not a stub standing in for the rule: on this machine the row can only
be lifted by root (the `is_root` clause), and root holds the capability anyway, so every pair the row would
admit is already admitted, and a line implementing it would be one no fixture could tell apart from its
absence - rule 4's counterfactual would fail. It becomes real work the moment `rlimit_admit` learns
`CAP_SYS_RESOURCE` (a distinct, larger question about who may lift a *hard* limit), and it is named in
`docs/ROADMAP.md` as such.

**The rule.** A table's infinity is a default, and a default is a promise: a row Linux answers `0` for and this
kernel answers `infinity` for tells the caller the opposite of what the kernel will do. The rows that matter are
the ones whose zero *is* the rule (`RLIMIT_NICE`, `RLIMIT_RTPRIO`), and "we do not carry that row" is a claim
to check with `getrlimit` before it is written into a comment - this one read `infinity` on the machine that
was said not to carry it.

## §6.95 `RUSAGE_THREAD` is the thread's CPU, and this kernel refused it as a bad argument (LANDED 2026-11-07)

`getrusage(2)`'s `who` has three values: `RUSAGE_SELF` (0), `RUSAGE_THREAD` (1) and `RUSAGE_CHILDREN`
(-1). This kernel served two of them and answered `EINVAL` for the third - a *valid* constant refused
the way a bad argument is, which is the one shape `docs/ARCHITECTURE.md` §3's rule 2 cannot excuse:
a caller that asks for its thread's CPU is told its request was malformed, and has no fallback to take.
The refusal was not a decision recorded anywhere; it was the `if who != SELF && who != CHILDREN`
guard, and the two-valued world behind it.

**What Linux answers.** Measured on host 7.2.14 (`build/scratch/batch/b.c`, musl-gcc -static -O1
-pthread) - a process with one spinner thread, `ru_utime` in seconds:

```
                          RUSAGE_SELF     RUSAGE_THREAD
before any spin              0.033            0.031
after the spinner ran        0.046            0.031     <- the spinner's CPU is the group's, not main's
after main also spun         0.049            0.034
who = 2                     -1 EINVAL
```

So the two doors report different numbers the moment a process has a second thread, and `who == 1`
answers the *calling task's* own CPU, with no child term. (Linux's own `getrusage(RUSAGE_THREAD)` in
glibc is served from `current->utime/stime`, which is why the two rows above diverge exactly by what
the spinner burned.)

**What this kernel answered.** A probe calling all three (`build/scratch/batch/a.c`, run through the
one-test boot, `build/probe/oneshot/serial.log`):

```
getrusage(RUSAGE_SELF, &ru)      = 0
getrusage(RUSAGE_CHILDREN, &ru)  = 0
getrusage(RUSAGE_THREAD, &ru)    = -1 errno=22 (Invalid argument)
getrusage(99, &ru)               = -1 errno=22
```

The last line is Linux's answer and is right; the third is the same answer for a `who` Linux serves.

**Why it was not servable in one line.** The kernel had no per-thread CPU counter. `Mm`'s
`cpu_utime`/`cpu_stime` are the *group's* - shared by every thread, because `RUSAGE_SELF` and
`times()` are the process's - and `current_cpu_ticks`'s doc comment said so: "there is no per-thread
split because there are no threads here". That sentence was inherited and false: this kernel has had
threads since `clone(CLONE_THREAD)` and `t-exec-threads` runs them on both machines. Serving
`RUSAGE_THREAD` from the group's counters would have been the *approximating* fix - it passes a
"the call succeeds" claim and hands back a body that is the wrong process's (`docs/notes/` §3's rule 4,
refuse rather than approximate).

**The fix.** Two `AtomicU64`s on the live `Task` (`cpu_user`, `cpu_system`, `src/sched/runtime/task.rs`
- the pair a *thread* owns), charged in the same `charge_cpu` call that adds the tick to the group, so
the two doors cannot disagree about how many ticks there were; `current_thread_cpu_ticks()` in
`src/sched/runtime/child_signal.rs` reads them; `handle_getrusage` accepts `who == 1` and answers from
that accessor. The charge happens in the timer interrupt, so both additions are atomics - allocation-free
and bounded, as rule 5 requires of interrupt context - and a task that never runs costs nothing.

**The pin** is four claims in `t-times`, and its counterfactual is a real red, because the shape it
pins is "the call is served" rather than "the call is nearly served":

```
unfixed kernel                                            fixed kernel
FAIL:  getrusage(RUSAGE_THREAD) succeeds (got Err(...         ok:    getrusage(RUSAGE_THREAD) succeeds
         code: 22, kind: InvalidInput ...))                   ok:    a spinner thread's CPU lands in RUSAGE_SELF (grew 24000 us)
ok:    a spinner thread's CPU lands in RUSAGE_SELF            ok:    and is not charged to this thread's own RUSAGE_THREAD
FAIL:  and is not charged to this thread's own RUSAGE_THREAD         (grew 4000 us, the group 24000 us)
         (grew -6148914691235850538 us ...)                  ok:    this thread's own record grows when this thread burns CPU
FAILED  (=== t-times exit=1)                                  PASS    (=== t-times exit=0)
```

(`build/boot-thread-cpu-cf1.log` - built from a stash of the six source files with **both** the kernel
and the loader rebuilt, since the loader carries the kernel as a deflate payload - and
`build/boot-thread-cpu3.log`.) The third claim is the one that could be satisfied by a kernel that
always answers zero, so the fourth exists: the same record must move when *this* thread burns. On the
host both machines' runs agree (`MPOS_ON_HOST=1`: `grew 70 us, the group 20867 us`).

The second claim's bound is the spinner's own target (20 ms) rather than a ratio: an idle thread can
still pick up a tick or two of accounting slop on a uniprocessor (the guest's tick is 4 ms, and one
run showed 4000 us), and a claim that reddened on that would be pinning the scheduler's accounting
slop rather than who owns the record. A kernel answering with the group's totals reports *at least*
the group's growth there, so the bound still catches the approximating shape.

**The rule.** A `who` that Linux serves is not "an unsupported option": `EINVAL` is a statement that
the argument was malformed. When the honest answer needs a counter that does not exist yet, the choice
is between adding the counter and naming the gap - refusing a valid constant is neither, and it is
especially cheap to spot when the refusal sits beside two siblings that resolve it correctly (the
`_ => EINVAL` arm here is the same disagreement §6.91 found in `prlimit64`'s cross-process half).

## §6.96 `/proc/<pid>/stat`'s CPU fields were zero while the kernel counted them (LANDED 2026-11-07)

The `stat` file's fields 14-17 are `utime`, `stime`, `cutime`, `cstime`, and this kernel printed `0`
in all four. The comment beside them said so on purpose - "fields this kernel does not account - page
faults, CPU time, wchan - are zero, which is what a machine that never counted them would honestly
report" - but by the time this was measured, `wchan` had already stopped being zero (field 36 reads
`park_site`) and CPU had been accounted since `times()` landed. So the sentence was true when it was
written and had been overtaken twice; the zero was the one part of it that nothing revisited.

**The Linux reading** (`build/scratch/batch/s8.c`, musl static, a spinner thread plus the main
thread, Linux 7.2.14, `sysconf(_SC_CLK_TCK) = 100`):

```
t=0  /proc/self/stat utime=  3 | RUSAGE_SELF 0.032 | RUSAGE_THREAD 0.030
t=1  /proc/self/stat utime= 66 | RUSAGE_SELF 0.667 | RUSAGE_THREAD 0.030
```

Field 14 followed `RUSAGE_SELF` (the whole group, `66` ticks = 0.667 s) and **not** the reading
thread's own CPU (0.030 s). That is Linux's `whole` flag: set for `/proc/<pid>/stat` and for a
thread's own `/proc/<tid>/stat`, clear only for `/proc/<pid>/task/<tid>/stat`, which carries *that
thread's* numbers. The unit is `USER_HZ` ticks (100 is ABI), not the kernel's timer ticks, and the
fields count completed ticks, so the scaling truncates.

The host this measurement ran on hides thread directories (`/proc/<pid>/task` lists only the leader
and `/proc/<pid>/task/<tid>/stat` does not open), so the per-thread half of the rule could not be
measured here - it is Linux's source semantics, and the claim that pins it runs on the machine only.

**The fix.** `TaskSnapshot` (and its host mirror) carries `cpu: (utime, stime, cutime, cstime)` from
`task.mm.cpu_all()` - the group's four numbers, the same ones `getrusage(RUSAGE_SELF)` and `times()`
answer - and `cpu_own: (utime, stime)` from the per-thread atomics §6.95 added. `stat_line` takes
Linux's `whole` flag from its caller (`process_dir` builds `/proc/<pid>` and `/proc/<tid>`, both
`whole`; `thread_dir` builds `/proc/<pid>/task/<tid>`, not), `cpu_fields` picks the pair, and
`user_hz_ticks` scales `ticks * 100 / TICK_HZ` the way `handle_times` and `write_rusage` already do.
`cutime`/`cstime` are the group's either way: Linux keeps the child totals in the group's
`signal_struct`, not on the task.

**The pin** is three claims in `t-times`, inside the section that already measures CPU: field 14
against `RUSAGE_SELF`, field 16 against the reaped child's folded CPU, and (on the machine only) the
`task/<tid>` spelling against the group's. Its counterfactual is a real red - the stash removed
`src/vfs/procfs.rs` and `src/sched/runtime/procfs.rs` while keeping the test, and both the kernel and
the loader were rebuilt:

```
unfixed kernel (`build/boot-procstat-cf.log`)             fixed kernel (`build/boot-procstat2.log`)
FAIL:  /proc/self/stat's utime is the group CPU            ok:    /proc/self/stat's utime is the group
         getrusage reports (0 us against 40000 us)                 CPU getrusage reports (50000 us against 40000 us)
FAIL:  and its cutime is the waited child's CPU            ok:    and the thread spelling carries this
         (0 us against 12000 us)                                   thread's own (3 ticks against the group's 5)
FAIL:  and the thread spelling carries this thread's
         own (0 ticks against the group's 0)
FAILED  (=== t-times exit=1)                               PASS    (=== t-times exit=0)
```

The tolerance is two `USER_HZ` ticks in each direction: the record is read before the comparison and
the field truncates, so the two cannot agree to the microsecond by construction, while a kernel
printing raw timer ticks would be off by a factor of 2.5 (`TICK_HZ` is 250).

**The rule.** A field a reader parses is a promise like any other, and "we do not count it" is only
honest while it is true. The tell here was a comment that named three fields as unaccounted and had
been overtaken twice without anyone re-reading it - so the check is not "does the comment explain the
zero" but "does the kernel still not count it". And the fields a reader cross-checks against
`getrusage` are the cheap place to notice: `ps` and `top` show `0:00` for a process that has been
busy, which is the silent-success shape §3 rule 2 forbids in a syscall and forbids just as much in a
file.

## §6.97 `clock_getres` told every caller the tick, while the clock stepped in nanoseconds (LANDED 2026-11-07)

A clock's advertised resolution is not decoration: `epoll` loops, `timerfd` arming, a compositor's
frame pacing and a program's own back-off all read it to decide how coarse their arithmetic has to
be. Two things were wrong with this kernel's answer, and they were two halves of one subject - what
the clock family says about itself versus what it delivers.

**What the host answers.** `build/scratch/batch/g.c` (musl static, Linux 7.2.14) walks all the ids a
real kernel keeps:

```
REALTIME          rc=0 res=0.000000001     REALTIME_COARSE   rc=0 res=0.001000000
MONOTONIC         rc=0 res=0.000000001     MONOTONIC_COARSE  rc=0 res=0.001000000
PROCESS_CPUTIME   rc=0 res=0.000000001     BOOTTIME          rc=0 res=0.000000001
THREAD_CPUTIME    rc=0 res=0.000000001     TAI               rc=0 res=0.000000001
MONOTONIC_RAW     rc=0 res=0.000000001     BOOTTIME_ALARM    rc=0 res=0.000000001
bad id 12345      rc=-1 errno=22 (EINVAL)
```

and the readings match those claims (`build/scratch/batch/k.c`): in 5000 back-to-back reads
`CLOCK_MONOTONIC` moved 5000 times with a smallest step of **20 ns**, `CLOCK_MONOTONIC_COARSE` moved
**0** times, and `CLOCK_REALTIME_COARSE` once, by a millisecond. The coarse clocks are the tick's own
reading, and the fine ones are the counter's.

**What this kernel answered.** One tick - 4 ms - for *every* id, under a comment saying "the tick
counter is the only source of time there is, so the honest resolution is one tick". That sentence was
true when it was written and had been overtaken by the very function it documents: `clock_gettime`
interpolates the free-running counter inside the tick (`since_boot_nanos`), and the measured steps are
sub-microsecond - `MONOTONIC` **616 ns**, and, in the same run, `MONOTONIC_COARSE` **352 ns**. So the
resolution was four million times coarser than the readings, and the coarse clock was not coarse at
all: a caller that picked `_COARSE` for its cheapness paid the full read and was handed a fine value
under a coarse label.

Two more facts came out of the same walk:

- The id set was **three shorter than `clock_gettime`'s**: `{0..=9, 11}` at one door, eight ids at the
  other. So `clock_getres(CLOCK_TAI, …)` - a clock this kernel serves - came back `EINVAL`, the answer
  for a malformed argument. One argument, two doors, two verdicts: the same shape §6.95 recorded for
  `RUSAGE_THREAD`.
- `CLOCK_BOOTTIME_ALARM` (9) was answered as **wall** time while `CLOCK_BOOTTIME` (7) is boot-relative.
  Measured on the host (`build/scratch/batch/m.c`): `BOOTTIME_ALARM` is `752 ns` from `BOOTTIME`, and
  `REALTIME_ALARM` `1313 ns` from `REALTIME` - each alarm clock reads like the clock it alarms from,
  and the two bases are ~1.8e18 ns apart, so this was not a rounding difference.

**The fix.** `Clock::resolution_nanos` (default: the tick, so a machine whose only source is the tick
says so) is overridden by both machines:

- x86_64 `timer::resolution_nanos`: the reading is `ticks * tick_ns + frac * tick_ns / cpt`, so one
  step is `tick_ns / cpt` - sub-nanosecond with a gigahertz TSC and a 4 ms tick - and the ABI's unit
  rounds it up to `1 ns`, which is what Linux answers for its fine clocks. Before the second tick has
  calibrated `cpt` there is no fraction, and it answers the tick.
- aarch64 `timer::resolution_nanos`: the reading is `elapsed * 10^9 / freq`, so one step is
  `ceil(10^9 / CNTFRQ)` - the machine's own quantum (16 ns at QEMU's 62.5 MHz), answered from the
  register the machine states rather than a constant.

`clock_getres` then answers `hal::clock_resolution_nanos()` for the fine clocks and `hal::ns_per_tick()`
for the two coarse ones, over the *same* id set `clock_gettime` accepts; and `clock_gettime` makes the
coarse pair genuinely coarse - `ticks() * ns_per_tick()`, not the interpolated counter - so the value
and the resolution now agree. `CLOCK_BOOTTIME_ALARM` joined the boot-relative family (value and its
time-namespace offset, the latter by the family rule rather than a separate measurement: this host
cannot create a time namespace unprivileged, so the offset half is source-derived and the value half is
measured).

**Pin.** `user/tests/src/bin/t-clock-getres.rs` (31 claims). Every id gets a resolution inside one
second; `CLOCK_MONOTONIC` resolves *finer than a tick*; both coarse clocks resolve at exactly the
4 ms tick and every coarse reading across 2000 reads is a whole multiple of it (and the coarse clock
moves no more often than the ticks the loop itself crossed); each alarm clock reads within a millisecond
of its base; `CLOCK_TAI` is served and
reads as the wall clock (no leap table on this machine, and the host had none either - 1162 ns apart);
plus the vocabulary refusals (`10` and `999` are `EINVAL`, a null `res` is allowed). The three
coarse-resolution claims and the whole-tick claim are guarded with `host_run()`: they are statements
about *this* kernel's tick, and a host answers its own (1 ms, and a sub-tick offset in the value).

**Counterfactual** (`build/boot-clkres-cf1.log` against `build/boot-clkres-fix2.log`):

```
unfixed kernel                                    fixed kernel
FAIL: clock_getres(CLOCK_REALTIME_ALARM) answers  ok:   (1 ns)
FAIL: clock_getres(CLOCK_BOOTTIME_ALARM) answers  ok:   (1 ns)
FAIL: CLOCK_MONOTONIC resolves finer than a tick  ok:   (1 ns)
      (4000000 ns)
FAIL: a coarse reading is a whole number of its   ok:   (0 fractions in 2000)
      ticks (2000 fractions in 2000)
FAIL: and the coarse clock barely moves across    ok:   (0 steps)
      2000 reads (1999 steps)
FAIL: CLOCK_BOOTTIME_ALARM reads with             ok:   (14900 ns apart)
      CLOCK_BOOTTIME (1793993879000015781 ns apart)
```

**Two of those claims were written as numbers only a fast host reaches, and the second machine said so.**
The first gate that ran the pin asked both arms for it and the aarch64 arm - an emulated machine, on the
one core this container is pinned to - came back `=== FAIL t-clock-getres exit=1` with two reds that were
statements about its speed:

```
FAIL: and the coarse clock barely moves across 2000 reads (5 steps)
FAIL: and reads as the wall clock, this machine having no leap table (1603792 ns apart)
```

The first bound `moved <= 2` was a step *count*, chosen from a host where 2000 coarse reads finish inside
one tick; on aarch64 2000 syscalls span several ticks, so the coarse clock legitimately moved five times.
The second bound was a fixed 1 ms on the skew between a `CLOCK_TAI` read and a `CLOCK_REALTIME` read taken
one after the other - and three syscalls on an emulated aarch64 are themselves milliseconds of wall time,
so the skew measured the gap between the reads, not anything about the clocks. Both are the same mistake
as a test that reads a clock it invented: a claim about the clock wearing a claim about the host's speed.
Each is now anchored to the interval the claim's own measurement spans - the coarse bound is the
**number of tick boundaries the loop crossed** (`(last - first) / quantum`, plus one), and the wall-clock
skew is bracketed by reading `CLOCK_REALTIME` again afterwards and allowing the bracket
(`skew <= spanned + 1 ms`). Fixed-kernel readings: x86_64 `1 steps over 1 ticks`,
aarch64 `3 steps over 3 ticks` and `15968 ns apart over a 24368 ns read`. Counterfactually the same:
the amended coarse claim on the unfixed kernel reads
`FAIL: and the coarse clock moves only as often as its ticks do (1938 steps over 1 ticks)`
(`build/boot-clkres-cf2.log`), because a fine-grained reading under a coarse label moves once per read
while the loop crossed one tick.

**The rule.** A resolution is a promise about the readings, so the two must be computed from the same
source: if `clock_gettime` divides a counter, `clock_getres` cannot answer the tick, and if a clock is
named `_COARSE` its reading has to *be* the coarse one. And when two doors take the same argument,
they take the same set of values - the door that refuses a value its sibling serves is the defect,
whichever of the two is "right" about the number.

## §6.98 `/proc/loadavg` printed zeros, and `sysinfo.loads` printed zeros beside it (LANDED 2026-11-07)

**What Linux answers.** A load average is not the runnable count; it is a decayed integral of it, in
fixed point, resampled once every five seconds. `build/scratch/loadavg/p.c` (musl static, 16 CPUs,
Linux 7.2.14) read both doors on an idle host, then with four spinner threads:

```
idle          /proc/loadavg  1.85 2.04 2.21 3/1312 500203
              sysinfo loads  127456 134624 145376      (127456 / 65536 = 1.9449)
+4 spinners   /proc/loadavg  2.27 2.12 2.24 7/1316 ...
              sysinfo loads  148768 135168 145728
```

Forty samples at 100 ms intervals showed the three averages **changing only on a 5 s boundary** while
the fourth field (`runnable/total`) changed on every read:

```
1.85 2.04 2.21   -> 1.94 2.05 2.22   (0.7 s in)
1.94 2.05 2.22   -> 2.27 2.12 2.24   (5.0 s in, after the spinners started)
```

So: `/proc/loadavg`'s text is `avenrun` rounded to hundredths; `sysinfo.loads` is the *same*
`avenrun` shifted into its own fixed point (`<< (16 - 11)`); the fourth field's numerator is
`nr_running`, its denominator `nr_threads`; and both doors move together, once per five seconds.
The arithmetic is `kernel/sched/loadavg.c`: `FSHIFT 11`, `FIXED_1 2048`, `EXP_1 1884`,
`EXP_5 2014`, `EXP_15 2037`, and

```
calc_load(load, exp, active) = (load * exp + active * (FIXED_1 - exp) + (active >= load ? FIXED_1-1 : 0)) / FIXED_1
```

with `active = (nr_running + nr_uninterruptible) * FIXED_1` - the activity term is a fixed-point
number too, and the rounding addend is what keeps a steady load at its own fixed point.

**What this kernel answered.** `/proc/loadavg` was a literal: `format!("0.00 0.00 0.00 1/{} {}\n",
process_count(), newest)` - the string is the shape of a load average and none of its content - and
`handle_sysinfo` zeroed its 112-byte buffer and filled `uptime`, `totalram`, `freeram`, `procs` and
`mem_unit`, leaving `loads[0..3]` at zero. `docs/uapi/03-flag-vocabularies.md:50` already listed
`loadavg` as served "as the neutral renderings", so this was not a named gap but a shape that looks
finished while doing less (AGENTS.md §3 rule 5) - and unlike most divergences here, a caller cannot
tell, because zero is a legal load.

**The fix.** A new host-compiled `src/sched/loadavg.rs` carries Linux's constants, `calc_load`, and
`Averages { one, five, fifteen: AtomicU64 }` behind the kernel's one `AVENRUN`. The clock is taken
where this kernel already charges CPU (`charge_cpu`, the tick hook): `sample_tick(TICK_HZ)` counts
ticks modulo `5 * TICK_HZ` and, on the boundary, `active` is read from the scheduler under the table
lock that the hook already holds - `ready_count()` (the ready bands), `uninterruptible_count()` (the
sleepers this kernel parks as blocked-transfers, now carrying a flag) and the running task. Three
loads are updated in place; nothing allocates, and nothing walks a structure an interrupt can have
left half-changed. `/proc/loadavg` and `handle_sysinfo` read the same array and apply the same two
formats Linux does (`hundredths` adds `FIXED_1/200` before splitting; `sysinfo_load` shifts).

The *fourth* field also had its denominator wrong: it counted processes (`process_count()`), and
Linux's is threads. It is `task_count()` now.

**Pin.** `user/tests/src/bin/t-loadavg.rs`, 5 claims: the two doors agree; the fourth field counts
the ready threads while three spinners run; one sample lifts the one-minute average off zero; and
`sysinfo.loads[0]` carries the same lift; and the averages hold steady between samples (1 s of 50 ms
reads sees at most two distinct triples). On the fixed kernel
(`build/boot-loadavg3.log`): `ok: the runnable field counts the threads that are ready (6/10)`,
`ok: one sample lifts the one-minute average off zero (32 hundredths)`,
`ok: and sysinfo carries the same lift (loads[0] = 20992, which is 32 hundredths)`,
`ok: the averages hold steady between samples (1 distinct triples in 20 reads)`, `PASS`.

Counterfactually (`build/boot-loadavg-cf1.log`, kernel with `src/sched/` and both readers reverted):
`FAIL: the runnable field counts the threads that are ready (1/2)`,
`FAIL: one sample lifts the one-minute average off zero (0 hundredths)`,
`FAIL: and sysinfo carries the same lift (loads[0] = 0, which is 0 hundredths)`,
`=== t-loadavg exit=1` - while the "holds steady" claim stayed green, which is exactly what a
constant zero is. Host tests pin the arithmetic without a machine:
`the_first_sample_of_one_runnable_task_is_one_tasks_worth` (`calc_load(0, EXP_1, FIXED_1) == 164`,
which the file prints as `0.08`), `a_steady_load_is_its_own_fixed_point`,
`an_idle_machine_decays_by_the_one_minute_constant`, `the_two_doors_report_the_same_fixed_point_value`
(`3983 -> "1.94"` and `127456`), and `a_sample_lands_once_every_five_seconds_of_ticks`.

**The rule.** A counter that a reader parses is served only if the number answers the question the
reader is asking: "the machine is idle" and "the machine has this much load" are different answers,
and a zero that never moves is not the second one. And when one Linux array feeds two doors, both
doors read the array - a `sysinfo` that fills its own copy of a number the file prints from is the
divergence waiting to happen.

## §6.99 `CLOCK_PROCESS_CPUTIME_ID` was machine uptime wearing a CPU clock's name (LANDED 2026-11-07)

**What Linux answers.** The two cputime clocks are not clocks that run with the machine: they report
CPU time, so they stand still while the task sleeps and cover only what the machine charged to it.
`build/scratch/cpuclock/p.c` (musl static pthread, Linux 7.2.14) on a process 46.4 ms old:

```
process before: cpu=46409 us   rusage_self=46410 us
sleep 300 ms:   cpu grew 9 us              (rusage self 46420 us)
burn 400 ms:    cpu=445005 us  rusage_self=445006 us
granularity:    19988 of 20000 back-to-back reads moved, min step 9 ns
worker thread:  thread_cpu grew 298617 us for a 298 ms burn
                main thread_cpu=450391 us, process_cpu=753138 us   (450 + 298)
clock_getres(PROCESS_CPUTIME_ID / THREAD_CPUTIME_ID) = 1 ns
```

So: the process clock equals `RUSAGE_SELF`'s user+system (children excluded, as `RUSAGE_SELF` excludes
them); the thread clock is that thread's own share; sleeping does not touch either; and the resolution
is `1 ns` because Linux charges CPU in nanoseconds.

**What this kernel answered.** Both ids were in the monotonic list, so `handle_clock_gettime` filled
them from `hal::since_boot_nanos()` - **machine uptime** - and `clock_getres` answered the counter's
`1 ns` beside it. The same probe on the machine before the fix:

```
uptime by clock: MONOTONIC=1110 ms
process before:  cpu=1113219 us   rusage_self=4000 us       <- 1113 ms "CPU" for a process 1.1 s old
sleep 300 ms:    cpu grew 301038 us                          <- CPU time advanced by the sleep
worker thread:   thread_cpu grew 300140 us                   <- the sibling's burn
main thread:     thread_cpu=2142078 us process_cpu=2142070 us <- identical: both are uptime
granularity:     19998 of 20000 reads moved, min step 683 ns
clock_getres(PROCESS_CPUTIME_ID) = 1 ns
```

A profile that asks "how much CPU has this process used" was told the age of the machine, and a
sleeping process accrued CPU time while it did nothing. Nothing fails visibly, which is why this
survived: a `timespec` came back and its fields looked like time.

**The fix.** The two ids leave the `since_boot_nanos` path and read the counters `getrusage` and
`/proc/<pid>/stat` already answer from - `sched::current_cpu_ticks()`'s `utime + stime` for the process
(`cutime` stays out, the way `RUSAGE_SELF` leaves children out) and
`sched::current_thread_cpu_ticks()` for the thread - times `ns_per_tick()`. Both are tick-granular, so
`handle_clock_getres` answers `ns_per_tick()` for these two ids as it already did for the coarse pair:
a resolution is a promise about the readings (`§6.97`), and `1 ns` beside a counter that steps in 4 ms
is that defect written the other way round. The time namespace still does not touch them, which the
offset table already said.

**Pin.** `user/tests/src/bin/t-cpuclock.rs`, 5 claims: the process clock agrees with `RUSAGE_SELF`
within two ticks; it does not advance across a 300 ms sleep; it does advance across a 300 ms burn; a
sibling's burn lands in the process's clock and not in this thread's; and `clock_getres` states the
granularity the counter moves in. Fixed kernel (`build/boot-cpuclock-fix2.log`):
`ok: CLOCK_PROCESS_CPUTIME_ID and RUSAGE_SELF agree about this process's CPU (8000 us against 8000 us,
0 us apart)`, `ok: and it does not run while the process sleeps (grew 0 us over a 300 ms sleep)`,
`ok: and it moves when this thread burns CPU (300000 us for a 300 ms burn)`,
`ok: a sibling's CPU is the process's, not this thread's (this thread grew 4000 us while the process
grew 204000 us …)`, `ok: clock_getres states the granularity of the CPU counters (4000000 ns)`,
`PASS`. On the host the same five read `1 us apart`, `10 us`, `297748 us`, `65 us against 186754 us`,
`1 ns` - the program is `PASS` on both machines.

Counterfactually (`build/boot-cpuclock-cf1.log`, `src/syscall/linux/proc.rs` reverted):
`FAIL: CLOCK_PROCESS_CPUTIME_ID and RUSAGE_SELF agree about this process's CPU (310002 us against
8000 us, 302002 us apart)`, `FAIL: and it does not run while the process sleeps (grew 300276 us over a
300 ms sleep)`, `FAIL: a sibling's CPU is the process's, not this thread's (this thread grew 205931 us
while the process grew 205940 us …)`, `FAIL: clock_getres states the granularity of the CPU counters
(1 ns)`, `=== t-cpuclock exit=1`. The one claim that passes without the fix is "it moves when this
thread burns CPU" - any running counter satisfies it, which is exactly why the other four are the pin.

**The gate found a fixture that had been written against the old answer.** The first two-arm run of
this change was green on 265 of 266 programs on x86_64 and 264 of 265 on aarch64, both red on the same
claim: `t-ns-time`'s `FAIL:  CLOCK_PROCESS_CPUTIME_ID is unmoved by 0 s in the child and unmoved here`
(`build/gate-cpuclock.log`, `build/test-lane-x86_64-1/serial.log`, `build/test-lane-aarch64-0/`). That
claim brackets a child's reading between the parent's two readings of **the same scale** -
`child - after <= offset <= child - before` - which is exact for the four uptime readings and was
never exact for this one: a CPU clock counts the work of the process that reads it, and Linux
**starts a forked child's at zero**. Measured for this repair (`build/scratch/forkcpu/f.c`, musl
static, Linux 7.2.14):

```
parent before burn:   242041816 ns      (242 ms of CPU before the fork)
parent after:         242143085 ns      (+101 us: the child's work is the child's)
child reported:         12794 ns  -> 47711431 ns   (at entry, then after a 50 ms burn)
```

So the old bracket passed only while the "CPU" clock was the machine's uptime - both processes read
one rising number. The repair gives the child an eighth number in its report, `getrusage`'s
user+system, and judges this one row by the sibling call instead
(`CLOCK_PROCESS_CPUTIME_ID still reads the child's own CPU inside the namespace (0 us against
RUSAGE_SELF's 0 us, 0 us apart)`, `build/boot-nstime-fix1.log`) - a namespace offset of 42 s, or the
machine's age, would read as CPU nobody spent. The other six rows keep the exact bracket, which is
still what proves the namespace moved them and not the caller.

**The rule.** "Clock" is not one promise: an id whose name says *CPU time* must be CPU time, and the
two ids Linux derives from the CPU counters must be derived from them here - agreeing with the sibling
call that reports the same number (`getrusage`) is the check that catches this class. And when the
reading moves in tick steps, the advertised resolution has to be the tick.

## §6.100 `/proc/stat` printed the uptime as CPU time, and three of its lines did not exist (LANDED 2026-11-07)

**What Linux answers.** `/proc/stat` is the first file a monitor reads, and every field in it is a
*counter*: two readings subtracted are a rate, one reading is "since boot". Measured on the host
(`build/scratch/stat/s.c`, musl static pthread, Linux 7.2.14):

```
cpu  2372652 1659219 418642 84996969 90527 127699 64261 0 10254 12726   (ten columns)
cpu0 ... (one line per CPU, same shape)
ctxt 398176780      btime 1793942878      processes 538201
procs_running 3     procs_blocked 0       intr 415096808 130 5605 ...
```

and the probe fixing each field's meaning:

```
processes: 538372 -> after 3 forks 538375 (+3) -> after 3 pthread_create 538378 (+3)
ctxt:      398374095 -> 398374107 (+12)
intr:      +6 during the run
btime=1793942878  now=1793998793  uptime=55915.27   (now - uptime rounds to btime)
```

So `processes` counts **threads as well as processes** - Linux increments `total_forks` in
`copy_process`, which every `clone` goes through - `ctxt` counts context switches, `btime` is
`now - uptime` to the second, `procs_running` is `nr_running`, and `procs_blocked` is
`nr_uninterruptible`.

**What this kernel answered.** The generator was one `format!`:

```
cpu  {} 0 0 0 0 0 0 0 0 0        <- the {} is (ticks / TICK_HZ), the uptime in seconds
intr 0
ctxt 0
```

with no `btime`, `processes`, `procs_running` or `procs_blocked` line at all. Measured on the machine
before the fix (`build/scratch/stat/read.c` staged as `t-statread`, log `build/boot-statread-cf.log`):

```
--- t=0
cpu  0 0 0 0 0 0 0 0 0 0
intr 0
ctxt 0
--- t=12s
cpu  12 0 0 0 0 0 0 0 0 0
intr 0
ctxt 0
```

The user column **grew by one per second of wall time** - it was the uptime, and by minute ten it would
have claimed ten thousand times the CPU time the machine had spent - while `system` and `idle` were
hardcoded zero, `intr`/`ctxt` claimed the machine had never taken an interrupt or left a task, and the
four lines every monitor wants were absent (a reader's parse error, not a zero).

**The fix.** A new host-compiled `src/sched/cpustat.rs` carries the counters this kernel actually keeps:
`USER_TICKS`/`SYSTEM_TICKS`/`IDLE_TICKS` charged from the tick (`charge_cpu`, the same hook the load
average samples on), `TASKS_MADE` bumped in `Table::alloc_slot` (the one door a task enters through, so
threads count exactly as Linux counts them), and `SWITCHES` bumped in `schedule` after `hand_over` and
before the stacks change. `/proc/stat` prints them in Linux's order and shape: an aggregate `cpu` line
with ten columns, a `cpu0` line (a uniprocessor's aggregate *is* its one CPU), `ctxt`, `btime`
(`hal::boot_epoch`, which is what the wall clock anchors on), `processes`, `procs_running`
(`sched::runnable_count`) and `procs_blocked` (a new `sched::blocked_count`, the uninterruptible
sleepers the load average already folds into `active`).

**Which context is idle.** The first boot of the pin failed on the idle column: it stayed zero while the
process slept. The cause was the definition of "nobody was running" - every reader asked "is there a
current task", and the answer is always yes: the scheduler's own loop *is* the current task whenever it
has nothing to hand the CPU to (`hand_over` enters `SCHED_ID` and the picker runs there). So idle ticks
were charged to the scheduler, and the same mistake floored the **load average** at about 0.9 on an idle
machine (`active` always counted one) and made `procs_running` at least one. All three readers now ask
`current != SCHED_ID`, which is what "the idle context" means here. Linux's `nr_running` excludes idle
for the same reason, and its idle time is the idle task's own.

**What is still not counted, and is named rather than faked.** The `cpu` line's other seven columns -
nice, iowait, irq, softirq, steal, guest, guest_nice - print zero because this kernel keeps no such
dimension: interrupt time is charged to whatever the interrupt interrupted (Linux takes `irq`/`softirq`
*out* of system and times handlers separately, a measurement this kernel does not make), and a
uniprocessor with no steal or guest notion has nothing to put in the last four. The `intr` line is
**absent** rather than zero: there is no per-vector interrupt count, and a zero would claim the machine
had never taken an interrupt - the one kind of lie `AGENTS.md` §3 forbids. Both are recorded in
`docs/ROADMAP.md` with what each needs (per-handler time accounting, a per-vector dispatch count).

**Pin.** `user/tests/src/bin/t-procstat.rs`, nine claims: the ten columns exist; user+system grow while
this process burns CPU; idle grows while the machine sleeps; `btime` is `now - uptime` within two
seconds; `processes` grows when the run forks; `ctxt` grows across a sleep; `procs_running` reaches two
with a spinner beside this thread; `procs_blocked` reads zero (and exists); and the aggregate equals
`cpu0` on this one-CPU machine (machine-only - Linux on many CPUs would disagree). Every claim holds on
the host too (`MPOS_ON_HOST=1`: `btime 1793942878 against 1793942878, 0 s apart`, `processes 544329 ->
544330`, `ctxt 403807382 -> 403808181`).

Red on the unfixed kernel (`build/boot-procstat-cf1.log`): seven of the nine -

```
FAIL:  user and system grow while this process burns CPU (0 -> 0)
FAIL:  and the idle column grows while the machine has nothing to run (0 -> 0)
FAIL:  the file names the boot instant (btime is a line of its own)
FAIL:  the file counts the tasks it made and the switches it took
FAIL:  procs_running counts the threads that could run (0)
FAIL:  procs_blocked names the uninterruptible waiters (absent)
FAIL:  the file has a cpu0 line beside the aggregate
```

- and green with it (`build/boot-procstat-stat2.log`): `the cpu line carries ten columns ([46, 0, 23, 0,
...])`, `user and system grow while this process burns CPU (70 -> 146)`, `and the idle column grows while
the machine has nothing to run (0 -> 76)`, `btime is now minus uptime (1793999696 against 1793999695, 1 s
apart)`, `processes counts the task this run forked (12 -> 13)`, `ctxt counts the switches a sleep takes
(493 -> 539)`, `procs_running counts the threads that could run (4)`, `procs_blocked names the
uninterruptible waiters (0)`, `the aggregate is cpu0, this being one CPU (323 against 323)`. The two
claims that stay green on both are shape claims (ten columns, and the burn) - which is why a pin needs
claims that would fail if the number were a constant.

**The rule.** A field every monitor parses is a promise that the kernel *counts* that thing; printing
the nearest number at hand (here, the uptime) is worse than leaving the line out, because the reader
subtracts two readings and reports the difference. And "idle" is a context, not the absence of a slot:
the scheduler's own loop is a task on this machine, so every question about "is anything running" has to
name it.

## §6.101 `/proc/<pid>/stat`'s nice pair were two literals (LANDED 2026-11-07)

`/proc/<pid>/stat` fields 18 and 19 — `priority` and `nice` — were the string literals `"20"` and
`"0"`, so a process that had called `setpriority` was still described by a `ps` as nice 0 at priority
20. It is one number reported twice, and the kernel had it stored (`Task::nice`, `src/sched/runtime/
task.rs:1291`) the whole time: the file simply did not read it. Measured on a current host that the
pair is one number (`build/scratch/psig/n.c`, Linux 7.2.14): with the process at nice 19 the two
fields read

```
nice=0       priority(f18)=39 nice(f19)=19
```

so field 18 is `20 + nice` and field 19 is the stored nice. (The same probe is why the host cannot
*counterfactual* this: every process in that container starts at nice 19 and `RLIMIT_NICE` is `0/0`,
so `setpriority` to anything else is refused - the machine's own process starts at nice 0 and can be
lowered, which is what the pin sets.)

**The fix** is the field reading the value rather than a constant: `TaskSnapshot` carries `nice`
(`src/sched/runtime/procfs.rs:275`, filled by `snapshot_of` from `task.nice`), the host mirror
`Process` carries it too, and `stat_line` (`src/vfs/procfs.rs:1698`) formats `20 + nice` and `nice`.

**The pin** is in `user/tests/src/bin/t-priority.rs`, which already sets and restores a nice value
through `setpriority`: two claims beside the round-trip.

- `proc stat's priority is 20 + its nice (reads 23/3)` — the relation, on both machines.
- `proc stat reports the nice setpriority stored (reads 23/3)` — the machine claim: after
  `setpriority(PRIO_PROCESS, 0, 3)` both doors say 3.

The relation claim alone is not a counterfactual (the literals `20`/`0` satisfy it), which is why the
second claim exists and is machine-only: this host starts every process at nice 19.

```
unfixed kernel (build/boot-nicefield-cf1.log)   ok:   priority is 20 + its nice (reads 20/0)
                                                FAIL: reports the nice setpriority stored (reads 20/0)
                                                === t-priority exit=1
fixed kernel   (build/boot-nicefield2.log)      ok:   priority is 20 + its nice (reads 23/3)
                                                ok:   reports the nice setpriority stored (reads 23/3)
                                                === t-priority exit=0
```

**The rule.** A field a reader formats is answered from the kernel's own record of that thing, not
from the default value of the thing: two literals look like an answer because the common case is nice
0, and the case that is not the common case is the one a monitor exists to show.

## §6.102 `/proc/<pid>/stat`'s four signal words were zeros (LANDED 2026-11-07)

Fields 31-34 of `/proc/<pid>/stat` are `signal`, `blocked`, `sigignore` and `sigcatch`: what is
pending **on this task**, what it has blocked, and the two disposition sets of its **process**. This
kernel printed `0` for all four. That is the §6.101 shape again, and it has the same tell - the
common case is a quiet process, so a `ps` with extra columns, a debugger or a supervisor asking "the
signal was sent and has not arrived, why" reads four zeros that look like the answer.

All four were already in the table: `Task::signals` and `Task::blocked` are the pending and blocked
words (`signal::Pending`, a `u64` with signal *n* in bit *n-1*), and the group's dispositions sit in
`Task::dispositions` (`Arc<SignalHand>`, shared across the threads by `CLONE_SIGHAND`).

Measured first (`build/scratch/sigfield/p.c`, Linux 7.2.14), one call at a time so the layout is a
reading rather than a guess:

```
baseline                           sigpnd=0 blocked=0 ignore=16781312 catch=0
after blocking SIGUSR1 (10)        sigpnd=0 blocked=512 ignore=16781312 catch=0
after SIG_IGN SIGUSR2 (12)         sigpnd=0 blocked=512 ignore=16783360 catch=0
after a SIGTERM (15) handler       sigpnd=0 blocked=512 ignore=16783360 catch=16384
after raising a blocked SIGWINCH   sigpnd=134217728 blocked=134218240 ignore=16799744 catch=0
after unblocking (default ignore)  sigpnd=0 blocked=134217728 ignore=16799744 catch=0
```

Three facts in that block are the implementation: the bit is `1 << (n - 1)` (`SIGUSR1` is 10 and
adds 512, `SIGWINCH` is 28 and adds `1 << 27`); a disposition is `SIG_IGN` *or* a handler and never
both (replacing the `SIGTERM` handler with `SIG_IGN` moved the same bit from field 34 to field 33);
and the two disposition words are the process's while the two signal words are the task's - the
probe's worker thread blocking `SIGUSR1` changed its own `task/<tid>/stat` and left the leader's word
where it was. The baseline is not zero even on Linux: the libc or the launcher ignores `SIGPIPE`
(`16781312` is `SIGPIPE | SIGXFSZ`), which is why every claim in the pin is a *relation*.

**The fix.** `TaskSnapshot` carries `pending`, `blocked`, `ignored`, `caught`
(`src/sched/runtime/procfs.rs`), filled in `snapshot_of`; the last two from a
`fn disposition_sets(actions: signal::Actions)` walk over `1..=MAX_SIGNAL`. It takes
`SignalHand::snapshot()` rather than the shared handle on purpose: a disposition is read by every
thread of the group, so walking the shared `Arc` sixty-four times would hold its lock across the
walk while another thread may be in `sigaction`. `stat_line` (`src/vfs/procfs.rs:1727`) formats the
four words as decimal numbers.

**The pin** is `user/tests/src/bin/t-stat-signal-fields.rs`, nine claims, each a *movement* against
what the process did to itself rather than a constant (the baseline is not the same on the two
machines - Linux's is `16781312`, this kernel's is `4096`, both of them `SIGPIPE`):
blocking `SIGUSR1` adds exactly its bit to `blocked`; `SIG_IGN` on `SIGUSR2` adds exactly its bit to
`sigignore`; a `SIGTERM` handler adds exactly its bit to `sigcatch` and moves nothing in `sigignore`;
raising a blocked `SIGWINCH` adds exactly its bit to `pending` and nothing to `blocked`; unblocking
delivers the default (discard) and clears `pending`; and this thread's `task/<tid>/stat` carries the
leader's `blocked` word.

```
unfixed kernel (build/boot-sigfield-cf1.log)  ok:   the four signal words read as numbers (… 0, 0, 0, 0)
                                              FAIL: blocking SIGUSR1 puts its bit in the blocked word (0 -> 0 …)
                                              FAIL: ignoring SIGUSR2 puts its bit in the ignore word (0 -> 0 …)
                                              FAIL: a handled SIGTERM puts its bit in the catch word (0 -> 0 …)
                                              FAIL: a blocked signal that is raised shows in the pending word
                                              === t-stat-signal-fields exit=1
fixed kernel   (build/boot-sigfield2.log)     all nine ok, PASS, exit=0
```

**Two claims in `t-times` were reshaped by this gate, and neither was this change's doing.** The
aarch64 arm reddened on two readings §6.96 had written: first `/proc/self/stat`'s utime against a
`RUSAGE_SELF` record taken *before* the forked child and the burn loops (`110000 us against
88000 us` - the file honestly counted CPU the stale record predated), then the thread spelling
demanded `thread < group` where both read `5` ticks because this thread was the one that burned
(`build/gate-sigfield.log`, `build/gate-sigfield3.log`). Both are the same mistake as §6.97's
`5 steps`/`1603792 ns apart`: a claim comparing two doors must bracket them around the read
(two `RUSAGE_SELF`/`RUSAGE_THREAD` reads with one tick of slack each way) and state a monotone
relation separately, rather than demand two separately-taken numbers agree to a constant. The
unfixed-kernel red cited above (§6.96's `0 us against 40000 us`) still holds for the bracketed
form: `0 + 10000 >= 40000` is false, which is what `build/boot-procstat-cf.log` measured.

**The rule.** A word a reader subtracts, masks or prints is answered from the kernel's own set, and
a set field has a second obligation a scalar does not: the *layout* is part of the contract. Here it
is the wire `sigset_t`'s (signal *n* in bit *n-1*), which is why no conversion sits between
`Task::blocked` and the file - and why the pin could be written as "moved by exactly this bit"
before either machine was touched.

## §6.103 `vsize` and `statm`'s total counted only the pages that were *present* (LANDED 2026-11-07)

Two files answer the two memory questions a monitor asks separately, and this kernel answered
both with the same number. `/proc/<pid>/statm`'s seven fields are total, resident, shared, text,
library, data and dirty, in pages; `/proc/<pid>/stat`'s field 23 is `vsize`. The first field and
`vsize` are Linux's `mm->total_vm` - the pages the address space has **mapped**, present or not -
and this kernel formatted `vsize` as `rss_pages * 4096` and printed `rss_pages` three times in
`statm`. So a process that mapped a gigabyte and touched one page was reported as one page: an
address-space leak was invisible, and the file could not answer the question it exists for.

**Measured on a current host first** (`build/scratch/vsize/p.c`, Linux 7.2.14, x86_64, one page
size of 4096):

```
baseline            vsize=258048  statm total=63   resident=24  shared=18 text=13 data=37
mapped 4 MiB rw, touched none
                    vsize=4452352 statm total=1087 resident=24
touched all of it   vsize=4452352 statm total=1087 resident=1048 dirty=0
a second 8 MiB, untouched
                    vsize=12840960 statm total=3135 resident=1048
unmapped the first  vsize=8646656 statm total=2111
```

`vsize - total * 4096 == 0` at every step, and the mapping moved `total` by exactly `1024`/`2048`
pages while `resident` did not move at all - so the two fields are two questions and `vsize` is
`total` in bytes.

**This kernel, same probe, before the fix** (`build/boot-vsize-cf.log`):

```
baseline            vsize=151552  statm total=37  resident=37
mapped 4 MiB rw, touched none
                    vsize=151552  statm total=37  resident=37      <- nothing moved
touched all of it   vsize=4345856 statm total=1061 resident=1061
a second 8 MiB      vsize=4345856 statm total=1061 resident=1061
unmapped the first  vsize=151552  statm total=37  resident=37
```

The untouched mapping did not exist to either file, and touching it moved *both* numbers by
1024 pages - the pairing that says one number is being read twice.

**The fix.** The counting belongs to the address space, so it lives on `Mm`:
`src/sched/runtime/child_mm.rs`'s `Mm::mapped_pages` sums the VMA table and the eagerly-filled
ranges in one critical section (the two descriptions of the space `MapFacts` already names), each
range rounded up to whole pages. `TaskSnapshot` carries it as `virtual_pages` beside `rss_pages`,
`stat`'s field 23 prints it times the page size through `vsize_bytes`, and `statm_line` prints
`{virtual_pages} {rss_pages} 0 0 0 0 0`.

Two consequences worth stating rather than hiding:

- **`statm`'s other five fields print `0` and are a named gap**, not a shape. `shared`, `text`,
  `library`, `data` and `dirty` need accounting this kernel does not keep - which frames belong to
  a file and are shared, how many are the image's text, dirty versus clean. `data` used to print the
  resident count, which is not that question; a plausible number there is worse than an honest
  zero, because `ps_mem` and `smem` fold these fields into a total. The gap is named in
  `docs/ROADMAP.md` and item 216.
- `stat`'s field 23 is in **bytes** and `statm`'s first field in **pages**; the two files must agree
  through the page size, which the host unit test asserts as one relation
  (`vfs::procfs::tests::vsize_counts_address_space_and_rss_counts_what_is_behind_it`).

**Pin.** `user/tests/src/bin/t-statm.rs` (11 claims): the seven fields read; `vsize == total * 4096`; resident never exceeds the space; a 4 MiB anonymous mapping nobody touches extends `total`
by 1016+ pages while `resident` does not move; touching it makes 1016+ pages resident and leaves
`total` where it was; unmapping returns `total` to its baseline. The two files are read as a
*bracket* - `statm`, `vsize`, `statm` - because they are two reads of a moving address space: the
first version compared a `statm` taken before the read with a `vsize` taken after it and reddened
on the host by three pages the process had mapped in between (`622592 against 610304 bytes`).

**Counterfactual** (`build/boot-statm-cf1.log` against `build/boot-statm2.log`):

```
unfixed kernel  ok:    statm has seven page counts ((121, 121, 0, 0, 0, 121, 0))
                FAIL:  mapping 4 MiB nobody has touched extends the address space (121 -> 123 pages)
                ok:    and adds nothing to the resident set (121 -> 123 pages)
                FAIL:  and leaves the address space where it was (1147 against 123 pages)
                === t-statm exit=1
fixed kernel    all eleven ok, PASS, exit=0
             (124 -> 1151 pages; 121 -> 123 resident; touching makes 1024 resident, total 1151)
```

The three claims that pass in both are the relations (`vsize == total * page`, resident ≤ total,
unmapping returns to baseline) - they hold for any implementation that formats both fields from
one number, which is exactly why the claims that *move* are the pin.

**The rule.** A file two tools read must answer the question each is asking: "how much address
space does this process hold" is a claim about mappings, "how much of it is in memory" is a claim
about pages present, and one number cannot be both. When a field looks plausible because the
common case has them equal (here, a small program whose mapped and resident counts happen to be
close), find the case that separates them - map without touching - and make that the pin.

## §6.104 `stat`'s `starttime` was the literal zero, so every process started at boot (LANDED 2026-11-07)

`/proc/<pid>/stat`'s field 22 is the instant a task came into being, in `USER_HZ` ticks since boot,
and this kernel printed `0` there under a comment that said there was no per-task record of when a
process began. The comment was true when it was written and the field was not: `ps` reads it, and
`btime` (now served, §6.100) beside it, and computes a process's age as `now - btime -
starttime/USER_HZ`. A zero there tells every reader that every process on the machine started with
it - a plausible-looking `00:00` in `ps`'s `START` column instead of an error, which is why it
survived.

**Measured on a current host** (`build/scratch/starttime/p.c`, Linux 7.2.14,
`sysconf(_SC_CLK_TCK) = 100`):

```
hz=100 self starttime=6343185 uptime=63431.94 -> implied age 0.090 s
/proc/<pid>/stat for this pid: starttime=6343185 (same as self: 1)
uptime 63435.94 child(1 s in) starttime=6343194 implied age 4.000 | grandchild starttime=-1
```

Two children forked a second apart read counts a hundred ticks apart, the field is the same number
through `/proc/self` and `/proc/<pid>`, and the unit is the ABI's `USER_HZ` (100) - not this kernel's
`TICK_HZ` (250).

**The fix, and why it is a record rather than arithmetic.** `Task` carries `start_nanos`, written at
the six places that construct one, because nothing else knows when a task began: a child forked late
in a boot begins late, and its creation is the only moment that fact exists. `TaskSnapshot` and the
host `Process` mirror it, `stat`'s field 22 renders `start_nanos / 10_000_000` (Linux's
`nsec_to_clock_t` truncation), and a task created inside the first centisecond reads `0` exactly as
Linux floors it.

**Pin** `t-starttime` (6 claims, `user/tests/src/bin/t-starttime.rs`): a nonzero count; the same
number through `/proc/self` and `/proc/<pid>`; the field not moving while the task runs; and the two
that carry the change - a child forked 1.5 s later reads a later count, and the difference is the
wait itself (a centisecond per tick). Measured on the machine: `Some(30)` for a process 0.3 s into
the boot, then `Some(150) ticks against Some(1510) ms` between the two children.

```
unfixed kernel  FAIL:  stat's starttime is a tick count, not a literal zero (Some(0))
                ok:    the task's own file and its /proc/<pid>/stat agree (Some(0) against Some(0))
                ok:    the field does not move while the task runs (Some(0) against Some(0))
                FAIL:  a child created later carries a later starttime (Some(0) ticks)
                FAIL:  and the difference is the wait, a tick per centisecond (Some(0) ticks against Some(1500) ms)
                FAIL:  and an age computed from it is younger than the machine (Some(2120) ms of Some(2120) ms)
                === t-starttime exit=1
fixed kernel    all six ok, PASS, exit=0
             (Some(30); Some(150) ticks against Some(1510) ms; Some(1830) ms of Some(2130) ms)
```

The three claims that pass in both are the *agreements* - `self` versus the directory path, and the
field against itself - because a literal answers both consistently. The pin is the movement: a
later child's later count, and the wait between them.

**The rule.** A monitor's field is a record the kernel keeps, not a value that looks right in the
common case: `0` is a legal `starttime` only for a task created in the machine's first centisecond,
and for every other task it is a claim that the process is as old as the boot. When a comment says
"there is no record of that", check whether the record is what is missing - here it was one `u64` at
six construction sites.

## §6.105 `getrusage`'s two switch counters were zeros, beside a CPU pair it counted

**LANDED 2026-11-07.** `struct rusage` has been 144 bytes since the beginning: two `timeval`s of
CPU, twelve counters of page faults, block I/O, IPC and signals, and then the pair only a scheduler
can answer — `ru_nvcsw` (the times the task gave the CPU up because it could not use it) and
`ru_nivcsw` (the times the CPU was taken while it was still runnable). `/proc/<pid>/stat`'s CPU
fields were fixed in §6.96, `RUSAGE_THREAD` in §6.95 and the CPU-time clocks in §6.99, and through
all of it `write_rusage` wrote the two opening `timeval`s and left the last two counters zero — an
honest "nothing measured" that had stopped being true the moment the scheduler had both facts.

### What Linux does (measured)

`build/scratch/rusage/switch.c` (musl static, `-pthread`, Linux 7.2.14, one CPU):

```
200x nanosleep(1ms): nvcsw +200 nivcsw +0
200x sched_yield:    nvcsw +0 nivcsw +1
spinning beside a thread: nvcsw +80 nivcsw +82
join: nvcsw +120 nivcsw +2
```

The sleep is the clean one: one switch per sleep, in the **voluntary** column. The yield is the
other column — the task was runnable when it gave the CPU up, so it is involuntary, and on a
single-CPU machine 200 back-to-back yields cost exactly one switch (the yield returns to the same
task). The spinning row is noise (the libc's own futex waits land in `nvcsw`), which is why no claim
in the pin is built from it.

`build/scratch/rusage/threadsw.c` answers the second question — whose switches are these:

```
RUSAGE_SELF before 0/0 after 101/1 (nvcsw +101 nivcsw +1)
worker's own RUSAGE_THREAD: nvcsw 100 nivcsw 0
```

A hundred sleeps in a joined worker move the **process's** pair and not the main thread's: the group
has the totals, exactly as its CPU pair does (§6.95), and `RUSAGE_CHILDREN` is the third set — the
children's, folded in at the reap beside `cutime`/`cstime`.

### What this kernel answered

`write_rusage` (`src/syscall/linux/proc.rs`) built the wire record with the two `timeval`s and
nothing else, so bytes 128..144 stayed zero on **every** door: `getrusage`'s three `who` values,
`wait4`'s and `waitid`'s `rusage`. A program asking "did I block or was I preempted" was told
"neither, ever" — and the kernel had both facts, because every switch in this kernel goes through
one function.

### The fix

* `Task` gained `nvcsw`/`nivcsw` atomics (the **thread**'s pair) beside `cpu_user`/`cpu_system`;
  `Mm` gained `nvcsw`/`nivcsw`/`cnvcsw`/`cnivcsw` (the **group**'s own and children's columns).
* The classification is made in `Table::hand_over`, once, **after `prepare` has run**: a task that
  has just left the ready queue without exiting (it went to sleep, parked on a deadline, or waits
  for something) is **voluntary**; a task still runnable (the tick preempted it, or it yielded) is
  **involuntary**; an exiting task is counted in neither. The rule is read off the queue rather than
  set by a flag, because `sleep`, `block_until*`, `exit` and `requeue_current` all reach the same
  place — a flag would have to be set at every one of them and forgotten at one.
* The switch charges the task's column **and** the group's in the same call, so the two doors can
  never disagree about how many switches there were (§6.95's reason for the CPU pair).
* The reap folds the child's `Mm::switches_all()` into the parent's child columns beside
  `absorb_child`'s CPU, and `Retired` carries the pair so `wait4`/`waitid` can write the child's
  numbers after the slot is gone — the same carry `utime`/`stime` already needed.
* `write_rusage` writes `ru_nvcsw` at 128 and `ru_nivcsw` at 136 of the 144-byte record; the twelve
  counters before them stay zero (page faults would need an I/O signal this kernel's mapped objects
  do not report — a **named gap**, see below).

### The pin

`user/tests/src/bin/t-rusage-switch.rs`, seven claims, all movements:

```
ok:    getrusage fills the two switch counters (Some(Switches { voluntary: 0, involuntary: 1 }))
ok:    fifty sleeps land fifty voluntary switches (0 -> 50)
ok:    and none of them is a switch the machine took (1)
ok:    a yield is an involuntary switch (202)
ok:    a sibling's sleeps are the process's (Some(50) in that thread)
ok:    and are not this thread's (50 -> 101)
ok:    a waited child's switches fold into the child totals (0 -> 50)
PASS
```

Unfixed kernel (`build/boot-switch-cf1.log`) — the same program, five reds:

```
ok:    getrusage fills the two switch counters (Some(Switches { voluntary: 0, involuntary: 0 }))
FAIL:  fifty sleeps land fifty voluntary switches (0 -> 0)
ok:    and none of them is a switch the machine took (0)
FAIL:  a yield is an involuntary switch (0)
FAIL:  a sibling's sleeps are the process's (Some(0) in that thread)
FAIL:  and are not this thread's (0 -> 0)
FAIL:  a waited child's switches fold into the child totals (0 -> 0)
FAILED
```

The two that pass on both are shape claims (the counters read as numbers; a zero involuntary column
is within the sleep row's bound), which is why they are not the pin: the movements are.

### One old claim had to move with it

`t-wait4` asserted that **all fourteen** counters were zero after a `wait4` - an honest statement
while `write_rusage` filled two `timeval`s and nothing else, and a false one the moment the last two
became real. The gate answered it on both arms (`FAIL:  the unmeasured counters are zero, not the
caller's own seed`, `build/gate-switch.log`). The claim is now two: the twelve fault/block-I/O/IPC
counters are still zero, and the switch pair is the child's - written rather than the `-1` seed
(`build/boot-wait4-switch.log`: `(0 / 58)`, and on this one-CPU machine the tick took the CPU from
the burning child 58 times). That is the contract changing because the kernel now serves the field,
not the test being bent to fit: the twelve that stayed zero are still asserted to be zero.

### Named gap

The twelve counters between the `timeval`s and the switch pair — `ru_maxrss`, `ru_minflt`,
`ru_majflt`, `ru_nswap`, `ru_inblock`, `ru_oublock`, `ru_msgsnd`, `ru_msgrcv`, `ru_nsignals` — stay
zero. The CPU pair and the switches are things this kernel counts; the page-fault pair needs the
minor/major split, and the split needs the mapped-object seam to say whether a fault was answered
from a frame the object already held or by a device read (`MappedObject::frame_at` answers only
"here is the frame"), and `ru_maxrss` is a high-water mark nothing tracks. An honest zero is
recorded in `docs/ROADMAP.md` as this item's gap rather than a plausible number.

**The rule.** A record's field is either counted or named as not counted — "we only write the first
two" ages into a wrong answer as soon as the kernel starts counting the rest, which is exactly what
happened here: the CPU pair beside the zeros was right, so nothing looked broken. And a counter with
two columns has one place that decides which column a switch lands in; that place is after the
caller has said where the task is going, because before it, the question has no answer.

## §6.106 `/proc/<pid>/status` printed zeros for the masks and the switches

**LANDED 2026-11-07.** `/proc/<pid>/status` is the long form of the process table — what `top`,
`ps`, a shell's job control and a debugger read. Six of its lines were literals in the kernel while
the facts behind them were already in the task table: `SigPnd`, `ShdPnd`, `SigBlk`, `SigIgn`,
`SigCgt`, and the two `*_ctxt_switches` lines. §6.102 had just put the same four masks into
`/proc/<pid>/stat`'s fields 31–34 and §6.105 had just started counting switches, so the zeroes were
not a missing feature but a second door that had not been told.

### What Linux prints (measured)

`build/scratch/status/t.c` (musl static, Linux 7.2.14):

```
blocked USR1+INT+ILL:             SigBlk:	000000000000020a
ign+handler:                      SigIgn:	0000000001001800
                                  SigCgt:	0000000000004000
raised USR1 (blocked):            SigPnd:	0000000000000200
```

Sixteen lowercase hex digits, zero padded, signal `n` at bit `n - 1` — the same layout `stat`'s
words use, in the other notation. `build/scratch/status/s.c` additionally showed the disposition
words partition: setting `SIGUSR2` to `SIG_IGN` moves its bit from `SigCgt` to `SigIgn`, never both.

The switch lines are the **thread's** own, which is the fact that decides where they come from.
`build/scratch/status/u.c`:

```
main before voluntary=15
worker own status voluntary=50
main after voluntary=16
```

Fifty sleeps in the worker read `50` in the worker's own `/proc/<pid>/task/<tid>/status`, while the
main thread moved by one — and `getrusage(RUSAGE_SELF)` for the same process answered `106`, the
group's number. So the status lines are the thread's pair (§6.105's `Task::nvcsw`/`nivcsw`), not the
group's columns, and a program that opens one file and reads the other gets two numbers that
disagree **because they are two different questions**.

### The fix

`status_of` renders all six lines from the same `TaskSnapshot` that `stat`'s fields read (§6.102,
§6.105), through one helper:

```rust
fn mask_word(mask: u64) -> alloc::string::String { alloc::format!("{mask:016x}") }
```

* `SigPnd`/`SigBlk` — this thread's pending and blocked sets.
* `SigIgn`/`SigCgt` — the group's dispositions, split into what it ignores and what it has a handler
  for. The two words never share a bit, which is the property that makes the split a partition
  rather than two entries in a table.
* `ShdPnd` — the group's pending set. This kernel keeps one pending set **per task**, and the one
  fact that makes the group word answerable is §6.78b's delivery rule: a process-directed signal
  lands on the leader, so the leader's pending set is the shared one every member would report.
* `voluntary_ctxt_switches`/`nonvoluntary_ctxt_switches` — the thread's pair, off `Task` and not off
  the group's `Mm`, because the measurement above says Linux answers from the thread.

### The pin

`user/tests/src/bin/t-status-masks.rs`, eleven claims, movements and two-door agreements:

```
ok:    status prints the mask and switch lines (true)
ok:    blocking SIGUSR1 puts its bit in SigBlk (0000000000000000 -> 0000000000000200)
ok:    and stat's blocked word says the same (Some(512))
ok:    ignoring SIGUSR2 puts its bit in SigIgn (0000000000001000 -> 0000000000001800)
ok:    a handled SIGTERM puts its bit in SigCgt (0000000000000440 -> 0000000000004440)
ok:    and the two words never share a bit (0000000000001800 & 0000000000004440)
ok:    a raised blocked signal shows in SigPnd (0000000000000200)
ok:    fifty sleeps move voluntary_ctxt_switches by fifty (0 -> 50)
ok:    and none of them is an involuntary one (7 -> 9)
ok:    a sibling's sleeps are that sibling's lines (Some(50))
ok:    and not this thread's (50 -> 51)
PASS
```

Unfixed kernel (`build/boot-status-cf1.log`) — seven red:

```
FAIL:  blocking SIGUSR1 puts its bit in SigBlk (0000000000000000 -> 0000000000000000)
FAIL:  and stat's blocked word says the same (Some(512))
FAIL:  ignoring SIGUSR2 puts its bit in SigIgn (0000000000000000 -> 0000000000000000)
FAIL:  a handled SIGTERM puts its bit in SigCgt (0000000000000000 -> 0000000000000000)
FAIL:  a raised blocked signal shows in SigPnd (0000000000000000)
FAIL:  fifty sleeps move voluntary_ctxt_switches by fifty (0 -> 0)
FAIL:  a sibling's sleeps are that sibling's lines (Some(0))
FAILED
```

The four that pass on both are shape claims (a zero mask parses, two zero words do not share a bit,
a thread that moved by zero did not move by fifty) — the mask format itself is pinned on the host
instead, by `vfs::procfs::tests::a_signal_mask_is_sixteen_lowercase_hex_digits`, because a
sixteen-digit lowercase word is text and not state.

**The rule.** A second door onto the same fact is not free: §6.102 made `stat`'s words real and this
file kept printing zeros for weeks of commits — the tell was that the numbers were *round* (all
zero, all the time) while a sibling file carried the same facts. When a field lands on one door,
grep for the other doors that print it: `status`, `stat`, `statm`, `getrusage` and `wait4` are five
renderings of one task, and they must be made of the same numbers in the same units.

## §6.107 `/proc/<pid>/status`'s identity lines were constants

**LANDED 2026-11-07.** §6.106 made the masks and the switch lines real; ten lines below them were
still literals, and every one of the facts behind them was held by the kernel: the process's umask,
the task's tracer, `no_new_privs`, the seccomp mode and filter count, and the five capability words.
The capability words were the worst of the five kinds of wrong: they were **right for a root payload
and wrong for everything else**, so a `t-status`-style reading of a dropped program reported the full
set it no longer held.

### What Linux prints (measured, `build/scratch/status2/p.c`)

```
baseline                             Umask: 0022  TracerPid: 0  NoNewPrivs: 0  Seccomp: 0  Seccomp_filters: 0
after umask(077) + PR_SET_NO_NEW_PRIVS    Umask: 0077   NoNewPrivs: 1
after seccomp(SET_MODE_FILTER, one filter)   Seccomp: 2   Seccomp_filters: 1
tracee's status (PTRACE_TRACEME child)      TracerPid: 684234    (its tracer's pid)
tracer's own status                         TracerPid: 0
```

`Umask:` is four **octal** digits; `Seccomp:` is a mode (`0` none, `1` strict, `2` filter) and
`Seccomp_filters:` how many filters are stacked; `TracerPid:` is the tracer's number **in the
reader's namespace** (the same door `Pid:` goes through) and `0` when nobody is attached. The five
`Cap*` words are sixteen lowercase hex digits and come from the credential model's own masks —
`build/scratch/status2/p.c` read `CapInh 0000000800000000`, `CapPrm`/`CapEff` `0000000000000000`
(a container drops them), `CapBnd 000001ffffffffff`.

### The fix

`TaskSnapshot` gained `umask`, `no_new_privs`, `seccomp_mode`, `seccomp_filters` and the four
credential masks; `status_of` renders them through two helpers:

```rust
fn mask_word(mask: u64) -> alloc::string::String { alloc::format!("{mask:016x}") }
fn umask_word(umask: u32) -> alloc::string::String { alloc::format!("{:04o}", umask & 0o777) }
```

and `TracerPid` comes from a new `tracer_number(slot)` (`crate::sched::tracer_of` → `reader_number`,
so the file's `Pid:` and `TracerPid:` are the same numbering). `CapAmb` stays `0` and is honest
rather than claimed: `PR_CAP_AMBIENT`'s raises are refused here, so no task can hold an ambient cap.

### The pin

`user/tests/src/bin/t-status-identity.rs`, ten claims — movements, a two-door agreement against
`capget(2)`/`PR_CAPBSET_READ`, and a **dropped-identity** child so a constant cannot pass:

```
ok:    status prints the identity lines (true)
ok:    umask(077) shows as the four octal digits 0077 (0077)
ok:    PR_SET_NO_NEW_PRIVS shows as NoNewPrivs (0 -> 1)
ok:    a filter shows as Seccomp 2 and one filter (0 -> 2, 0 -> 1)
ok:    status's CapInh/Prm/Eff are capget's words (0/000001ffffffffff/000001ffffffffff against the same)
ok:    and its CapBnd is the set PR_CAPBSET_READ answers (000001ffffffffff against 000001ffffffffff)
ok:    a dropped identity empties the two capability words (0000000000000000/0000000000000000)
ok:    and status says the same (0/0 against 0/0)
ok:    a tracee's status names its tracer (Some(5) against this pid 5)
ok:    and the tracer's own status names none (Some(0))
PASS
```

Unfixed kernel (`build/boot-identity-cf2.log`) — five red:

```
FAIL:  umask(077) shows as the four octal digits 0077 (0022)
FAIL:  PR_SET_NO_NEW_PRIVS shows as NoNewPrivs (0 -> 0)
FAIL:  a filter shows as Seccomp 2 and one filter (0 -> 0, 0 -> 0)
FAIL:  and status says the same (000001ffffffffff/000001ffffffffff against 0/0)
FAIL:  a tracee's status names its tracer (Some(0) against this pid 5)
FAILED
```

The five that pass on both are the root payload's own agreement claims (`CapInh/Prm/Eff` against
`capget`, `CapBnd` against `PR_CAPBSET_READ`, and the empty-set claim read from `capget`), which is
exactly why the dropped child is in the test: a constant is indistinguishable from a fact while the
only reader holds the constant's value.

**The rule.** A capability word is a *set a task holds*, and a constant is a claim that every task
holds the same set. §6.93 and §6.106 each set a precedent: measure the thing the field reports, and
put the reader in a state that can distinguish it from its most plausible constant. For the
capability words that state is a dropped identity — and `capget(2)`/`PR_CAPBSET_READ` are the second
and third doors onto the same words, so the pin compares doors rather than trusting one.

## §6.108 `/proc/<pid>/status`'s `Groups:` line was empty, and two of the notation tests were never run

**LANDED 2026-11-07.** §6.107 made the identity lines real. The `Groups:` line under them was a
literal part after the tab - empty for every task - while the identity record has carried the
supplementary group list since `setgroups(2)` was served (`Identity::groups`, in the order the call
gave them).

### What Linux prints (measured, `build/scratch/status2/g.c`)

```
Groups:	953 966 992 998 1000 
```

One space after **every** gid, the last one included, and nothing after the tab when the list is
empty. The `cat -A` shape is `Groups:^I953 966 992 998 1000 $`, which is why `groups_line` writes a
trailing space rather than joining: a reader that splits on whitespace cannot tell, but a reader that
compares the line to its own `%u ` formatting can.

### The fix

`TaskSnapshot` gained `groups: Vec<u32>` (a clone of the identity record's list, which is small and
per process); `status_of` renders it through

```rust
fn groups_line(groups: &[u32]) -> alloc::string::String // "10 20 30 " and "" for none
```

and the host test `vfs::procfs::tests::a_group_list_is_gids_separated_by_one_space_each` pins the
spacing on the host, where the line is text and not state.

### The pin

Three new claims in `t-status-identity` (thirteen total), machine-only because `setgroups(2)` needs
privilege the host test lacks:

```
ok:    setgroups(0) empties the Groups line (Some([]))
ok:    setgroups(10, 20, 30) lists them in that order (Some([10, 20, 30]))
ok:    and getgroups reports the same list (Some([10, 20, 30]))
```

Unfixed kernel (`build/boot-groups-cf1.log`):

```
FAIL:  setgroups(10, 20, 30) lists them in that order (Some([]))
FAILED
```

The clear-to-empty claim passes on both, which is the shape a zero field always has - the listing is
the claim that discriminates, and it is the one that names the order.

### A test-integrity correction, in the same file

`5b28889` reported that host tests pin both notations, and at that commit only one of them ran: the
`#[test]` attribute belonged to `a_signal_mask_is_sixteen_lowercase_hex_digits`, and inserting
`a_umask_is_four_octal_digits` *above* it left the new function holding the attribute and the old one
without. The host suite stayed at 738 either way, which is exactly the tell - a count that does not
move when a test is added. Both are registered now (740 host tests), and the lesson is §12.4a's
neighbour: **a test that does not move the count did not run**, and "it passed" from a suite whose
count is unchanged is a statement about nothing.

### A claim this gate had to reshape, in the same commit (corrected by §6.126)

`t-starttime`'s "the difference is the wait, a tick per centisecond" compared one number (`delta*10`)
to one number (`late_at - born_at`) with 60 ms of slop. On the aarch64 arm it read
`150 ticks against 1320 ms` (`build/gate-groups.log`). This section originally called that "fork
latency on a contended TCG lane" — **that diagnosis was wrong**, and §6.126 closes the real cause:
`/proc/uptime` printed an unpadded centisecond fraction (`S.2` where Linux prints `S.02`), and a
bracket whose two endpoints fell on opposite sides of that bad window read `1500 - 180` ms. The
claim now measures a bracket - `before_first`, `born_at`, `before_second`, `late_at` - and asks
whether `delta*10` lies inside `[before_second - born_at, late_at - before_first]` widened by 60 ms.
Green arm reads `151 ticks inside Some((1500, 1500)) ms, Some(1500) ms apart`
(`build/gate-groups-a.log`). The bracket is still the right shape (§6.97's rule: **two stamps whose
moment lies inside an interval cannot be compared as two numbers**), but the 60 ms of slop was
papering over a 180 ms formatting error, not over scheduler load. §6.126 fixed the file; the
`t-starttime` reds were a ~9% probabilistic bug, which is why a single-arm rerun could pass.

**The rule.** A list field is a promise about *order and separators* as much as about membership:
`Groups:` is gids in `setgroups` order with a space each, not a set, and a reader that rebuilds the
line from a set will pass every membership test and still print a different file.

## §6.109 `status`'s four `Vm` lines were one number, and a high water is a memory (LANDED 2026-11-07)

**LANDED 2026-11-07.** §6.103 gave `/proc/<pid>/stat`'s `vsize` and `statm`'s total the address
space rather than the resident set, and §6.108 finished the `Groups:` line. The four `Vm` lines
below them in `/proc/<pid>/status` still came from one `let kb = task.rss_pages * 4096 / 1024;`:
`VmPeak`, `VmSize`, `VmHWM` and `VmRSS` all printed the same kilobytes, and `VmData`/`RssAnon` used
it as well. A program reading `top` or a monitoring agent asking "how big did this process get"
was told its current resident set, and a 4 MiB mapping nobody had touched moved **none** of the
four lines.

### What Linux answers (7.2.14, measured)

`build/scratch/vmfields/u.c` maps 4 MiB `MAP_PRIVATE|MAP_ANONYMOUS`, touches all of it, then
unmaps it, reading the four lines at each step with `sscanf(strchr(buf, ':') + 1, "%ld", …)` (the
tab after the colon is why a fixed `buf + 6` reads -1):

```
baseline                  VmPeak=248   VmSize=248   VmHWM=88   VmRSS=88   kB
mapped 4 MiB, untouched   VmPeak=4344  VmSize=4344  VmHWM=88   VmRSS=88   kB
touched all 4 MiB         VmPeak=4344  VmSize=4344  VmHWM=4184 VmRSS=4184 kB
unmapped it again         VmPeak=4344  VmSize=248   VmHWM=4112 VmRSS=88   kB
```

So the file is four books, not one number with four names: `VmSize` is the address space now,
`VmPeak` is the largest address space ever, `VmRSS` is what is present now, `VmHWM` is a record of
the largest resident set. The unmap row is the whole distinction — the two current counts fall back
and the two high waters do not unhappen.

`build/scratch/vmfields/w.c` also measured a subtlety worth writing down before it becomes a false
claim: `VmHWM` is **not** `max(VmRSS)` sampled at every instant. It is the synced `mm->hiwater_rss`,
and on this host it dipped `4188 -> 4112 kB` across a `munmap` while `ru_maxrss` stayed at
`4112 kB`. A pin asserting `VmHWM >= the largest transient VmRSS reading this program took` would be
red on Linux itself. The honest claim is that it still records a set much larger than the current
one after the pages are gone.

### The fix

- `src/sched/runtime/child_mm.rs`: `Mm` gains `hwm_frames: AtomicUsize` and
  `peak_pages: AtomicUsize`. `own()` does `self.hwm_frames.fetch_max(frames.len(), Relaxed)` inside
  its `critical_section::with`, and a private `record_peak()` does
  `self.peak_pages.fetch_max(self.mapped_pages(), Relaxed)`. It hangs off every place the VMA table
  grows — `record_range`, `vma_insert`, `vma_insert_present` — and off `set_vmas` (a `fork` copies
  the VMA table, and Linux's `copy_mm` carries the peak with it). `with_eager` seeds both from the
  eager ranges and frames so a task born with a loaded image starts at its own size, not zero.
- `src/sched/runtime/procfs.rs`: `TaskSnapshot` carries `peak_pages`/`hwm_frames` off `task.mm`.
- `src/vfs/procfs.rs`: `struct VmFacts { virtual_pages, rss_pages, peak_pages, hwm_frames }` and
  `fn vm_lines(facts) -> (peak_kb, size_kb, hwm_kb, rss_kb)`, each high water `max(recorded,
  current)` because a read can land between a mapping being made and the record being taken.
  `vm_kilobytes(pages) = pages * 4096 / 1024`. Extracting the four books into a pure function is
  what makes them host-testable: `fn task(_pid) -> Option<Process> { None }` on the host, so
  `status_of` itself is not.

`VmData` and `RssAnon` still print the resident set's kilobytes. Linux counts private *writable*
mappings in `VmData` (16 kB at rest, 4112 kB after a 4 MiB anonymous `MAP_PRIVATE`), and this
kernel's VMA table can tell private from shared but not yet "writable data" from "the stack".
Guessing would be a shape that looks finished while doing less, so that stays a **named gap**
(`docs/uapi/05-work-list.md`, item 222), as do the other `Vm*` lines that still print `0 kB`.

### The pin

`user/tests/src/bin/t-vmstatus.rs` (14 claims) walks the same four steps as the Linux probe and
asserts the separations: mapping moves `VmSize`/`VmPeak` and leaves `VmRSS`/`VmHWM` alone; touching
moves `VmRSS`/`VmHWM` and leaves `VmSize`; unmapping gives the two current counts back while
`VmPeak` keeps the largest address space and `VmHWM` still records the large resident set. Two
claims ask that the peaks stay separated from the current counts after the unmap.

Fixed kernel (`build/boot-vmstatus3.log`, 14 `ok:`, `PASS`):

```
ok:    mapping 4 MiB nobody has touched extends VmSize (492 -> 4600 kB)
ok:    and takes VmPeak with it (492 -> 4600 kB)
ok:    and leaves VmRSS alone (480 -> 488 kB)
ok:    touching it raises VmRSS (488 -> 4584 kB)
ok:    unmapping gives VmSize back (504 against 492 kB)
ok:    but VmPeak keeps the largest address space (4604 against 504 kB)
ok:    and VmHWM still records the large resident set (4588 against a baseline of 480 kB)
ok:    so the two peaks are separated from the two current counts (4604 > 504 and 4588 > 488 kB)
```

Unfixed kernel (`build/boot-vmstatus-cf1.log`, `git stash push` of the three source files, kernel
and loader both rebuilt) — five reds:

```
FAIL:  mapping 4 MiB nobody has touched extends VmSize (480 -> 488 kB)
FAIL:  and leaves VmSize where it was (4584 against 488 kB)
FAIL:  but VmPeak keeps the largest address space (488 against 488 kB)
FAIL:  and VmHWM still records the large resident set (488 against a baseline of 480 kB)
FAIL:  so the two peaks are separated from the two current counts (488 > 488 and 488 > 488 kB)
FAILED
```

The host unit test `vfs::procfs::tests::the_four_vm_lines_are_four_different_books` feeds
`VmFacts { 1024, 128, 2048, 256 }` and `{ 128, 32, 2048, 256 }` and expects four different
answers each time; a one-number implementation cannot produce them. A first attempt at that test
asserted `core::cmp::max(4096, 0) == 4096`, which is true with or without the fix and so pinned
nothing — the repo's counterfactual rule caught it before it landed.

**The rule.** A high-water mark is a *record of the past*, not a function of the current count: it
must be written where the table grows, and it survives the table shrinking. Any implementation that
recomputes it from what is present now will answer every question about the present correctly and
every question about the past wrongly. And before pinning a field, measure whether Linux's own
value is strictly monotonic — `VmHWM` is not, and a stronger claim would have been a test that is
red on Linux.

### §6.110 `/proc/<pid>/status` printed constants for `FDSize` and `SigQ` (LANDED 2026-11-07)

Two more lines of `status_of` were literals: `FDSize:\t64` and `SigQ:\t0/64`. Neither number was
the kernel's. Linux 7.2.14, measured with probes that hold their own `/proc/self/status` stream
open (a probe that reopens the file allocates an fd and perturbs the very table it is reading):

`build/scratch/fdq/formula.c` (fd 3 held, `rewind` on each read):

```
start FDSize=64
high fd=63 (slots=64) FDSize=128
high fd=127 (slots=128) FDSize=256
high fd=255 (slots=256) FDSize=512
high fd=511 (slots=512) FDSize=1024
high fd=1000 (slots=1001) FDSize=1024
```

`boundary2.c` (one `dup(2)` at a time): `fd=64 (65 slots) 64 -> 128`, `fd=128 (129) -> 256`,
`fd=256 (257) -> 512`, `fd=512 (513) -> 1024`. `boundary.c`: closing all 70 descriptors leaves
`FDSize=128` — the value never shrinks. `hole.c`: `dup2` to 200 gives 256, to 1000 gives 1024,
so the number is the *capacity the table has grown to*, not the highest descriptor in use.

That is exactly `slot_count().next_power_of_two().max(64)` — the same rule `FdTable::select_clamp`
already implements. `close()` only `take()`s a slot, it never truncates the `Vec`, and `take_all()`
(`drain(..)`) runs only at exit: so `slot_count()` *is* the high-water slot count. No new fd-table
field is needed; the earlier `both.c` reading that "the select clamp stays 64 while FDSize grows"
was a probe that read its own status file through a fresh `fopen` each time. `TaskSnapshot` now
carries `fd_slots`, and the pure `fd_size(slots)` in `src/vfs/procfs.rs` derives the 64-slot step.

`SigQ` is two books, not one number. `build/scratch/fdq/sigq.c`:

```
baseline SigQ=1/125239
5 RT signals queued SigQ=6/125239
setrlimit(RLIMIT_SIGPENDING,{3,3}) rc=0
after lowering the limit SigQ=6/3
```

The denominator is the **task's own** `RLIMIT_SIGPENDING.cur` (it moves with `setrlimit`, and the
numerator may be larger than it). `sigq3.c` shows the numerator is the **per-user** shared count:
a parent with 5 queued, a live child that queues 2 more, reads `8/125239` for itself and `8/9`
for the child — the child's two records raise the parent's numerator too, while each denominator
stays the rlimit of whichever task was read. `sigq0.c` shows Linux's numerator has a baseline of 1
(the kernel counts a record of its own); this kernel's `queued_for_user` honestly starts at 0, so
the pin asserts *movement*, not that baseline.

Both facts already lived in the kernel. `queued_for_user(t, ruid)` sums `rtq.queued()` across every
task of that uid — it is the same function `send_signal_info` charges against
`task.rlimits[RLIMIT_SIGPENDING].cur` — and is now `pub(crate)` so `snapshot_of` reuses it instead
of writing a second sum that could disagree. `snapshot_of` adds three fields (`fd_slots`,
`sig_queued`, `sig_pending_limit`) all inside the same `with_table` look, and `status_of` renders
them through two pure functions (`fd_size`, `sigq_line`) because a host build has no `TaskSnapshot`
to read (`task()` returns `None`). The remaining `FDSize`/`SigQ` constants (`Ngid`, `CoreDumping`,
`Cpus_allowed*`, `Mems_allowed*`, the other `Vm*` lines) stay a named gap — item 222 covers the
`Vm*` ones.

Pin `user/tests/src/bin/t-status-counts.rs` — nine claims, all movement or separation, so the
literal kernel cannot pass:

```
status prints FDSize and SigQ as two counts
growing the descriptor table past 64 slots moves FDSize (64 -> Some(128))
and the next 64-slot step moves it again (Some(256))
and one more step is the same rule (Some(512))
closing them all never shrinks FDSize (Some(512))
SigQ's denominator is this task's RLIMIT_SIGPENDING soft limit (Some(16) against Some(16))
five queued real-time copies move SigQ's numerator by five (Some(0) -> Some(5))
lowering the soft limit moves the denominator with it (Some(16) -> Some(3))
and the numerator may be larger than that limit (Some((5, 3)))
```

Host (`MPOS_ON_HOST=1`) 9/9 `PASS`; fixed kernel (`build/boot-counts1.log`, kernel + loader both
rebuilt) 9/9 `PASS`, `exit=0`. Counterfactual (`git stash push -q -m cf-counts --` the three
source files, kernel **and** loader rebuilt, `build/boot-counts-cf1.log`) — eight reds, only the
shape claim green:

```
FAIL:  growing the descriptor table past 64 slots moves FDSize (64 -> Some(64))
FAIL:  and the next 64-slot step moves it again (Some(64))
FAIL:  and one more step is the same rule (Some(64))
FAIL:  closing them all never shrinks FDSize (Some(64))
FAIL:  SigQ's denominator is this task's RLIMIT_SIGPENDING soft limit (Some(64) against Some(16))
FAIL:  five queued real-time copies move SigQ's numerator by five (Some(0) -> Some(0))
FAIL:  lowering the soft limit moves the denominator with it (Some(64) -> Some(64))
FAIL:  and the numerator may be larger than that limit (Some((0, 64)))
FAILED
```

`git stash pop` + rebuild + re-boot (`build/boot-counts2.log`) returns to 9 `ok:`/`PASS`.

The host test `vfs::procfs::tests::fd_size_grows_by_the_slot_high_water_and_sigq_keeps_two_books`
feeds `fd_size` the boundaries `0→64, 3→64, 64→64, 65→128, 129→256, 257→512, 513→1024,
1001→1024` and asserts `sigq_line(3,16) != sigq_line(16,3)` — a constant implementation fails
both. The same commit fixes two test-registration mistakes in that module: a duplicate `#[test]`
above the `Vm` test and a missing `#[test]` on `a_group_list_is_gids_separated_by_one_space_each`
(the host count had stayed at 741 instead of 742 — the same tell as §6.108).

**The rule.** A counter is a promise about what is being counted, and Linux counts `SigQ` across a
*user* against *one task's* limit: two different owners, so one `0/64` string cannot be right.
Reuse the kernel's own charge function for the file rather than writing a second sum — the two
readers of one quantity agreeing is part of the contract, not an implementation detail. And measure
a table's boundary with a probe that does not itself grow the table: the first `FDSize` reading was
wrong because `fopen` opened an fd.

### 6.111 `stat`'s `rsslim` was a literal zero, so no process had ever been given a ceiling (LANDED 2026-11-07)

`/proc/<pid>/stat`'s field 25 is `rsslim`. Linux prints the *task's* `RLIMIT_RSS.rlim_cur` as a
bare `u64` word: infinity is the twenty digits of `u64::MAX`, not `-1` and not `0`. Measured on
the host, Linux 7.2.14 (`build/scratch/rsslim/q.c`, the token read raw rather than through
`atol`, which overflowed the first probe into `-1`):

```
baseline  field25 rsslim=[18446744073709551615]
set(4MiB) rc=0
lowered   field25 rsslim=[4194304]
restored  field25 rsslim=[18446744073709551615]
```

`/usr/include/asm-generic/resource.h` names the row `RLIMIT_RSS = 5`. This kernel's
`stat_line` printed `Cow::Borrowed("0")` for the field, and `crates/abi` had no named constant
for row 5 — the table's `t[5]` simply stayed at the all-`RLIM_INFINITY` default, so the honest
answer was there but unreachable from the file. A reader comparing field 25 to
`getrlimit(RLIMIT_RSS)` saw `0` against `18446744073709551615`; after the reader lowered its own
soft limit the field still said `0`. The wrong shape is not merely a wrong number: a `0` ceiling
claims the process may hold no resident pages at all, which is a promise about resource
accounting this kernel does not even implement.

The fix is four small edges, all pointing at the one row:

- `crates/abi/src/lib.rs` gains `pub const RLIMIT_RSS: usize = 5;` beside
  `RLIMIT_SIGPENDING`, with the `q.c` reading and the header's index in its doc.
- `TaskSnapshot` gains `pub rsslim: u64`, and `snapshot_of` fills it from
  `task.rlimits[crate::abi::RLIMIT_RSS].cur` in the same `with_table` look that already reads
  `sig_pending_limit`. The row is the *task's*, not a global default and not the reader's.
- the host mirror `Process` gains the same field (its `process(...)` fixture defaults it to
  `RLIM_INFINITY`, because an ordinary process's field is the infinity word, not zero).
- `stat_line` renders field 25 through `rsslim_word(limit) -> String { format!("{limit}") }`.
  It is a pure function of the row because a host build's `task()` returns `None` and
  `stat_line` itself cannot be unit-tested (the same shape as §6.109's `vm_lines` and §6.110's
  `fd_size`/`sigq_line`).

Host test `vfs::procfs::tests::rsslim_is_the_u64_row_not_a_literal_zero` holds the three readings
above against three different limits (`u64::MAX`, 4 MiB, 8 MiB) and asserts the fixture's default
is the infinity word — a constant answer fails every line.

Pin `user/tests/src/bin/t-stat-rsslim.rs` — six claims, parsed with a `u64` field reader (the
`i64` helper in `t-starttime.rs` cannot hold the infinity word):

```
stat prints field 25 as a u64 word
field 25 is this task's RLIMIT_RSS soft limit (Some(18446744073709551615) against Some(18446744073709551615))
lowering the soft limit to 4 MiB moves field 25 with it (Some(4194304) against Some(4194304))
a child's own ceiling is the field in its file (Some(2097152), child Some(6))
and the parent's file keeps the parent's ceiling (Some(4194304) against Some(2097152))
restoring the ceiling moves field 25 back to the infinity word (Some(18446744073709551615))
```

The child claim is the one `/proc/self/stat` cannot make: a forked child lowers *its own* soft
limit to 2 MiB and its `/proc/<child>/stat` must read `2097152` while the parent's still reads
`4194304`. A fix that read the startup default instead of the task's stored table would pass the
first two claims and fail the third; a fix that read the reading task's row would fail the child
claim. Host (`MPOS_ON_HOST=1`) 6/6 `PASS`; fixed kernel (`build/boot-rsslim1.log`, kernel **and**
loader rebuilt) 6/6 `PASS`, `exit=0`.

Counterfactual (`git stash push -q -m cf-rsslim --` the three source files, kernel **and** loader
rebuilt, `build/boot-rsslim-cf1.log`) — five reds, only the shape claim green:

```
FAIL:  field 25 is this task's RLIMIT_RSS soft limit (Some(0) against Some(18446744073709551615))
FAIL:  lowering the soft limit to 4 MiB moves field 25 with it (Some(0) against Some(4194304))
FAIL:  a child's own ceiling is the field in its file (Some(0), child Some(6))
FAIL:  and the parent's file keeps the parent's ceiling (Some(0) against Some(0))
FAIL:  restoring the ceiling moves field 25 back to the infinity word (Some(0))
FAILED
```

`git stash pop` + rebuild + re-boot (`build/boot-rsslim2.log`) returns to 6 `ok:`/`PASS`.

**The rule.** A field that names a resource limit is a promise about *which* row and *whose*
row it reads, not merely a plausible-looking number. Two probes paid for the same lesson: the
first `p.c` let `atol` turn `u64::MAX` into `-1` and momentarily looked like Linux's answer, and
a pin that only read `/proc/self/stat` cannot distinguish "the task's row" from "a default",
because there the task and the reader are one. Render the row through a leaf the kernel already
charges against, and give the claim a second task with a different row.

### 6.112 `stat`'s `exit_code` was a literal zero, so a zombie looked like a live task (LANDED 2026-11-07)

`/proc/<pid>/stat`'s field 52 is `exit_code`, but it is **not** a raw exit code: it is the encoded
`wait4` status word of the zombie, and `0` while the task (or its group) is still alive. Measured
on the host, Linux 7.2.14 (`build/scratch/statmore/q.c`, `ulimit -c 0`, field 52 read next to the
`waitpid` status of the same child):

```
exit(42)          field52=10752  wait_status=10752  SAME=yes
SIGTERM           field52=15     wait_status=15     SAME=yes
SIGKILL           field52=9      wait_status=9      SAME=yes
SIGSEGV (core)    field52=139    wait_status=139    WTERMSIG=11 WCOREDUMP=128
SIGABRT (core)    field52=134    wait_status=134    WTERMSIG=6  WCOREDUMP=128
```

The `0x80` core bit follows the signal and the process's dumpability, not whether a core file was
actually written (`src/syscall/linux/abi.rs`'s old `wait_status` doc recorded the same measurement
against `.workbuddy/probe_wcore_dumpable.c`). Two more readings bound the "alive" half
(`build/scratch/statmore/r.c`, `s.c`): a live task reads `0`; a group whose leader has exited
while a sibling still runs still reads `0` (`waitpid` eventually answers `0`); and only after the
whole group dies of `SIGTERM` does the zombie's field read `15`. A first probe
(`build/scratch/rsslim/p.c`) parsed the `u64` token with `atol` and turned `u64::MAX` into `-1` —
that overflow was the probe's, not Linux's, which is why the new pin parses field 52 as `u64`.

This kernel printed `Cow::Borrowed("0")` for the field on every task. The "before" reading
(`build/boot-statmore-before.log`) makes the wrongness concrete:

```
self (live):              state=[R] field52=[0
child exit 42 (zombie):   state=[Z] field52=[0
waitpid: pid=6 status=10752
```

The two tasks are indistinguishable in the file even though `waitpid` already had the word. The
word existed only inside `wait4`'s encoder, `src/syscall/linux/abi.rs::wait_status`, reachable
from `vfs::procfs` only by adding a `vfs → syscall` edge — an edge the architecture record
(`docs/ARCHITECTURE.md` §2/§3, the `vfs ↔ sched ↔ syscall ↔ net` cluster) already lists as a
violation, not a licence for another one. The fix therefore moves the encoding *down*:

- `crates/abi/src/lib.rs` gains `pub fn wait_status(code: isize, dumped: bool) -> i32` — the one
  definition of the word, in the leaf any layer may call downward; a host test
  (`wait_status_is_the_word_stat_and_waitpid_agree_on`) pins `10752/15/9/139/134`, `-11 false →
  11`, `3 true → 768`, and the low-byte truncation (`256 → 0`, `257 → 256`).
- `src/syscall/linux/abi.rs` replaces its own `wait_status` body with `pub use
  mpos_abi::wait_status;`. `wait4` and `/proc/<pid>/stat` now read the same definition, so the
  two doors cannot drift; the existing host test in that file still reaches it through
  `use super::*`.
- `src/sched/runtime/child_exit.rs` gains `pub fn retired_status(id: TaskId) -> Option<i32>`.
  It reads the whole `Retired` record — `code` **and** `dumped` — without consuming it (reaping
  still owns the record), and returns `crate::abi::wait_status(code, dumped)`. The old
  `exit_code(id) -> Option<isize>` accessor was not enough: it yields the raw `code` (`42`, or
  `-11`) and drops the core bit, which is the difference between `139` and `11`.
- `TaskSnapshot.exit_code: i32` is filled by `snapshot_of` from
  `retired_status(task.tgid).unwrap_or(0)`: a zombie leader's slot survives until reap, so
  `snapshot(pid)` still finds it; a live task has no `Retired` record and answers `0`. The
  nested `critical_section::with` is the shape `snapshot_of` already uses to read `mm.frames`.
- `src/vfs/procfs.rs` renders field 52 through the host-testable pure function
  `fn exit_code_word(status: i32) -> String` (0 → "0", 42 → "10752", `-15` → "15", `-9` → "9",
  `-11 true` → "139", `-11 false` → "11"), with a host test asserting those are six *different*
  words. `stat_line` itself is machine-only (the host `task()` returns `None`), so this is the
  same extraction §6.109-§6.111 used.

Pin `user/tests/src/bin/t-stat-exitcode.rs` (11 claims). Each death forks a child that resets
dispositions, unblocks the signal, announces readiness on a pipe, dies, and is read *before*
reaping; the pin asserts the absolute Linux word **and** its equality with the `waitpid` status
that finally collects it, and that the living parent's own field stays `0`:

```
ok:    a live task's field 52 is zero (Some(0))
ok:    exit(42) leaves 10752 in the zombie's field 52 (Some(10752))
ok:    and that is the word waitpid reports (Some(10752) against 10752)
ok:    while the living parent's own field 52 stays zero (Some(0))
ok:    SIGTERM leaves 15 in the zombie's field 52 (Some(15))
ok:    and that is the word waitpid reports (Some(15) against 15)
ok:    SIGKILL leaves 9 in the zombie's field 52 (Some(9))
ok:    and that is the word waitpid reports (Some(9) against 9)
ok:    SIGSEGV leaves a signal-death word in field 52 (Some(139))
ok:    and that is exactly the word waitpid reports (Some(139) against 139)
```

Host `MPOS_ON_HOST=1` and the rebuilt machine (`build/boot-exitcode1.log`) both answer 11/11
`PASS`. Counterfactual: stashing the seven source files and rebuilding kernel **and** loader
(`build/boot-exitcode-cf1.log`) gives eight red — every death claim reads `Some(0)` against the
`waitpid` word, while the two "live"/shape claims stay green:

```
FAIL:  exit(42) leaves 10752 in the zombie's field 52 (Some(0))
FAIL:  and that is the word waitpid reports (Some(0) against 10752)
FAIL:  SIGTERM leaves 15 in the zombie's field 52 (Some(0))
FAIL:  and that is the word waitpid reports (Some(0) against 15)
FAIL:  SIGKILL leaves 9 in the zombie's field 52 (Some(0))
FAIL:  and that is the word waitpid reports (Some(0) against 9)
FAIL:  SIGSEGV leaves a signal-death word in field 52 (Some(0))
FAIL:  and that is exactly the word waitpid reports (Some(0) against 139)
FAILED
```

`git stash pop` + rebuild + re-boot (`build/boot-exitcode2.log`) returns to 11 `ok:`/`PASS`.

**The rule.** A field that names a status word is a promise about the *encoding*, not about a
number that happens to fit the column. Three easy implementations all fail some claim above: a
literal `0` (the old kernel), the raw `exit()` code (`42` instead of `10752`), or the signal
number with the core bit dropped (`11` instead of `139`). The encoding already existed in the
kernel, but on the far side of a layer boundary; copying it would have made two doors that can
disagree, and calling across the boundary would have added a forbidden edge. Move the one
definition down to the leaf, and let both doors read it.

### 6.113 `getrusage`'s `ru_maxrss` was a literal zero, beside a CPU pair it counted (LANDED 2026-11-08)

`struct rusage` is 144 bytes: two `timeval`s of CPU (offsets 0-31), then fourteen
`long` counters. The third `long`, at **offset 32**, is `ru_maxrss` — the
process's resident-set high-water mark in **KB**. §6.105 landed the last two
counters of that tail (`ru_nvcsw`/`ru_nivcsw`, offsets 128/136) and named the
rest a gap: `ru_maxrss` had "no high-water record". §6.109 then built exactly
that record for `/proc/<pid>/status`'s `VmHWM` (`Mm::hwm_frames`), so the gap
became closable. What was missing was not the measurement but the *wiring* from
that one record to the three `getrusage` readers and the two `wait` doors.

Measured on Linux 7.2.14, musl static, in `build/scratch/maxrss/`. The first
probe (`p.c`) ran in an already-existing process and was useless: this
container's own `ru_maxrss` baseline is ~1.09 GB, so an 8 MiB mapping never
moved it and `RUSAGE_CHILDREN` was pinned by an older, larger child. Every
decisive probe forks a **fresh** process (its counters start at zero) and uses
one call per statement with `setvbuf(stdout,0,_IONBF,0)` (§6.92's printf
argument-evaluation trap paid for again):

```
q.c (fresh child)   baseline SELF=0 THREAD=0
                    mapped 8 MiB (untouched)  SELF=0        <- mapping is not residency
                    touched 8 MiB             SELF=8204 THREAD=8204   (KB, ~8*1024+4)
                    after munmap              SELF=8204     <- a true peak, never lowers
                    wait4(child) ru_maxrss=8204
r.c (grandchild)    C1 own SELF=2060, reaped a grandchild of peak 8204
                    wait4(C1) ru_maxrss=8204   <- RUSAGE_BOTH: own OR children, the MAX
u.c (fresh worker)  CHILDREN=0 -> BIG(8MiB) wait4=8204 CHILDREN=8204
                    -> SMALL(0) wait4=0 CHILDREN=8204   <- never lowers
                    -> REAPER(own 1MiB, grand 8MiB) wait4=8204 CHILDREN=8204
s.c (fork)          a child inherits the parent's *current* peak: parent touched
                    32 MiB (VmHWM 32868) -> child fresh ru_maxrss=32776 VmHWM=32864
t.c (munmap dip)    fresh child ru_maxrss=7900 stays 7900 across map/touch/unmap,
                    while VmHWM goes 88 -> 4184 -> 4112: the two fields are NOT
                    the same number, and ru_maxrss does not dip.
```

So there are **two books**, not one. A process has its own peak (what `RUSAGE_SELF`
and `RUSAGE_THREAD` read in a single-threaded process). `RUSAGE_CHILDREN` is the
**MAX** of the peaks of every child it reaped — not their sum, not the last one,
and a smaller new child never lowers it (`u.c`). `wait4`'s rusage is
`RUSAGE_BOTH` for that child: its own peak OR the largest peak among the children
*it* reaped, whichever is greater (`r.c`: 2060 own, 8204 reported). A fork
inherits the parent's current high-water mark (`s.c`), which is why a fresh
child of a big process is not zero — and why the pin must use size relations
and a 32 MiB touch with a half-of-it margin instead of absolute numbers.

The fix, in five moves:

- `src/sched/runtime/child_mm.rs`: `Mm` gains `cmaxrss: AtomicU64` (the MAX of
  reaped children's peaks) beside the `hwm_frames` §6.109 already kept.
  `maxrss_kb()` converts that frame count to KB; `maxrss_all()` returns
  `(own, reaped_children)`; `maxrss_both()` is `RUSAGE_BOTH` (their max);
  `absorb_child_maxrss()` is a `fetch_max`, not an add. The three **pure**
  derivations live in the `crates/abi` leaf as `rusage_maxrss`,
  `rusage_both`, `fold_child_maxrss` — `proc.rs` is machine-only, so a host
  test could not otherwise reach them.
- `src/sched/runtime/task.rs` / `child_exit.rs`: `Retired` and
  `WaitEvent::Exited` carry `maxrss` (read once at death, while the `mm` is
  still reachable — the same reason §6.105 carried `nvcsw`/`nivcsw`). The
  reap fold in `Table::release_exited` pushes that `maxrss_both()` into the
  parent's `cmaxrss` **as a MAX**, beside the CPU/switch pairs' sums.
- `src/sched/runtime/child_signal.rs` / `mod.rs`: `current_maxrss()` returns
  the two books; `maxrss_of(id)` reads a live task's `maxrss_both()` for the
  stop/continue wait doors (the `switches_of` shape). A host stub returns
  `(0, 0)` — the host has no resident record, and zero is the honest answer.
- `src/syscall/linux/proc.rs`: `write_rusage` takes `maxrss_kb` and writes
  `wire[32..40]` (leaving the other ten counters an honest zero). All four
  wait doors — `finish_wait_event`'s death arm, its two live arms,
  `take_retired_current`'s direct `finish_wait`, and `waitid`'s `Reported` —
  carry the field; `handle_getrusage` splits it three ways (SELF/THREAD → own,
  CHILDREN → reaped) through the leaf's `rusage_maxrss`.
- The first bare-metal build failed with
  `error[E0425]: cannot find function rusage_maxrss in module abi` because
  `proc.rs`'s `abi::` is `super::abi` (`src/syscall/linux/abi.rs`), not the
  `crate::abi` leaf; the re-export list gained `rusage_maxrss`.

Pin `user/tests/src/bin/t-rusage-maxrss.rs` (10 claims, all movements or size
relations, all in freshly forked processes): untouched mmap does not move the
peak, a 32 MiB touch moves it by at least half, `RUSAGE_THREAD` equals `SELF`,
`munmap` never lowers it, `wait4` reports the child's peak, a big child's peak
exceeds a small one's, the big one folds into `RUSAGE_CHILDREN`, and a smaller
child never lowers `CHILDREN`. Host 10/10 `PASS`; machine
`build/boot-maxrss1.log` 10/10 `PASS` (`476 -> 33244 kB`, `CHILDREN 0 -> 33252`).

Counterfactual: `git stash` of all nine source files + rebuild kernel *and*
loader (`build/boot-maxrss-cf1.log`, `build/boot-maxrss-cf2.log`) → four red
(the touch, `wait4`, the big-vs-small comparison, and the CHILDREN fold all read
`0`), `FAILED`, `exit=1`; only the shape and zero-tolerant relation claims stay
green, which is exactly why the strengthened `c1 > 0` was added. `git stash
pop` + rebuild + `build/boot-maxrss2.log` returns to 10/10 `PASS`.
`mpos_abi::tests::rusage_maxrss_keeps_three_readers_and_two_books_apart` pins
the leaf's three functions against a literal-zero implementation. One old
claim had to move with it (§6.105's rule again): `t-wait4`'s "the twelve
unmeasured counters are zero" covered `counters[0]`, which is now served; it is
now "the eleven unmeasured counters" over `counters[1..12]` plus a new claim
that the child's peak is written and positive.

**The rule.** A high-water mark is a record of the past, not a function of the
current count — §6.109's rule applied to a second door. Three tempting
implementations all fail: printing the current `rss_pages` (it drops on
`munmap`, `t.c`), summing children's peaks instead of taking their max (`u.c`),
or reporting a child's own peak only and ignoring the grandchildren it reaped
(`r.c`). `VmHWM` and `ru_maxrss` are *not* the same number — one dips, one does
not — so wiring the two doors to one record is a measurement, not a shortcut.
The remaining ten counters (`ru_minflt`/`ru_majflt`, block I/O, IPC,
`ru_nsignals`) stay an honest, named zero: the fault split needs
`MappedObject::frame_at` to say whether a fault went to a device, which it does
not (§6.105).

### 6.114 the seven `sched_*` doors read a 64-bit register where Linux reads a `pid_t` (LANDED 2026-11-08)

Measured on Linux 7.2.14 (`build/scratch/schedpid/vector.c`, musl static, raw
`syscall()` so the *full 64-bit word* reaches the kernel and the truncation is
the kernel's, not a userspace wrapper's). Each row is the errno of all seven
doors — `sched_setparam`, `sched_getparam`, `sched_setscheduler`,
`sched_getscheduler`, `sched_rr_get_interval`, `sched_setattr`,
`sched_getattr` — for one pid word; `0` means success:

```
-1       setparam=22 getparam=22 setsched=22 getsched=22 rr_int=22 setattr=22 getattr=22
-2       setparam=22 getparam=22 setsched=22 getsched=22 rr_int=22 setattr=22 getattr=22
0        setparam=0  getparam=0  setsched=0  getsched=0  rr_int=0  setattr=0  getattr=0
999999   setparam=3  getparam=3  setsched=3  getsched=3  rr_int=3  setattr=3  getattr=3
2^32     setparam=0  getparam=0  setsched=0  getsched=0  rr_int=0  setattr=0  getattr=0
2^32-1   setparam=22 getparam=22 setsched=22 getsched=22 rr_int=22 setattr=22 getattr=22
```

`build/scratch/schedpid/w.c` pinned the truncation one value at a time
(`4294967296 -> self`, `4294967295 -> EINVAL`, `4294967297 -> pid 1`): the kernel
reads the pid as a **signed 32-bit `pid_t`**. `2^32` truncates to `0` (self) and
`2^32-1` truncates to `-1`, so those two rows exactly repeat `0` and `-1`. A
negative pid is `EINVAL`(-22), a positive number naming nobody is `ESRCH`(-3)
— two different refusals, not one "not found".

**The defect.** `src/syscall/linux/posix.rs`'s `fn live_thread(pid: isize)`
opened with `if pid < 0 { return None; }`, but every one of the seven handlers
called it with `args[0] as isize` — a 64-bit register value. A userspace `-1`
arrives as `0xffff_ffff_ffff_ffff`, which is a huge *positive* `isize`, so the
`pid < 0` guard was dead code and `resolve_pid` was handed a number that named
nobody. All seven doors answered `ESRCH`(-3) for `-1`, `-2` and `2^32-1`, and
`2^32` (which Linux reads as self) also went to `resolve_pid` as "nobody" and
answered `ESRCH` instead of success. §6.91 had already fixed these doors to
resolve a number *in the caller's namespace*; that fix kept the 64-bit read,
which is why the negative-pid shape stayed wrong.

**The fix is one shared verdict, not seven patches.**
`src/syscall/linux/abi.rs` (host-compiled, so it can be unit-tested) gains
`pub(crate) fn sched_pid_verdict(word: usize) -> Result<i32, isize>`: truncate
to `i32` first, `Err(err::EINVAL)` for a negative result, else `Ok(pid)`.
`posix.rs`'s `live_thread` becomes `fn sched_target(word: usize) -> Result<usize,
isize>`: it asks that verdict, maps `0` to `current()` and a positive number
through `resolve_pid` (ESRCH when nobody answers), and the seven handlers now
start with `if let Err(e) = sched_target(word) { return e; }` (or bind `target`
for the two `sched_*attr` doors that need it). Every handler's existing
buffer/policy check order is left exactly as it was. The host unit test
`a_sched_pid_is_a_signed_32_bit_word_and_negative_is_einval` pins the verdict
against `-1/-2/i32::MIN/0xffff_ffff -> EINVAL`, `0/1/999999 -> Ok`, and the two
truncations `2^32 -> 0`, `2^32-1 -> EINVAL`; a literal-ESRCH implementation
would be red.

Pin `user/tests/src/bin/t-sched-pid.rs` (6 claims, one per pid word): it calls
the **raw** syscalls with the full 64-bit word — a libc wrapper such as
`sched_getparam(pid as i32, ..)` would truncate in userspace and hide the bug —
and asserts all seven doors answer the same errno. Host 6/6 `PASS`; machine
`build/boot-schedpid1.log` 6/6 `PASS` (`-1/-2/2^32-1` all `[22 * 7]`,
`999999` all `[3 * 7]`, `0/2^32` all success). Counterfactual: `git stash` of
`abi.rs` + `posix.rs` + rebuild kernel *and* loader
(`build/boot-schedpid-cf1.log`) → three red rows reading `[3 * 7]` where Linux
answers `22`, `FAILED`, `exit=1`; `git stash pop` + rebuild +
`build/boot-schedpid2.log` returns to 6/6 `PASS`.

**The rule.** A syscall argument is the width its ABI says it is. Reading a
64-bit register where Linux reads a `pid_t` did not just mis-answer `-1`: it
made a whole guard unreachable and made `2^32` name a different task than Linux
names. When the same wrong shape appears in seven handlers, the fix belongs in
the one function they all share, and that function belongs host-side where its
truncation can be pinned. The refusal-*ordering* differences between the doors
(`sched_getparam` checks its buffer before the pid, `sched_rr_get_interval`
checks the pid first — `build/scratch/schedattr/shapes.c`) are a separate named
gap; this change deliberately keeps every handler's existing order and the pin
uses only well-formed buffers. `sched_rr_get_interval`'s slice value itself is
also still host-configuration-dependent (§6.114's own measurement: 2 ms on
SCHED_OTHER, 100 ms on SCHED_RR, 0 on SCHED_FIFO) and remains a named gap —
refuse-not-approximate, not a value to guess.

### 6.115 the five buffered `sched_*` doors check the buffer before the pid (LANDED 2026-11-08)

§6.114 closed the negative-pid half of the `sched_*` family and deliberately
left its counterpart named: the *order* in which the doors refuse. Measured on
Linux 7.2.14 (`build/scratch/schedorder/matrix.c`, `edges.c`, `ambig.c`,
`sizes.c`, musl static, raw `syscall()` so no userspace wrapper pre-validates
the buffer). Five doors take a userspace struct — `sched_getparam`,
`sched_setparam`, `sched_setscheduler`, `sched_getattr`, `sched_setattr` —
and share one **first** guard, literally `if (!uattr || pid < 0) return
-EINVAL;`, before the pid is ever looked up:

```
                    NULL buffer      valid buffer     bad (0x1) ptr
self   (pid 0)       EINVAL (22)     success (0)      EFAULT (14)
nobody (999999)      EINVAL (22)     ESRCH (3)        (door-specific)
negative (pid -1)    EINVAL (22)     EINVAL (22)      EINVAL (22)
```

The **NULL + nobody → EINVAL** row is the ordering claim: the failed pid
lookup's `ESRCH` must not win over the NULL buffer's `EINVAL`. `edges.c`
adds that the guard runs before the policy is examined —
`sched_setscheduler(0, SCHED_FIFO, NULL)` is `EINVAL`, not `EOPNOTSUPP` — and
that a bad *non-NULL* pointer is still `EFAULT`, not the NULL guard's
`EINVAL`. `sched_rr_get_interval` is the deliberate **exception**: it has no
such guard, so its pid lookup comes first (`rr_get_interval(0, NULL)` is
`EFAULT`, `rr_get_interval(999999, NULL)` is `ESRCH`). §6.114 had already
kept that pid-first order for the exception door, so this fix must not be
"apply the guard to all seven" — only the five buffered ones.

**The defect.** After §6.114 every handler called `sched_target(word)` first
and its `buffer_ok` check second. On the five buffered doors that order made
a NULL buffer fall through to `buffer_ok`, which answers `EFAULT` on self and
never gets a chance on a nonexistent pid (the `sched_target` before it had
already answered `ESRCH`). MPOS before: `NULL + self` → `[14 × 5]`,
`NULL + nobody` → `[3 × 5]`, `setscheduler(0, FIFO, NULL)` → `EOPNOTSUPP`
(95), while Linux answers `EINVAL` for all three. The boot of the new pin on
the unfixed tree (`build/boot-schedorder-before.log`, and again
`build/boot-schedorder-cf1.log` after the stash) recorded exactly those
reds: three claims `FAILED`, `exit=1`.

**The fix is one shared verdict reused by five doors, not five patches.**
`src/syscall/linux/abi.rs` (host-compiled) gains
`pub(crate) fn sched_buffered_pid_verdict(word: usize, buffer: usize) ->
Result<i32, isize>`: it asks §6.114's `sched_pid_verdict` (truncation +
negative→EINVAL), then adds `buffer == 0 → EINVAL` **before** returning the
pid. `posix.rs` gains `sched_buffered_target(word, buffer)`, which resolves
the returned pid exactly as `sched_target` does, and the five handlers now
open with `if let Err(e) = sched_buffered_target(word, param) { return e; }`
(the two `sched_*attr` doors bind `target`). `sched_getscheduler` (no buffer)
and `sched_rr_get_interval` (the exception) keep `sched_target` unchanged.
The host unit test
`a_sched_null_buffer_is_einval_before_the_pid_is_looked_up` pins the verdict
against an implementation that only runs `sched_pid_verdict` — the two
`Err(EINVAL)` NULL-buffer rows fail without the buffer check.

Pin `user/tests/src/bin/t-sched-order.rs` (8 claims): the five buffered
doors on NULL + self / NULL + nobody / NULL + negative, valid-buffer
controls, a bad non-NULL pointer (EFAULT), the NULL-param guard across four
`setscheduler` policies, and the `rr_get_interval` exception's pid-first
control. It calls the raw syscalls with the full 64-bit word. Host 8/8
`PASS`; machine `build/boot-schedorder1.log` 8/8 `PASS`. Counterfactual:
`git stash` of `abi.rs` + `posix.rs` + rebuild kernel *and* loader
(`build/boot-schedorder-cf1.log`) → three red rows (`[14 × 5]`,
`[3 × 5]`, `[14, 95, 95, 22]`), `FAILED`, `exit=1`; `git stash pop` +
rebuild + `build/boot-schedorder2.log` returns to 8/8 `PASS`.

**The rule.** A refusal's *order* is part of its shape, not just its errno.
Two calls can name the same bad buffer and the same bad pid and still owe the
caller different errnos depending on which check the kernel makes first — a
caller branches on EINVAL (fix your argument) differently from ESRCH (the
task is gone). §6.114 fixed the width of the word; this fixed the precedence
of the checks, on the same seven doors, through the same host-side verdict
style. What is *still* a named gap, measured but deliberately out of this
intent: the two `sched_*attr` doors' `size`/`flags` guards have their own
different order (`getattr(size=0, nobody)` → EINVAL, `getattr(flags=1,
nobody)` → ESRCH, `setattr(flags=1, nobody)` → EINVAL), a bad non-NULL
pointer's door-specific EFAULT/ESRCH precedence on a nonexistent pid, the
`usize > PAGE_SIZE` ceiling, and `sched_rr_get_interval`'s host-configured
slice value. One intent per commit: this one is the buffer-before-pid
precedence all five buffered doors share.

### 6.116 the two `sched_*attr` doors' `size`/`flags` words and their guard order (LANDED 2026-11-09)

§6.115 named this as a gap: the five buffered `sched_*` doors share one
NULL-buffer-before-pid guard, but the two `sched_*attr` doors have their own
`size` and `flags` guards with a *different* order. Every measurement here is
Linux 7.2.14 via raw `syscall()` (no wrapper truncates a word or validates a
buffer first), probes in `build/scratch/schedattrorder/` (`q.c`, `r.c` with
`strerror`, `s.c` for the 32-bit truncation, `order.c` for a bad pointer),
`build/scratch/schedorder/edges.c` re-measured to kill a stale reading.
`PAGE_SIZE` is 4096 on both the measuring host and this kernel
(`crates/vm/src/lib.rs:33`). The errno words: EINVAL 22, ESRCH 3, EFAULT 14,
**E2BIG 7** (`Argument list too long`, `r.c`), EOPNOTSUPP 95.

```text
sched_getattr(pid, attr, usize, flags)   [usize is the ARG, NOT attr.size]
  usize 0/1/4/47/4097   -> EINVAL(22) on self, nobody(999999) and pid -1
                            ALIKE: the size guard beats the pid lookup.
  usize 48/56/4096      -> self 0, nobody ESRCH(3)
  flags=1 usize=48      -> self EINVAL(22), nobody ESRCH(3), pid -1 EINVAL
                            => the LOOKUP comes first, flags AFTER it.
  usize=0 flags=1        -> EINVAL on everyone: size beats flags.
  flags=1 pointer=0x1    -> EINVAL (flags before the copy); flags=0 -> EFAULT.
  2^32+48 / flags=2^32   -> served; 2^32+0 / 2^32+1 -> EINVAL
                            => both words are read as 32-bit unsigned.

sched_setattr(pid, attr, flags)          [size is attr.size IN the struct]
  flags=1                -> EINVAL(22) on self, nobody AND pid -1
                            => here flags is checked BEFORE the lookup.
  attr.size 4/47/4097    -> E2BIG(7) on self AND nobody (NOT EINVAL, and
                            it beats ESRCH); attr.size 4096 -> served.
  attr.size 0            -> self 0 (0 means "use the kernel's version"),
                            nobody ESRCH(3); it is NOT refused as too small.
  attr.size 48/56/4096   -> self 0, nobody ESRCH(3)
  flags=2^32             -> served (truncates to zero); 2^32+1 -> EINVAL.
```

An `edges.c` contradiction ("in-struct size 4 still served") was a probe
artifact: an earlier `getattr` call wrote `size = 48` back into the same
buffer, so `setattr` then read 48, not 4. §13.15's rule applied: re-measure
with a freshly `arm(size)`-ed 64 KiB buffer, one syscall per statement, errno
stored before printing. The `q/r/s/order.c` readings above are the authority.

**MPOS before.** Both handlers called `sched_buffered_target` (pid lookup)
first, then compared the *full 64-bit* `size` with `< 48` (EINVAL), had no
`> PAGE_SIZE` ceiling, checked `flags != 0` before the lookup in *both* doors,
and treated `attr.size = 0` as too small. Linux: `size` is 32-bit-bounded by
PAGE_SIZE with E2BIG for short/over-page, `getattr`'s size guard is before the
lookup but its flags guard is after, `setattr`'s flags guard is before the
lookup and its size guard is before the lookup too, and `size = 0` means
"default". A counterfactual boot (`build/boot-schedattr-cf1.log`) of the new
pin against the old handlers gives **8 red** of 12 claims — the size words,
the E2BIG errno, `size = 0`, the flags precedence, and both truncations all
answer the wrong errno.

**The fix** keeps the pure contract in host-compiled `src/syscall/linux/abi.rs`
(the verdict style §6.114/§6.115 established) and leaves `posix.rs` two doors
with only the ordering each door owes:

- `pub(crate) const SCHED_ATTR_SIZE_V1: usize = 48;`
- `pub(crate) fn sched_getattr_usize_verdict(word: usize) -> Result<u32, isize>`
  — `word as u32`; `< 48` or `> crate::mm::pframe::PAGE_SIZE` → `EINVAL`,
  else `Ok(size)`.
- `pub(crate) fn sched_attr_flags_verdict(word: usize) -> Result<(), isize>`
  — `word as u32 != 0` → `EINVAL`.
- `pub(crate) fn sched_setattr_size_verdict(size: u32) -> Result<(), isize>`
  — `0` → `Ok` (default), `0 < size < 48` or `size > PAGE_SIZE` → **`E2BIG`**,
  else `Ok`.
- `fn sched_resolve(vetted: i32) -> Result<usize, isize>` in `posix.rs` is the
  single lookup both `sched_target`/`sched_buffered_target` and the attr doors
  now share (`0` → current, positive → `resolve_pid`, else `ESRCH`), so the
  guard order can be spelled once per door without duplicating the lookup.
- `sched_getattr`: NULL/pid verdict → `sched_getattr_usize_verdict` → lookup →
  `sched_attr_flags_verdict` → `buffer_ok` → write. `sched_setattr`: NULL/pid
  verdict → `sched_attr_flags_verdict` (before lookup) → `buffer_ok` → read the
  in-struct size → `sched_setattr_size_verdict` (E2BIG, before lookup) →
  `sched_resolve` → policy. The size's E2BIG had to be moved *before* the
  lookup because `q.c` measured pid 999999 + `size = 4` → E2BIG, not ESRCH.

Three host unit tests in `abi.rs`'s `mod diskstat_tests` pin the three verdicts
against 32-bit, PAGE_SIZE and E2BIG shapes; a pid-only or 64-bit implementation
fails them. Host unit tests `749 passed; 0 failed` (three new).

Pin `user/tests/src/bin/t-sched-attr.rs` (12 claims): all the rows above,
called through raw syscalls with the full 64-bit word, a 64 KiB buffer so a
large `usize` cannot overflow the stack, and `MPOS_ON_HOST=1` measuring the
same claims on Linux. Host 12/12 `PASS`; machine
`build/boot-schedattr1.log` 12/12 `PASS`. Counterfactual: `git stash` of
`abi.rs` + `posix.rs` + rebuild kernel *and* loader
(`build/boot-schedattr-cf1.log`) → 8 red rows, `FAILED`, `exit=1`;
`git stash pop` + rebuild + `build/boot-schedattr2.log` returns to 12/12
`PASS`.

**The rule.** A `size` or `flags` word is a *32-bit* word, and which guard runs
first is a promise a caller branches on — EINVAL (fix the argument), E2BIG
(the negotiated struct is the wrong length), ESRCH (the task is gone) and
EFAULT (the pointer) are four different recoveries. Two doors of the same
family put their `flags` guard on *opposite* sides of the pid lookup; only a
measurement on the real kernel says which. Named gaps still open, deliberately
out of this intent: a bad non-NULL pointer's door-specific EFAULT/ESRCH
precedence on a nonexistent pid combined with a short size,
`sched_getparam`'s priority range, `size > PAGE_SIZE` on the *host* when
PAGE_SIZE differs, and `sched_rr_get_interval`'s host-configured slice.

### 6.117 `VmData`/`VmStk`/`VmExe`/`VmLib` and `stat`'s three addresses (LANDED 2026-11-10)

Item 222's named gap, plus the three `stat` fields that were borrowed zeros in
the same block of the file. Both are the same question asked twice: *which
mapping is this page, and which mapping is the image?* — and both were answered
by reading Linux's own source rather than by guessing at a shape.

#### What Linux answers

`fs/proc/task_mmu.c`'s `task_mem`:

```c
text = PAGE_ALIGN(mm->end_code) - (mm->start_code & PAGE_MASK);
text = min(text, mm->exec_vm << PAGE_SHIFT);
lib = (mm->exec_vm << PAGE_SHIFT) - text;
/* ... */
seq_put_decimal_ull(m, "VmData:", data_vm << (PAGE_SHIFT - 10));
seq_put_decimal_ull(m, "VmStk:", stack_vm << (PAGE_SHIFT - 10));
seq_put_decimal_ull(m, "VmExe:", text >> 10);
seq_put_decimal_ull(m, "VmLib:", lib >> 10);
```

`mm/mmap.c`'s `vm_stat_account` books every mapping into exactly one of
`exec_vm`/`stack_vm`/`data_vm`, in that order of precedence, using the three
predicates in `mm/vma.h`:

```c
is_exec_mapping : (flags & (VM_EXEC | VM_WRITE | VM_STACK)) == VM_EXEC
is_stack_mapping: ((flags & VM_STACK) == VM_STACK) || (flags & VM_SHADOW_STACK)
is_data_mapping : (flags & (VM_WRITE | VM_SHARED | VM_STACK)) == VM_WRITE
```

and `fs/binfmt_elf.c` sets the two addresses while walking `PT_LOAD`s:

```c
k = p_vaddr;                 if ((p_flags & PF_X) && k < start_code) start_code = k;
k = p_vaddr + p_filesz;      if ((p_flags & PF_X) && end_code < k)   end_code = k;
/* ... */
mm->start_code = start_code; mm->end_code = end_code; mm->start_stack = bprm->p;
```

So: `startcode` is the **lowest** executable segment's linked `p_vaddr`
(unaligned), `endcode` is the **highest** one's `p_vaddr + p_filesz` (file
bytes, not memory bytes), and `startstack` is the entry `rsp`.

#### The measurements

Fetched from `git.kernel.org/.../torvalds/linux.git/plain/<path>` into
`build/scratch/` (`task_mmu.c`, `binfmt_elf.c`, `mmap.c`, `vma.h`, `mm.h`,
`mm_types.h`, `internal.h`, `mm_inline.h`, `mman.h`), then checked against
probes on Linux 7.2.14.

*`VmData` counts writable private mappings, resident or not* -
`build/scratch/vmlines/f.c`, one probe per perturbation:

| change | `VmData` | `VmStk` | `VmSize` |
|---|---|---|---|
| at rest | 1228 kB | 132 kB | 4000 kB |
| `mmap(1)` private rw | **1232** (+4) | 132 | 4000 |
| `mmap(4097)` private rw | **1236** (+8) | 132 | 4000 |
| `alloca(1 MiB)` and touch it all | 1228 (unmoved) | **1032** (+900) | **4900** |

The `alloca` row is the one that separates the books: a stack that grew by 900
kB moved `VmStk` and left `VmData` alone. Page granularity, not bytes.

*`VmData` does not count shared, and `VmStk` does not follow `MAP_STACK`* -
`build/scratch/vmlines/b.c` and `c.c`, 2 MiB each:

| mapping | `VmData` | `VmLib` |
|---|---|---|
| anon `PRIVATE` rw | 2252 | 1704 |
| anon `PRIVATE` r only | 204 (unmoved) | 1704 |
| anon `PRIVATE` rw **shared** | 204 (unmoved) | 1704 |
| anon `PRIVATE` rx | 204 | **3752** (+2048) |
| anon `PRIVATE` rwx | **2252** | 1704 (write beats exec) |
| memfd `PRIVATE` rx | 204 | **3752** |
| memfd **shared** rx | 204 | **3752** |
| `/usr/lib/libz.so.1` private rx | 204 | **3752** |
| anon rw with `MAP_STACK` | 2252 | 1704 (`VmStk` unmoved) |

*`VmExe` is a span, not a sum.* Two executable `PT_LOAD`s and a custom linker
script (`build/scratch/vmlines/*.ld` + `PHDRS`; two `.text.*` sections get
merged into one segment by `ld`, so the probes use two distinct section names):

| probe | segments | `VmExe` | `VmLib` |
|---|---|---|---|
| B | one page at `0x400000` | 4 | 8 |
| A | two adjacent pages at `0x400000` | 8 | 8 |
| D | one page at `0x400000`, two at `0x500000` | **20** | 0 |
| C | one page at `0x400000`, six at `0x500000` | **36** | 0 |
| twos | one page at `0x400000`, one at `0x500000` | **16** | 0 |

`twos` is the discriminator: two pages of text reporting 16 kB rules out
"sum the segments' pages". It is the span from the page the first segment
starts in to the page the last one ends in, clamped to `exec_vm` — which is
also why `VmLib` is 0 there (`text` clamps to the whole book).

*`endcode` is file bytes, not memory bytes* - the decisive probe is a single
`RWE` `PT_LOAD` with a 1 MiB `.bss` inside it, `build/scratch/statcode/x.S`
linked with `-N`:

```
LOAD 0x000158 0x400158 FileSiz 0x640 MemSiz 0x100648 RWE
startcode = 0x400158   endcode = 0x400798   endcode - startcode = 0x640
```

`0x640` is `FileSiz`. With `MemSiz` the difference would have been
`0x100648`.

*`startstack` is the entry `rsp`, exactly* - `build/scratch/statcode/start.S`
saves `%rsp` before anything else touches it:

```
saved_sp  = 0x00007ffe289e0f20
startstack = 0x00007ffe289e0f20   delta 0
```

and inside `main` the live `rsp` was 5024 bytes below it, so the field is
neither the current pointer nor the `[stack]` mapping's boundary.

*fork/exec/zombie* - `build/scratch/statcode/inherit2.c`, `zombiev/z.c`:

| state | `startcode` | `endcode` | `startstack` |
|---|---|---|---|
| parent | `0x55cdb2efc000` | `0x55cdb2efc54d` | `0x7fff5738d870` |
| child after `fork` | same three | | |
| child after `exec` | the new image's | | |
| uncollected zombie | `0` | `0` | `0` |

and a zombie's `/proc/<pid>/status` has **no** `Vm*`/`Rss*` line at all —
`State:` is followed directly by `Threads:`, because Linux reaches `task_mem`
only through an `mm` that still exists.

#### The code

The bookkeeping is deliberately *recounted* rather than *incremented*. This
kernel keeps an address space in two tables — `Mm::vmas` for what can fault and
`Mm::eager` for what was filled at load — and a counter would need six mutation
sites kept in step with both. Linux can afford `vm_stat_account` because every
mapping there is one `vm_area`; here a recount costs what `Mm::mapped_pages`
already costs over the same two tables and cannot drift from them.

- `src/mm/vma.rs` — `pub fn vm_book(prot, shared, name) -> VmBook` carries the
  three predicates and their precedence; `ImageBounds { start_code, end_code,
  start_stack }` carries the three addresses as one value, so no construction
  site can supply two of them and forget the third.
- `src/sched/runtime/child_mm.rs` — `Mm::vm_books() -> (exec, stack, data)`
  walks both tables once; `Mm::start_code`/`end_code`/`start_stack` are
  `AtomicUsize` records; `Mm::with_image` takes the bounds and `with_eager`
  forwards a default, so every existing construction site is unchanged.
- `src/sched/runtime/load.rs` — `load_user` grew a `user_rsp` argument (the
  same number the argument block is written at, because `startstack` *is* that
  number) and computes `start_code`/`end_code` with `fs/binfmt_elf.c`'s own
  loop; `Loaded` carries the bounds.
- `src/sched/runtime/child_exit.rs` — `fork` copies the parent's bounds
  (`copy_mm`), `exec` replaces them.
- `src/sched/runtime/procfs.rs` — `TaskSnapshot` grew `image` and `books`; a
  zombie gets `ImageBounds::default()`.
- `src/vfs/procfs.rs` — the four lines, the zombie's empty block, and the two
  pure renderers: `text_and_lib_kb(bounds, exec_pages)` (the `task_mem` split,
  clamp and all) and `address_word`. `stat_line` was split into `stat_fields`
  + `stat_text` while doing this, because it read a task through the machine's
  snapshot and so no host test could reach a single field it printed — which is
  how three of them stayed borrowed zeros.

#### The pins

`user/tests/src/bin/t-stat-bounds.rs` (10 claims) and
`user/tests/src/bin/t-status-vm.rs` (12 claims), both runnable on the host with
`MPOS_ON_HOST=1`.

`t-stat-bounds`: the three fields are addresses; the end is above the start;
both are in the user half; `startcode` is inside the page `/proc/self/maps`
gives the executable segment (an independent witness from a different part of
the kernel); the entry `rsp` is at or above the live one and inside
`[stack]`; **8 MiB more address space moves none of the three**; a forked child
reports its parent's three; a zombie reports `(0, 0, 0)` while its parent still
reports its own.

`t-status-vm`: the four lines are counts; none exceeds `VmSize`; `VmStk` is a
stack's size; one private writable page moves `VmData` by exactly 4 kB and
leaves `VmStk`; a non-writable mapping moves nothing; a shared one moves
nothing; **an executable mapping outside the image grows `VmLib` by 4 kB and
leaves `VmExe` alone**; a zombie's status has no `Vm*` line while a live
process does.

Host: 10/10 `PASS` (`build/boot-statbounds1.log` on the machine too — 10/10,
`exit=0`) and 12/12 `PASS` (`build/boot-statusvm1.log`, `exit=0`).

Counterfactual (`git stash push` of all seven files, kernel *and* loader
rebuilt — `build/boot-statbounds-cf1.log`, `build/boot-statusvm-cf1.log`):

| pin | red without the fix | green with it |
|---|---|---|
| `t-stat-bounds` | 6 red (`the three fields are addresses, not zeros`, `the text's end is above its start`, `the start is in the page maps gives…`, `the entry stack pointer is at or above…`, `and it is in the stack's mapping…`), `FAILED`, `exit=1` | 10/10 `PASS`, `exit=0` |
| `t-status-vm` | 7 red (`the four mapping lines are counts, not zeros`, `the stack book is a stack's size…`, `a private writable page is one more…`, `an executable mapping outside the image…`, `a zombie's status carries no Vm line`, …), `FAILED`, `exit=1` | 12/12 `PASS`, `exit=0` |

Two claims stayed green in both — `a forked child reports its parent's three`
and `eight more megabytes … move none of the three` — because a kernel that
prints zeros for everything satisfies "the child matches the parent" and
"nothing moved". That is why the pair that *does* move is in both pins, and why
the zombie's zero row is paired with a live row in the same claim set.

**Named gaps left deliberately open**, each an honest `0` rather than an
estimate: `VmLck` and `VmPin` (no mlock/pin accounting), `VmPTE` (no page-table
byte count) and `VmSwap` (no swap). `RssAnon`/`RssFile`/`RssShmem` were in this
list — "the frames table does not record what backs a page" — and
**§6.147 / item 264** closed it six days later by recounting the *describing*
VMA and eager-range tables instead; `RssAnon` no longer keeps the whole
resident set. This paragraph records the state at §6.109's landing, not a live
gap.

**The rule.** Two files answered the same question with different shapes — and
the answer was in Linux's source, not in either shape. A count of "mapped
pages" and a count of "pages in each of three books" are different numbers, and
a span is not a sum. Where a rule has a *clamp*, the clamp is part of the rule:
`min(text, exec_vm)` is why a program with scattered segments reports 16 kB of
text rather than a stretched span, and reading the formula without it produces
a number no probe ever produced.

## §6.118 `/proc/<pid>/statm`'s seven fields are four books and three of Linux's zeros

`statm` answered `"<virtual_pages> <rss_pages> 0 0 0 0 0"` — two counts this
kernel keeps and five borrowed zeros, and the two it did keep were the wrong
two: Linux's first field is the address space and its second is the resident
set, but its fourth (`text`), sixth (`data`) and third (`shared`) are books
`/proc/<pid>/status` had just been taught to keep (§6.117). This section is the
reader that *disagrees* with `status` and is right to.

### What Linux answers

`fs/proc/task_mmu.c`'s `task_statm` is four assignments and a return:

```c
unsigned long task_statm(struct mm_struct *mm, unsigned long *shared, unsigned long *text,
                         unsigned long *data, unsigned long *resident)
{
	*shared = get_mm_counter_sum(mm, MM_FILEPAGES) + get_mm_counter_sum(mm, MM_SHMEMPAGES);
	*text = (PAGE_ALIGN(mm->end_code) - (mm->start_code & PAGE_MASK)) >> PAGE_SHIFT;
	*data = mm->data_vm + mm->stack_vm;
	*resident = *shared + get_mm_counter_sum(mm, MM_ANONPAGES);
	return mm->total_vm;
}
```

The seven words the file prints are
`total_vm · (*shared + *resident's anon half) · *shared · *text · 0 · *data · 0`
— `lib` and `dt` are literal zeros in the printer, not books with no pages
(`fs/proc/base.c`'s `ONE("statm", …)` entry names `proc_pid_statm`).

Three of the four live numbers are **not** what `status` prints beside them,
and the differences are all Linux's:

| `statm` field | `status`'s nearest line | the difference |
|---|---|---|
| 4 `text` | `VmExe:` | `task_mem` clamps to the executable book (`min(text, exec_vm << PAGE_SHIFT)`); `task_statm` does not |
| 6 `data` | `VmData:` + `VmStk:` | `data_vm + stack_vm` in one number, where `status` splits the stack out |
| 3 `shared` | `RssFile:` + `RssShmem:` | the file/shmem half of the resident set — a counter this kernel does not keep |

Measured on Linux 7.2.14 before a line was written.

**The span against the clamp** is the decisive one, because it is one `mm`
read through two doors at one instant. A custom linker script
(`build/scratch/vmlines/far.ld`) puts two one-page `PT_LOAD`s at `0x400000` and
`0x900000`:

```
statm:  43 4 2 1281 0 33 0
status: VmExe:  16 kB     VmLib:  0 kB     VmSize:  172 kB
```

`1281` pages is the span `PAGE_ALIGN(end_code) - (start_code & PAGE_MASK)`;
`16 kB` is what remains of the four-page executable book after the clamp. The
same tree's adjacent version (`near.ld`, `0x400000` + `0x500000`) reads
`statm: 43 4 2 257 0 33 0` with the same `VmExe: 16 kB`. A kernel that prints
`VmExe / 4` in field 4 answers 4 for a binary whose text spans 1281 pages, and
a kernel that prints the span in `VmExe:` breaks the split `VmLib` needs.

**The stack is inside `data`.** `build/scratch/statm/d.c` at rest:

```
statm:  744 463 438 1 0 84 0
status: VmData 51 pages  VmStk 33 pages  VmExe 1 page
                           84 == 51 + 33
```

A one-megabyte `alloca` touched moves `data` to `310 = 51 + 259` as `VmStk`
goes 33 → 259 with `VmData` unmoved — the page-count relation
`data == VmData/4 + VmStk/4` is the honest claim, not a number.

**Mapping moves `data`, residency moves `resident`.** A 1 MiB anonymous
`MAP_PRIVATE` mapping, untouched, moves `data` 84 → 340 (256 pages) and leaves
`resident` alone; touching it moves `resident`. That is the same
mapped-vs-resident split §6.103 taught `statm`'s `size`/`resident` pair.

**A zombie is seven zeros** (`build/scratch/statm/z.c`): `task_statm` is
reachable only through an `mm`, and a zombie has none. `status` prints no `Vm*`
line at all for the same reason (§6.117).

### What was wrong here

`src/vfs/procfs.rs`'s `statm_text` was
`format!("{} {} 0 0 0 0 0\n", task.virtual_pages, task.rss_pages)` — five
literal zeros, and its doc comment said so honestly ("the remaining five are a
named gap"), which is why this was a documented gap and not a lie. But the
`docs/notes/` line named the wrong five: `text`, `data` and `shared` became
answerable the moment §6.117 gave the kernel `ImageBounds` and three mapping
books. Only `shared` (a residency counter) and the two fields Linux itself
prints as zeros are still not this kernel's to answer.

### The fix

- `src/vfs/procfs.rs`: `text_span_pages(bounds)` is the expression both
  readers share, computed once — the unclamped span, in pages.
  `text_and_lib_kb` now calls it and clamps, which is the one place the two
  doors differ, and the doc comment on each names the other.
- `statm_text` prints `virtual_pages · rss_pages · 0 · text_span_pages · 0 ·
  (data_pages + stack_pages) · 0`, with `state == 'Z'` short-circuiting to
  seven zeros. `shared` stays `0` as a named gap: no counter here splits the
  resident set into its file-backed and shared half (`docs/uapi/` Item 232).

### The pin

`user/tests/src/bin/t-statm.rs`, 9 claims, green on the host and on the
machine (`build/boot-statm1.log`, `build/boot-statm2.log`, `exit=0`):

- `/proc/self/statm answers seven numbers`
- `lib and the page-table tick are Linux's own zeros`
- `a megabyte of private mapping is 256 more pages of statm data (24 -> 280 pages)`
- `and status's VmData moved too (32 kB -> Some(1056) kB)`
- `statm's data is VmData's pages plus the stack's`
- `statm's text is the image's executable span, in pages (80)`
- `and never fewer pages than status's clamped VmExe reports`
- `statm's size is stat's vsize in pages`
- `a zombie's statm is seven zeros`

The movement claim is written as a **relation and a direction**
(`after > before`, `<= 4096`), not an exact page count, for a measured reason:
the same natively-linked Rust test binary reports `41 -> 299` pages on the host
against the guest's `24 -> 280`, because its own baseline differs. The exact
numbers belong where two readers can be compared to each other — claim five
(`data == VmData/4 + VmStk/4`) holds on both machines and does not hold for
any kernel that prints one book in both places.

Counterfactual (`git stash push` of `src/vfs/procfs.rs`, kernel *and* loader
rebuilt — `build/boot-statm-cf1.log`): **5 red**.

```
FAIL:  a megabyte of private mapping is 256 more pages of statm data (0 -> 0 pages)
FAIL:  statm's data is VmData's pages plus the stack's
FAIL:  statm's text is the image's executable span, in pages (0)
FAIL:  and never fewer pages than status's clamped VmExe reports
FAIL:  a zombie's statm is seven zeros
FAILED
```

The four that stayed green are the honest limit of this pin: the *shape*
(field count, the two Linux zeros, `size` against `stat`'s `vsize`) is
satisfied by the old file too, and `status`'s `VmData` moved because §6.117
already made it real. Only the three fields this commit filled fail without it.

**The rule.** A file that prints a number next to another file's number is
making a claim about how the two agree. `statm` and `status` disagree in three
measured places and agree in the rest, and each disagreement is a Linux
decision with a probe behind it — the clamp, the stack's book, and which half
of residency is counted. Reading "seven fields, five are zeros" as one gap hid
that the three answerable ones were being reported *wrongly* (a resident count
in `data`'s place) rather than not at all.

## §6.119 `mlock`, `munlock`, `mlock2`, `mlockall`: a limit, a bit on the mapping, and a set of refusals

`AGENTS.md` calls it the single worst thing this codebase can do: return
success for something not implemented. The `mlock` family was that, in its
purest form — every well-formed request answered `0`, nothing was pinned, and
no reader could tell the difference. `VmLck` printed the literal `0 kB`,
`smaps`'s `Locked:` was in the `absent` list beside `KSM:` and `SwapPSS:`, and
`src/vfs/procfs.rs`'s own doc comment said so out loud: *"no locked page -
`mlock` is served as the no-op it is, so `Locked` is the truth rather than a
placeholder."* It was not the truth. It was a zero that agreed with a no-op.

### The six answers Linux gives, measured

Everything below was read off a current host (7.2.14, `euid` 1000,
`RLIMIT_MEMLOCK` `8388608/8388608`) with the probes in
`build/scratch/mlk/`, and cross-checked against `mm/mlock.c` fetched from
the kernel tree (`build/scratch/mlock/mlock.c`).

**1. The limit is enforced, and re-locking is free.** `do_mlock` computes
`locked = len >> PAGE_SHIFT`, adds `mm->locked_vm`, and — *only if that
exceeds the limit* — subtracts `count_mm_mlocked_page_nr(mm, start, len)`, the
pages already pinned **inside the range**. So an already-pinned range passes
even at the limit. `acct.c`, 16-page limit: lock 8 pages reads `VmLck 32 kB`;
lock the same 8 again still reads `32 kB` (and answers `0`); lock 8 more
reads `64 kB`; one page past that is `ENOMEM`. `acct2.c`'s `munlock` of a
3-page sub-range takes the count from `32 kB` to `20 kB` — exactly its own
pages, not the whole range's.

**2. Coverage, not "was the start below something".** `apply_vma_lock_flags`
walks the VMA iterator from `start` and fails if `vma->vm_start != tmp` at any
step — a hole anywhere in the range is `ENOMEM`, the whole range being past
every mapping included. This one cost a wrong reading first: `b386` recorded
"start past the last mapping answers 0", and `holes.c` — which reads
`/proc/self/maps` before it concludes anything — showed why. A library mapping
(`/etc/ld.so.cache`, `[vvar]`) had been landing in the range the earlier probe
called empty. With the layout verified: `40..48 starts just past everything`
is `-1 ENOMEM`, and so is a one-byte range there.

**3. Zero length is judged after alignment.** `len = PAGE_ALIGN(len +
offset_in_page(start)); start &= PAGE_MASK;` — so `mlock(mapped, 0)` is `0`,
but `mlock(unmapped + 1, 0)` is `ENOMEM`, because the alignment turns it into
a one-page range that has nothing under it (`z2.c`). And the alignment is
where the overflow lives: `mlock(aligned, SIZE_MAX)` rounds to length 0 and
answers `0` (`ovf.c`).

**4. At the limit's zero, `EPERM` — before any address is looked at.**
`can_do_mlock()` is `rlimit != 0 || capable(CAP_IPC_LOCK)`, and it runs first,
so with the limit at 0 an *unmapped* range answers `EPERM`, not `ENOMEM`
(`ov.c`). `munlock` does not call it at all: unpinning asks for no capability
and checks no limit (`q2.c`: `munlock 8 pages with limit 0` is `0`, while
`mlock 1 page with limit 0` is `EPERM`).

**5. `mlock2`'s one flag is 1, and `mlockall`'s three are 1/2/4.** This kernel
had `MCL_ONFAULT = 0x10` as `mlock2`'s whole flag vocabulary — the number is
`MLOCK_ONFAULT`'s *neighbour*, and it made the pairing exactly backwards:
`mlock2(..., 1)` (Linux's accepted spelling) was `EINVAL` and `mlock2(...,
0x10)` (Linux's refused one) was `0`. `mlockall` refused `0x40` but accepted
`0` and `MCL_ONFAULT` alone, where Linux refuses both: `!flags || flags &
~(MCL_CURRENT|MCL_FUTURE|MCL_ONFAULT) || flags == MCL_ONFAULT` is `EINVAL`.

**6. `MAP_LOCKED` is the same pin through `mmap`, and a pin is not advice's
to take.** `mlk/onf.c`/`mapl2.c`: `mmap(MAP_LOCKED, 8 pages)` arrives with
`VmLck 32 kB` and no separate `mlock`; over the limit it is `MAP_FAILED` with
`EAGAIN` (not `ENOMEM`) and `VmLck` is unchanged. And `madvise`'s two
destructive advices refuse a locked range — `MADV_DONTNEED` and `MADV_FREE` on
a pinned page are `EINVAL`, with the contents intact (`dont.c`), because
`madvise_dontneed_free_valid_vma` puts `VM_LOCKED` in the forbidden set.

There is a seventh, and it is a *non*-answer this kernel now gives honestly:
`fork` inherits **nothing**. `kernel/fork.c`'s `dup_mmap` clears the lock bit
and `mm_init` starts `locked_vm` at 0, so a child reads `VmLck 0 kB` and can
`munlock` a range it never pinned (`fork3.c`/`fork4.c`; the parent's count is
untouched). `MCL_FUTURE` is likewise per-`mm`.

### What changed here

* `crates/abi`: `RLIMIT_MEMLOCK` (row 8) is a constant and `default_rlimits()`
  fills it with Linux's 8 MiB — it had been left at `RLIM_INFINITY`, which is
  the row every `mlock` limit test reads.
* `mm/vma`: `Vma` and `MappedRange` carry `locked: bool` (**the mapping is
  what remembers**, so `fork`'s copy can be cleared by clearing bits, and the
  count is recounted from the bits exactly as `vm_books` recounts the three
  books). `VmaTable::{set_locked, locked_pages, locked_pages_within,
  lock_all, clear_locks}`; `abuts` refuses to merge two descriptions that
  disagree about it, so a `munlock` of a sub-range can split the range back
  out.
* `sched/runtime/child_mm`: `Mm::{set_locked, locked_pages, locked_within,
  lock_everything, unlock_everything, clear_locks, lock_future,
  set_lock_future}` and the free functions the syscall layer calls. `Mm` also
  gained `lock_future: AtomicBool` — `mm->def_vma_flags`'s `VM_LOCKED` bit.
* `syscall/linux/posix`: `lock_bounds` (align, overflow → `ENOMEM`),
  `lock_coverage` (page-by-page `user_mapped`), `lock_admits` (the limit with
  the already-pinned pages subtracted), `mlockall_admits` (`total_vm <=
  lock_limit`), and the real `handle_mlock2`/`handle_mlockall` shapes.
* `syscall/linux/proc.rs`: `MAP_LOCKED` (and `MCL_FUTURE`) pin a mapping as
  it is made, refusing with `EAGAIN` past the limit; `madvise`'s
  `MADV_DONTNEED`/`MADV_FREE` answer `EINVAL` on a pinned range.
* `vfs/procfs`: `VmLck` is `vm_kilobytes(task.locked_pages)` and
  `smaps`'s `Locked:` is the range's size when it is pinned — the two readers
  that were zeros.
* `sched/runtime/child_exit`: the fork path clears the child's lock bits.
* `sched/runtime/procfs`: `TaskSnapshot::locked_pages`, zero for a task with
  no address space.

### Named gaps this leaves

`munlock` on a range this process never locked answers `0` here and `0` on
Linux too, so no gap. What is *not* served: `MADV_DONTNEED_LOCKED` (a
`madvise` flag that lets the advice through a pinned range — it has no
constant in this kernel, so it is `EINVAL` like every unknown advice);
`/proc/<pid>/smaps`'s `Locked:` is per-range but its `VmLck`-equivalent
rollup line is still the zero list; and huge-page pins, which have no mapping
kind here to pin.

### The pin

`user/tests/src/bin/t-mlock.rs`, **25 claims**, every one a movement or a
refusal — never a fixed `VmLck`, which belongs to the test binary's loader
history. Host green (`MPOS_ON_HOST=1`: `Some(0) -> Some(32)`, `Some(32) ->
Some(20)`, `Some(604)`, `16 kB here, wait status 0`), machine green
(`build/boot-mlock2.log`, `build/boot-mlock3.log`, 25 ok, `PASS`, exit 0).

Counterfactual: all eleven files stashed, kernel and loader rebuilt
(`4333 KiB -> 1150 KiB`), booted — **13 of 25 red**
(`build/boot-mlock-cf1.log`):

```
FAIL:  RLIMIT_MEMLOCK is Linux's 8 MiB row (Some(18446744073709551615))
FAIL:  and VmLck grows by those eight pages (Some(0) -> Some(0) kB)
FAIL:  an unaligned address is rounded out, not refused (-1)
FAIL:  and takes off those three pages (Some(0) -> Some(0) kB)
FAIL:  mlock2 accepts MLOCK_ONFAULT, which is 1 (-1)
FAIL:  mlock2 refuses 0x10, which is not a flag (0, errno 22)
FAIL:  mlockall refuses an empty flags word (0)
FAIL:  and VmLck now covers the mapping it pinned (Some(0) kB)
FAIL:  and the mapping arrives pinned (Some(0) -> Some(0) kB)
FAIL:  MADV_DONTNEED on a locked page is EINVAL (0, errno 22)
FAIL:  and the page was not dropped (first byte false)
FAIL:  and a mapping made afterwards is pinned as it is made (Some(0) -> Some(0) kB)
FAILED
```

The twelve that stay green are the honest limit: an unlocked range still
answers `0` to `mlock`, `munlock` still answers `0`, a hole is still
`ENOMEM`, and a child that inherits nothing reads `0 kB` — all four are what a
no-op says too. Only the claims that name *what a pin does* are red.

**The rule.** A no-op that answers success is invisible to its own readers:
`VmLck` printed `0` because nothing was locked, so nothing looked wrong. The
way this family was found is the way it will be found again — by measuring
the *limit* and the *refusals*, not the happy path, because the happy path is
the one thing a stub and a kernel agree on.

---

### 6.120 `/proc/<pid>/stat`'s data, break and string addresses

Fields 45-51 (`start_data`, `end_data`, `start_brk`, `arg_start`, `arg_end`,
`env_start`, `env_end`) were seven literal `Cow::Borrowed("0")` in
`src/vfs/procfs.rs`'s `stat_fields`. They are the other half of §6.117's
`ImageBounds`: the same ELF load knows all of them, and the same `exec`
replaces them.

**What Linux answers, from its own source.** `fs/binfmt_elf.c`'s
program-header walk is one loop with five accumulators:

```c
k = elf_ppnt->p_vaddr;
if ((p_flags & PF_X) && k < start_code) start_code = k;
if (start_data < k) start_data = k;
k = elf_ppnt->p_vaddr + elf_ppnt->p_filesz;
if ((p_flags & PF_X) && end_code < k) end_code = k;
if (end_data < k) end_data = k;
k = elf_ppnt->p_vaddr + elf_ppnt->p_memsz;
if (k > elf_brk) elf_brk = k;
```

then `mm->start_brk = mm->brk = ELF_PAGEALIGN(elf_brk)`. So `start_data` is
the maximum `p_vaddr`, `end_data` the maximum `p_vaddr + p_filesz`, and
`start_brk` the page-aligned maximum `p_vaddr + p_memsz`.

The four string fields come from `create_elf_tables`, which walks one cursor
`p` upward through the strings it just copied: `mm->arg_start = p` before the
`argv` loop, `mm->arg_end = p` after it, `mm->env_end = mm->env_start = p`
immediately after that, and `mm->env_end = p` after the environment loop.
**`arg_end` and `env_start` are the same address**, and the strings run
ascending - `argv` in order, then the environment, then the `AT_EXECFN` path.

**Measured on the host** (Linux 7.2.14, `build/scratch/stataddr/`):

| probe | what it read |
|---|---|
| `a.c` (static) | `startcode=401000 endcode=47fa3d start_data=4b5168 end_data=4baa18 start_brk=4c1000`, `arg_start=…75e arg_end=…762 env_start=…762 env_end=…ff4` |
| `a.c` after `malloc(1 MiB)` + 8 KiB local | **all seven unchanged** - they are a record, not a live view |
| `b.c` | `arg_start == argv[0]`, `env_start == environ[0]`, `env_end - env_start == 2194 == sum(strlen(env)+1)`, `sbrk(0)` (`0x2d4df000`) ≠ `start_brk` (`0x2d4bd000`) |
| `c.c` `./c alpha beta gamma` | `argv[0] < argv[1] < argv[2] < argv[3]` - **ascending**, `arg_start == argv[0]`, `env_start == arg_end` |
| `d.c` | `AT_EXECFN == env_end` (`execfn - env_end = 0`) |
| `e.c` (dynamic) | `start_data=…7dd0 end_data=…8050 start_brk=…9000` - the **program's** loads, not the interpreter's |
| `zombiev/z.c` | a zombie's seven are `0` |

Against `readelf -lW a`'s loads (`0x400000 FileSiz 0x518`, `0x401000
0x7ea3d`, `0x480000 0x344e0`, `0x4b5168 FileSiz 0x58b0 MemSiz 0xb180`) each
of the six image rules reproduces its measured value exactly: `max(p_vaddr) =
0x4b5168`, `0x4b5168 + 0x58b0 = 0x4baa18`, `PAGE_ALIGN(0x4b5168 + 0xb180) =
0x4c1000`.

**The layout this kernel had was not Linux's.** `write_arg_block` wrote the
strings *downward* from the top of the block: `argv` first, so `argv[0]` ended
up highest, then the environment below it, then `AT_EXECFN` lowest. The
resulting order, low to high, was `[execfn][env…][argv…]` - which is not only
the reverse of Linux's but makes `start_data`-style reporting impossible to do
honestly: any `arg_start`/`env_end` pair read off that layout describes an
argument area *above* the environment, which no Linux program expects and
which would put `arg_start > env_end`.

So the fix is in two places, and the second is the load-bearing one:

- **`src/sched/runtime/load.rs`** - `write_arg_block` now sums the three
  string areas first (`argv_bytes`, `env_bytes`, `execfn_bytes`), computes
  `string_at = random_at - total`, and then writes **upward**: `argv` in
  order, the environment, then `AT_EXECFN`. `arg_start` is where the first
  string landed, `arg_end`/`env_start` are the cursor after the last argument,
  `env_end` the cursor after the last environment entry. The four are returned
  in an `ArgBounds` and merged into the same `ImageBounds` the reader takes
  the image addresses from.
- **`src/sched/runtime/load.rs`** - `load_user`'s program-header walk now
  feeds all five accumulators from the same loop over `image.segments` (the
  main image only, as `load_elf_binary` does), producing `start_data`,
  `end_data` and `start_brk` alongside the two it already had.
- **`src/mm/vma.rs`** - `ImageBounds` grows from three fields to ten, plus an
  `ArgBounds` and `with_strings`, so a reader still takes the whole record as
  one value. `Mm` stores it under **one lock** rather than the three atomics it
  had: a `fork`/`exec` replacing the record while a reader took the fields one
  at a time would report `arg_start` from one program and `env_end` from
  another, and Linux has the same shape for the same reason.
- **`src/vfs/procfs.rs`** - fields 45-51 render from `task.image` through the
  existing `address_word`. A zombie's `ImageBounds::default()` is already all
  zeros by the time it gets there (§6.117's branch).

**Pin:** `user/tests/src/bin/t-stat-image.rs`, 11 claims. The two that carry
the layout are the byte-level ones: the range `arg_start..arg_end` must *be*
this program's `argv`, NUL-separated and in order, and the range
`env_start..env_end` must be entries each containing `=`. A hardcoded zero
fails them; so does the old descending layout, because the bytes between its
bounds do not read as the arguments in order. The other witnesses: all seven
non-zero and inside user space, `end_data >= start_data`, `start_brk`
page-aligned and at or above `end_data`, all four string bounds inside the
`[stack]` mapping, a `malloc(1 MiB)` moving none of the seven, a forked child
reporting the parent's seven, and a zombie reporting seven zeros.

Green on host (`MPOS_ON_HOST=1`) and on the machine
(`build/boot-statimage1.log`, `build/boot-statimage2.log`).

**Counterfactual:** the six source files stashed, kernel and loader rebuilt
(`4345 KiB -> 1160 KiB` against `4337 KiB -> 1153 KiB`), booted -
**6 of 11 red** (`build/boot-statimage-cf1.log`):

```
FAIL:  the seven fields are addresses, not zeros
FAIL:  the data segment ends at or above where it starts
FAIL:  the bytes from arg_start to arg_end are this program's argv
FAIL:  and argv[0] is the first string, so the strings ascend
FAIL:  and the bytes from env_start to env_end are environment entries
FAIL:  all four string bounds are inside the stack's mapping
FAILED
```

The five that stay green are the honest limit, and each for its own reason: a
page-aligned `0` is still page-aligned; `env_start == arg_end` holds trivially
when both are `0`; none of the seven moves under a `malloc` when all seven are
constants; a forked child inherits the parent's zeros as it inherits anything
else; and a zombie's seven zeros are what the counterfactual kernel says for
every task. Only the claims that name *what the numbers are* are red.

**The rule.** Two readers of one record must not each own half of it. The
temptation here was to report `arg_start`/`env_end` off the existing
descending layout - which "works" in the sense that both are addresses inside
the stack, and fails in the sense that a Linux program scanning
`arg_start..arg_end` would read its environment instead of its arguments. The
layout had to move to Linux's before the numbers could be Linux's; the fields
were the instrument that showed it.

## §6.121 A pid number is a promise: the init namespace allocates its own, it does not number by slot

**The reading, before and after.** `t-stale-pid` is what §6.89's hand-booted probe became
(`build/scratch/reapwait/kslot.c` → `user/tests/src/bin/t-kslot.rs` → this): it collects a child,
lets an unrelated helper's child take the pool slot that number just left, and then asks the
collected number the three things a caller does with one. On `c683813`'s kernel
(`build/boot-kslot0.log`):

```
my child 7 collected
stranger (helper's child) 7
kill(7, 0) = 0 errno=2 (NOT ESRCH)
the stale number: /proc/7 -> S
kill(7, SIGTERM) = 0 errno=2 (NOT ESRCH)
after SIGTERM: /proc/7 -> Z
NOTE: the stranger reused my number
```

On this kernel, the same program's eight claims (`build/boot-stalepid2.log`):

```
ok:    the freed slot's next tenant is somebody else (13 is not 12)
ok:    a collected number is ESRCH to kill (-1 errno=3, want -1/3)
ok:    and /proc does not name it any more (ENOENT)
ok:    and it cannot end the process that took its slot (-1 errno=3)
ok:    so the stranger is still running after that (sits at S)
ok:    numbers only move forward across reuse (up to 17)
ok:    an unreaped zombie's number still names it (state Z, kill 0)
ok:    and reaping it spends the number too (18 -> ENOENT)
```

**Linux's answer, measured.** `build/scratch/pidreuse/reuse.c` (host 7.2.14): ten children born and
collected one at a time are `129232`..`129241`; fifty children born *after* them reuse none of those
numbers; and the collected number answers `kill(<n>, 0) = -1 errno=3 (ESRCH)`, `/proc/<n>` =
`ENOENT`, `waitpid` = `-1/errno 10 (ECHILD)`, `pidfd_open` = `-1/errno 3 (ESRCH)`. A pid is a
refcounted `struct pid` whose lifetime is its own, which is exactly what makes the number a promise.

**Why the number came back.** It was never a resolution bug. `resolve_pid` and `named_process` were
asked about a slot that genuinely held a live tenant, whose number genuinely was that number - the
answer was true. The defect was at the allocation end: `PidNs`'s root level **had no table**, and
`number_of`/`task_of`/`insert`/`forget` each opened with `if self.is_root() { return identity }`
(`crates/pidns/src/lib.rs`), so the number *was* the slot, and the slot allocator hands out the
lowest free index. Release a child and the very next `fork` inherits its promise. The crate's own
header said so, and pointed at `docs/notes/` §7.1 as the open question (§7.1 did not carry it - a
dangling citation, §6.89's second reading).

**The fix.** Delete the four short-circuits. The root now has `Numbers { next, by_number, by_slot }`
like every nested namespace, starts its cursor at 1, and never reuses a number. Three consequences,
each argued in this commit rather than slipped in:

1. **The machine's init must be registered, or nothing boots.** The first boot after deleting the
   short-circuits died before `/init` printed a line - `mpos: /init is up` then `INIT_EXITED: the
   system has no init left`. The cause is the shape of the promise: `try_spawn_init` never called
   `adopt_pid`, because under an identity map the slot *was* the number and registration was a
   no-op. With a table, an unregistered init has no number - so its number falls back to its slot
   (`1`), the cursor is still `1`, and **the init's first child is handed number 1**. The shell's
   first `fork` was numbered 1, `is_init` and the `init_discards` rules called it the init, and the
   session collapsed. `try_spawn_init` now calls `adopt_pid(INIT_PID, &root_ns())` before anything
   else can allocate, which is also the honest statement of what pid 1 is: the namespace's first
   registration.
2. **A pid value changes once.** Every number the machine reports comes from `next`, so a boot's
   numbers are `1` (init), then `2`, `3`, … in creation order across tasks instead of following the
   slot allocator's holes. That is a visible divergence from every earlier boot and it is the point:
   Linux's numbers are its namespace's, not a bookkeeping index. Nothing else in the ABI depends on
   the old values - every door resolves a number through the namespace and reports one through it.
3. **`next` does not wrap.** Inherited from the nested namespaces, now the machine's rule too; a
   boot would have to create 2^32 processes to see it, and the header says so instead of pretending
   the counter is `pid_max`-like.

**The pin, and what it must discriminate.** The ordering in `t-stale-pid` is what makes the reuse
deterministic rather than a coincidence: a helper occupies its own slot first, then this program
collects its child (freeing the slot above the helper's), then the helper forks - so the allocator
has exactly one lowest-free slot and it is the one under test. Without the fix that grandchild took
the collected number; with it, it takes one of its own. The last two claims are the boundary that
stops the fix from overshooting: a **zombie**'s number still names it (`kill` = `0`, `/proc` = `Z`)
and reaping spends it - a fix that dropped the registration at exit instead of at reap would take
the first of those away.

Green on host (`MPOS_ON_HOST=1`, 8/8) and on the machine (`build/boot-stalepid1.log`,
`build/boot-stalepid2.log`).

**Counterfactual:** `crates/pidns/src/lib.rs` and `src/sched/runtime/load.rs` stashed, kernel and
loader rebuilt (`4341 KiB -> 1157 KiB` against `4337 KiB -> 1154 KiB`), booted - **6 of 8 red**
(`build/boot-stalepid-cf1.log`):

```
FAIL:  the freed slot's next tenant is somebody else (7 is not 7)
FAIL:  a collected number is ESRCH to kill (0 errno=2, want -1/3)
FAIL:  and /proc does not name it any more (S)
FAIL:  and it cannot end the process that took its slot (0 errno=2)
FAIL:  so the stranger is still running after that (sits at Z)
FAIL:  numbers only move forward across reuse (up to 8)
ok:    an unreaped zombie's number still names it (state Z, kill 0)
ok:    and reaping it spends the number too (8 -> ENOENT)
FAILED
```

The two that stay green are the honest limit: a zombie's number names it whether numbers are
promises or slots, so those claims pin the *boundary* of the fix rather than the fix.

**A claim that passed on the broken kernel, and had to be fixed before it could witness.** The
"stranger is still running" claim was first written as a read straight after the `kill`. On the
counterfactual it went `ok: ... (R sits at 7)` - the read won the race against the signal the
broken kernel really did deliver, and on a later run the same binary read `Z`. A claim whose answer
depends on how fast the reader runs pins nothing. It now sleeps 100 ms between the `kill` and the
read, and the counterfactual is `FAIL: ... (sits at Z)` every time.

**The rule.** A number a caller can hold must outlive the task it names, or it is not a number - it
is a slot with a nicer name. The identity map at the root was defensible only while nothing
promised otherwise, and the promise was already written down: `wait` refuses a stranger's number
(`t-stranger`, `t-slotowner`, §6.89), a death carries its owner (`Retired::parent`, §6.90), and a
number is read in the caller's namespace (§6.91). Four doors agreeing on a promise the allocator
did not keep is the shape of every gap this campaign has closed.

**The half the allocator fix uncovered.** Changing what a number *is* is a probe for every place
that assumed the old answer, and this one turned up six doors that had been right by coincidence:
each compared a caller's **number** against a task-table **slot**, or reported a slot where the ABI
wants a number. They were invisible while the identity map held, because the two spellings were the
same integer. `/proc/<pid>/stat`'s `exit_code` fix (§6.112) and `Retired::parent` (§6.90) had
already made the same point from the other end - a record that has to be read after the task is
gone cannot be keyed by a slot - but these six were read at the moment of the call, so nothing but
a boot could see them.

Measured, one gate at a time (`MPOS_TEST_LANES=2`, both arms): the first run after the allocator
change was `18 of 285` user programs red on x86_64 (aarch64 the same family); after the sender's
pid, the fork answer and `set_tid_address` were fixed, `9`; after netlink's port id, `IFLA_NET_NS_PID`
and the cgroupfs's two halves, `1`; the last one was `t-udev-worker` alone, whose own fix
(`mpos_cred::UCred`, below) made the run `pass=285 fail=0`, `mpos-kunit 35 passed, 0 failed` on both
arms. The witnesses are all old pins - `t-signal-frame` said
`FAIL: and si_pid naming the sibling (got 0x6, want 0xb)`, where `0xb` is the sibling's number and
`0x6` its slot; `t-cgroupfs` said `FAIL: the root's cgroup.procs lists this program`; `t-ns-net`
said `FAIL: moving it into the child's namespace succeeds (got -3)`. Six doors, in order:

1. **The sender's own pid in a signal's `siginfo`.** `send_signal` and `send_signal_process`
   (`src/sched/runtime/child_signal.rs`) each built `(cur.tgid.0 as i32, cur.identity.ruid as i32)`
   as the sender, under the table lock they already held. Now they resolve the **number** in the
   same critical section (`cur.pid_ns.number_of(cur.tgid.0)`, falling back to the slot only for the
   tasks that have no number - the kernel's own).
2. **What `fork`, `vfork` and `clone` answer.** The spawn layer's `child_id.0 as isize` is an
   *internal* slot: `hand_down_keyrings` and `hand_down_rseq` take it and build `TaskId(child)` from
   it. Changing the spawn layer to answer a number broke `t-rseq` and `t-keyctl` - the right lesson,
   learned the wrong way round. The conversion is at the door instead: a `fn number_answer(child)`
   in `src/sched/runtime/child_exit.rs` converts once, on the way out to userspace.
3. **`set_tid_address`'s answer** (`src/syscall/linux/proc.rs`): `current().0` → `current_pid_number()`.
4. **Netlink's autobind port id** (`src/net/rtnl.rs`, two sites): the sender's pid is the number,
   because a reader matches `nlmsg_pid` against its own `getpid()` - a slot there makes every
   message from a process read as foreign mail.
5. **`IFLA_NET_NS_PID`** (`src/net/rtnl.rs`): the attribute carries a *number*, and the code looked
   it up as a slot; it resolves through `resolve_pid` now, then asks the namespace by slot.
6. **`cgroup.procs` / `cgroup.threads`** (`src/vfs/cgroupfs.rs`): the read side printed slots, and
   the write side compared the number it parsed against `(tid, pid)` slots.

**`UCred`: a field can be stored as a slot and rendered as a number, and the audit has to ask who
reads it.** The first attempt at the credential fix changed `self_cred`'s store to a number and left
`observed_cred`'s render alone - so the number was mapped through the table a second time, and
`t-unix-scm` (`SCM_CREDENTIALS carries the peer pid (got 0, want 414)`), `t-ns-pid` and
`t-nsnumbers` went red. The store-and-render split is deliberate and documented: `UCred` holds what
the machine stores, and the reader's namespaces decide what it sees. But the render cannot survive
the sender's death: it resolved a **slot**, and `t-udev-worker` reads the credentials of a forked
worker that the manager has already `waitpid`ed - `release_pid` has spent the number, so the render
answered `0` (and, when the slot had a new tenant, would have named the wrong process). Linux holds
a refcounted `struct pid` for exactly this reason.

The shape that is both: store the **number**, captured when the peer is met, plus the pid namespace
it was minted in - `mpos_cred::UCred { pid, uid, gid, pid_ns }`. The render then answers `pid_vnr`
faithfully: the number for a reader in that namespace (which is how it outlives the sender), `0` for
one outside it (§6.3's measurement - a peer in an ancestor namespace is not a member of the
reader's). The type moved to the `cred` leaf because both carriers need it - `net::unix` and
`vfs::pipe`'s socketpair, which stores the pair's creator the same way - and both already depend on
that leaf, so no edge was added for it.

**Counterfactual, the door half.** The allocator half was already witnessed (§6.89's reading above,
6 of 8 red). For these six, the changed files (`child_signal.rs`, `child_exit.rs`, `proc.rs`,
`rtnl.rs`, `cgroupfs.rs`, `unix.rs`, `socket.rs`, `pipe.rs`, `cred/lib.rs`, `epoll.rs`) were stashed
with the allocator fix kept, and the kernel and loader rebuilt:

```
t-signal-frame   3 red   FAIL: and si_pid naming the sibling (got 0x6, want 0xb)
                         FAIL: a child that dies of SIGABRT ... (status 0x0)
                         FAIL: a child that dies of SIGSEGV ... (status 0x0)
t-udev-worker    1 red   FAIL: a forked worker's reply carries its own pid, not the manager's
t-cgroupfs      10 red   FAIL: the root's cgroup.procs lists this program
                         FAIL: writing this pid moves this process into the group
                         ...
```

Restored, rebuilt, all three green (`build/boot-fix-t-signal-frame.log`, `-t-udev-worker.log`,
`-t-cgroupfs.log`: 0 `FAIL:` lines each).

**The rule, second half.** The first half said a number must outlive the task it names. This one is
its corollary: **a slot is not a name, and every door that hands one to userspace - or takes a name
from userspace - is a door where the two can be confused.** The audit cannot be done by grepping
`as i32`; it has to ask, per field, which of the two the *reader* needs, because a value can be
stored as one and rendered as the other.

## §6.122 The policy word: a policy number with an optional flag, and the band judged before support

`sched_setscheduler`'s second argument is not a policy number. It is a policy number with
`SCHED_RESET_ON_FORK` optionally OR-ed into it, and Linux takes the flag off before it asks which
policy the rest names. Around that word Linux has three separate judgements in a measured order, and
until this section this kernel answered two of them with the wrong errno:

1. **Is the word a policy at all?** `0, 1, 2, 3, 5, 6, 7` are Linux's `valid_policy()` numbers after
   the flag is masked. `4` is unused; `6` is `SCHED_DEADLINE`, which this door refuses outright (its
   shape belongs to `sched_setattr`); `7` is `SCHED_EXT`, the extensible class, which the reference
   host serves and whose band answers 0. Anything else is `EINVAL`.
2. **Is the priority inside that policy's band?** `1..=99` for `SCHED_FIFO`/`SCHED_RR`,
   `0..=0` for every fair policy *and for the deadline policy*, which is why
   `sched_get_priority_max(6)` answers 0 even though the scheduler door refuses 6.
3. **Can this kernel serve that policy?** Only here does support matter - and it is the *last* thing
   asked, because a bad word and a bad priority are bad arguments whatever the kernel can run.

The two wrong answers were (1) and (2) crossed over: a policy that exists was refused with `EINVAL`
- "that is not a policy" - where the honest answer is `EOPNOTSUPP`, "present, cannot do that"; and a
priority outside its band was refused with `EOPNOTSUPP` where Linux refuses the argument itself.

### What Linux answers (measured, one `fork` per case)

```text
build/scratch/statpolicy/d.c   (sched_setscheduler, fresh process per row)
  sched_setscheduler(0, SCHED_OTHER,    {0})  -> 0
  sched_setscheduler(0, SCHED_BATCH,    {0})  -> 0
  sched_setscheduler(0, SCHED_IDLE,     {0})  -> 0
  sched_setscheduler(0, SCHED_EXT(7),   {0})  -> 0
  sched_setscheduler(0, RESET|SCHED_OTHER)    -> 0
  sched_setscheduler(0, RESET|SCHED_BATCH)    -> 0
  sched_setscheduler(0, SCHED_FIFO,     {0})  -> EINVAL(22)      the band first
  sched_setscheduler(0, SCHED_FIFO,   {100})  -> EINVAL(22)
  sched_setscheduler(0, SCHED_FIFO,    {10})  -> EPERM(1)        a legal band, then authority
  sched_setscheduler(0, SCHED_RR,       {0})  -> EINVAL(22)
  sched_setscheduler(0, SCHED_DEADLINE, {0})  -> EINVAL(22)      deadline has its own door
  sched_setscheduler(0, RESET|DEADLINE)       -> EINVAL(22)
  sched_setscheduler(0, 8) / (9) / (0x10000000) -> EINVAL(22)

build/scratch/statpolicy/e.c   (the boundary, and the sibling table)
  policy 6 -> EINVAL, 7 -> 0, 8..12 -> EINVAL, 0x10000000 -> EINVAL
  sched_get_priority_max: 0/1/2/3/5/6/7 -> 0/99/99/0/0/0/0, 8 -> EINVAL(22)
  sched_getattr(48) -> policy 0, nice 0, rtprio 0

build/scratch/statpolicy/f.c   (the *other* door reads the field raw)
  sched_setattr(policy 0/3/5/7)      -> 0          (one struct, four servable policies)
  sched_setattr(policy RESET|0)      -> EINVAL(22) the flag is not in this field
  sched_setattr(policy RESET|3)      -> EINVAL(22)
  sched_setattr(policy 1, prio 10)   -> EPERM(1)
  sched_setattr(policy 1, prio 0)    -> EINVAL(22)
  sched_setattr(policy 6) / (8)      -> EINVAL(22)
  sched_get_priority_max(RESET|0)    -> EINVAL(22) this door does not mask the flag
  sched_get_priority_max(RESET|1)    -> EINVAL(22)

build/scratch/statpolicy/g.c   (the attribute door judges the band too)
  sched_setattr(OTHER, prio 1)             -> EINVAL(22)
  sched_setattr(OTHER, prio 0, nice 5)     -> 0
  sched_setattr(BATCH / IDLE / EXT, prio 1) -> EINVAL(22)
  sched_setattr(RR, prio 0)                -> EINVAL(22)
  sched_setattr(FIFO, prio 100)            -> EINVAL(22)
  sched_getattr(48) -> policy 0, nice 0, rtprio 0
```

Two rows are worth reading twice. `sched_get_priority_max(RESET|SCHED_OTHER)` is `EINVAL` while
`sched_setscheduler(RESET|SCHED_OTHER)` is served: **the mask belongs to the door, not to the
number.** And `sched_setattr`'s policy field is a plain policy number - the reset behaviour travels
in its `sched_flags` word - so the same `RESET|OTHER` word that is legal at the scheduler door is
`EINVAL` in that struct.

### What this kernel answered before (§6.122's before-reading)

`build/boot-schedpolicy0.log`, the new pin on the kernel as it stood at `0df1e62`:

```text
FAIL:  SCHED_BATCH is a policy, so the answer is about support (22)
FAIL:  SCHED_IDLE likewise (22)
FAIL:  SCHED_EXT is a policy number too, so its band is the fair one (-1)
FAIL:  and its door is a support refusal, not a bad word (22)
FAIL:  SCHED_RESET_ON_FORK|SCHED_OTHER is a policy word too (22)
FAIL:  SCHED_FIFO at priority 0 is EINVAL, the band before support (95)
FAIL:  and 100 is outside the band too (95)
FAIL:  SCHED_RR at priority 0 is EINVAL as well (95)
FAIL:  sched_setattr's SCHED_EXT is a support refusal too (22)
```

Nine wrong answers in three shapes: four policies that exist called "not a policy", three
out-of-band priorities called "not supportable", one policy number refused at the modern door, and
`sched_get_priority_max(7)` called a bad word. `SCHED_OTHER` and every genuine `EINVAL` row were
already right, which is exactly why no existing pin caught it: every program in the suite asks for
`SCHED_OTHER` or a real-time policy at a legal priority, and those rows agree.

### The fix

The judgements are pure facts about Linux's ABI, so they live in the host-compiled `abi.rs` beside
§6.116's `sched_attr` verdicts, and the two handlers only read the caller's memory and dispatch:

- `Policy` + `policy_number(word)` - the raw reading, for the doors that switch on the word;
- `scheduler_policy(word)` - mask `SCHED_RESET_ON_FORK` off, then read the number, for the one door
  that masks;
- `policy_band(policy)` - `(1, 99)` or `(0, 0)`, the table the sibling door prints;
- `sched_setscheduler_verdict(word, priority)` - `Ok(true)` serve, `Ok(false)` `EOPNOTSUPP`, `Err`
  the word or the band;
- `sched_setattr_policy_verdict(policy, priority, nice)` - the same three, raw and then the nice;
- `priority_bound_verdict(policy, max)` - the band's end or `EINVAL`, raw.

`handle_sched_setscheduler` now reads the priority out of the param (it used to read it only on the
`SCHED_OTHER` arm) and asks the verdict; `handle_sched_setattr` now reads the struct's `priority`
field at offset 20, which it had never read at all - so `sched_setattr(SCHED_OTHER, priority 1)` is
now the `EINVAL` Linux gives instead of a 0 that dropped the priority, and the nice range check
moved inside the verdict where it belongs (after the band, before the apply).

### The pin

`user/tests/src/bin/t-sched-policy.rs`, 21 claims, every one of them a row of the tables above. Two
of the claims are *agreements* rather than counterfactuals and say so: `/proc/<pid>/stat`'s
`policy` field is the policy `sched_getscheduler` names (both 0), and its `rt_priority` is 0 -
honest zeros of a scheduler with one band, one policy and no real-time class, not a stub that was
never written. `processor` is 0 for the same kind of reason (one CPU), and is the one claim that is
machine-only, since the measuring host has many and moves a task between them.

Host run: 21/21 `PASS`. Machine run (`build/boot-schedpolicy2.log`): 21/21 `PASS`, exit 0.

Two traps were paid for on the way to that pin:

- **The Rust target's musl does not enter the kernel for these doors.** `libc::sched_setscheduler`
  and `libc::sched_getscheduler` answer `ENOSYS`(38) without issuing a syscall - measured by calling
  the wrapper and the raw `SYS_*` number from the same binary, where the raw call succeeds. A pin
  written with the wrappers would have measured the stub, on both arms. Every policy call in the pin
  is a raw syscall, the same shape §6.116's pin uses.
- **`/proc/self/stat`'s field index is (N-3) from the tokens after `)`.** A first draft used (N-4)
  and reported field 39 (`processor`) as 17 - which is field 38, `exit_signal`'s SIGCHLD. The tell
  was a claim failing with a plausible-looking number: 17 is a real value in that line, just the
  neighbour's.

### The counterfactual

`git stash push -q -m cf-schedpolicy -- src/syscall/linux/abi.rs src/syscall/linux/posix.rs`,
kernel and loader rebuilt, the pin booted (`build/boot-schedpolicy-cf1.log`): **9 of 21 red** - the
nine rows above, unchanged. Restored, rebuilt, 21/21 green.

### Named gaps this does not close

`SCHED_RESET_ON_FORK` is now *recognised* and refused rather than being called a bad word, but it is
not remembered: this scheduler has no per-task policy to reset at a fork, because it has no policies
but one. `SCHED_BATCH`, `SCHED_IDLE` and `SCHED_EXT` are likewise recognised and refused with
`EOPNOTSUPP`; serving any of them is the same named gap as serving `SCHED_FIFO` (no priorities, no
bands). `sched_rr_get_interval`'s slice **value** is still this kernel's own (`1_000_000_000 / TICK_HZ`,
4 ms) where the reference host answers its own (2 ms) - both are "this machine's round-robin
timeslice", and the host's value is tuned, not fixed. **`§6.127` corrected the rest of that
sentence:** the divergence was not only the value. Linux answers **zero** for a target that is not
on the runqueue (blocked, held at a stop, a zombie) and this door answered the constant for every
number it accepted, so the *shape* of the answer differs too, not just the figure.

### The rule

**A word that names a thing and a word that modifies it are different questions, and the errno has
to say which one failed.** `EINVAL` means "that is not a thing"; `EOPNOTSUPP` means "that is a
thing, and this kernel cannot do it". A caller retries an `EOPNOTSUPP` differently from an
`EINVAL`: `SCHED_BATCH` at `EINVAL` reads as "no such policy in this ABI" and loses the program its
fallback. And the order of the questions is part of the answer, because the first refusal is the one
that gets reported - which is why the band is asked before support, and the word before them both.

## §6.123 A timer on a CPU clock counts CPU, not machine uptime

The POSIX timer section of the status appendix said, for two years' worth of commits, that this
kernel "refuses the CPU-time clocks with `EOPNOTSUPP`, **because it keeps no per-task CPU
accounting**". That sentence stopped being true at §6.95 (`RUSAGE_THREAD` gave every task its own
pair of CPU counters) and at §6.99 (`CLOCK_PROCESS_CPUTIME_ID` and `CLOCK_THREAD_CPUTIME_ID` started
answering from them). What was left was a refusal with a reason that no longer existed - and a
`timer_create(CLOCK_PROCESS_CPUTIME_ID)` that answered `EOPNOTSUPP` where Linux answers 0.

### What Linux does, measured

`timer_create` on every clock id, on the host (7.2.14, `build/scratch/timerclock/t.c`):

```text
timer_create(0  REALTIME)          -> 0
timer_create(1  MONOTONIC)         -> 0
timer_create(2  PROCESS_CPUTIME)   -> 0
timer_create(3  THREAD_CPUTIME)    -> 0
timer_create(4  MONOTONIC_RAW)     -> EOPNOTSUPP(95)
timer_create(5  REALTIME_COARSE)   -> EOPNOTSUPP(95)
timer_create(6  MONOTONIC_COARSE)  -> EOPNOTSUPP(95)
timer_create(7  BOOTTIME)          -> 0
timer_create(8  REALTIME_ALARM)    -> EPERM(1)      needs CAP_WAKE_ALARM
timer_create(9  BOOTTIME_ALARM)    -> EPERM(1)
timer_create(10)                   -> EINVAL(22)    no clock has that number
timer_create(11 TAI)               -> 0
timer_create(12)                   -> EINVAL(22)
```

So the four refusals this kernel already answered correctly are the raw/coarse three (Linux's own
`EOPNOTSUPP`) and a number no clock has (`EINVAL`); the alarm pair differs only in errno, and for a
reason this kernel does not model (there is no `CAP_WAKE_ALARM`). The divergence was clock 2.

What the accepted timer then *does* is the whole question, because a timer that expires on the wrong
clock is worse than a refusal (`build/scratch/timerclock/fire.c`, `SIGEV_SIGNAL` to a `SIGUSR1`
handler, one 30 ms value and one 10/10 ms periodic):

```text
settime(30 ms) = 0
  after 200 ms of sleeping           fired=0
  after burning CPU (60M iters)      fired=1
periodic settime(10/10 ms) = 0
  periodic: after 100 ms of sleeping fired=0
  periodic: after burning CPU        fired=65
gettime at arming: 50 ms
gettime after burning past it: 0 ms
```

Two facts sit in those lines: the deadline is reached by **running**, not by the wall clock, and
`timer_gettime`'s remaining time is measured on the same clock - it did not move across the sleep at
all and read zero once the value had been burned through.

### What this kernel did

`handle_timer_create` refused clock 2 in the same arm as clock 3, and `handle_timer_settime`
computed the deadline as `crate::hal::ticks() + value_ticks` for *every* timer. Both halves were
one assumption: that a timer's clock is the machine's tick counter. Every door that reads a timer's
state - the expiry scan, `timer_gettime`, and the `old_value` half of `timer_settime` - subtracted
from that same reading.

### The fix

A timer now carries the clock it was built on, and the deadline arithmetic goes through it.

- `TimerClock { Wall, Process }` (`src/sched/runtime/task.rs`) with one method, `now(wall,
  group_cpu)`, since both readings are in ticks. `PosixTimer` gained a `clock` field, set once at
  `timer_create` and never changed.
- `process`'s reading is the group's own `utime + stime` from `Mm::cpu_all()` - **the same pair
  `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` answers from**, so a caller cannot see the timer's clock
  disagree with the clock it named. (Children's CPU is excluded on both sides, which is what POSIX
  and Linux both mean by "the process's own CPU time".)
- The four doors follow: `check_posix_timers` reads the group's CPU once per leader and then
  `timer.clock.now(wall_now, group_now)` per timer, using that value for both the expiry test and
  the periodic reload; `timer_state` does the same for `timer_gettime`'s remaining; `timer_arm` now
  takes `value_ticks` instead of a deadline, because the scheduler is the only place both the clock
  and the reading are in hand - a syscall layer converting a timespec into a tick-counter value
  would be arming a CPU-time timer against machine uptime, which is exactly the defect this
  signature exists to make unspellable.
- `handle_timer_create` maps clock 2 to `TimerClock::Process` and leaves 3 refused, with the reason
  moved rather than deleted (see the named gap below), and the doc's old sentence - "this kernel
  keeps no per-task CPU accounting" - is gone.

### The pin

`user/tests/src/bin/t-timer-cpuclock.rs`, 16 claims, all of them the movement the two clocks differ
by rather than a snapshot:

- a timer can be built on the process's own CPU clock; 200 ms of CPU time can be armed;
- **sleeping 150 ms does not fire a 200 ms CPU-time timer**, and **does not eat the time left**
  (`200 ms -> 200 ms of 200 ms` on the machine, `200 -> 199` on the host) - the second half matters
  because the first is trivially true of a timer that does not exist;
- burning CPU reaches the deadline (1 expiry), and a one-shot has then spent its value (0 ms left);
- a periodic 20 ms CPU-time timer does not reload on a 120 ms sleep, and reloads three times as CPU
  is burned;
- a **wall** timer of the same order of value, in the same program, does fire across that sleep - 
  which is what makes the sleeping half a measurement of the clock rather than of the test's timing;
- the creating thread's clock is still refused here (Linux serves it: the claim accepts either
  answer per arm), and a raw-clock timer is `EOPNOTSUPP` on both.

Host run: 16/16 `PASS`. Machine run (`build/boot-cpuclock2.log`): 16/16 `PASS`, exit 0. The two
numbers that identify the clock on the machine are `fired 1 after 61 chunk(s)` and `200 ms -> 200 ms
of 200 ms`.

Two traps were paid for here:

- **A signal with a handler is handed over at a kernel entry, and a pure burn loop has none.**
  `deliver_pending_signal`'s own doc said it: "the syscall return path is the only place that has
  [a register file]". The first machine run of the pin burned 400 chunks (over 200 ms of CPU,
  `timer_gettime` reading 0 left) and the handler never ran, so two claims were red for a reason
  that had nothing to do with the clock. The loop now makes one cheap `getpid` per chunk, which is
  what an ordinary program's loop does. Linux also hands a handled signal over on an interrupt's
  return to ring 3, and since 2026-11-12 this kernel does too on aarch64 — the machine's interrupt
  entry hands its trap frame to `hal::kernel::deliver_at_trap` before it preempts. On x86_64 that
  sentence is still true and §2.13 says why: its handlers get an `InterruptStackFrame` with the
  GPRs saved below it where Rust cannot name them, so `Gate::capture_trap_regs` answers `false`
  there. **`docs/notes/` §2.13 measures that difference and names what remains**
  (`build/scratch/timerclock/iret.c` on the host, `build/boot-iret1.log` here).
  §7.4h is about a different question - which thread a delivered signal went to.
- **A test's contract moves when the kernel starts serving the field it measured.** `t-timer-create`
  carried a claim written as `a clock with no accounting behind it is EOPNOTSUPP`, which was true
  and is now false; it became `a timer can be built on the process's own CPU clock`, an agreement
  on both arms. This is the second time in this series (§6.105's `t-wait4` narrow, §6.123's move);
  the rule is that the pin's *claim* is part of the fix, and leaving the old wording is how a suite
  starts asserting last month's behaviour.

### The counterfactual

`git stash push -q -m cf-cpuclock -- src/sched/runtime/task.rs src/syscall/linux/proc.rs
src/sched/mod.rs`, kernel and loader rebuilt, the pin booted (`build/boot-cpuclock-cf1.log`): **7 of
16 red** - the timer cannot be built at all (`EOPNOTSUPP`), so arming is `EINVAL`, the remaining
time is `-1`, the burn fires nothing and the periodic half cannot run. Restored, rebuilt, 16/16
green.

The honest limit, and it is the same shape as §6.105's: the two "does not fire while sleeping"
claims are green on **both** kernels, because the broken kernel never gets far enough to arm a CPU
timer. They are still worth their place - they are what a *partially* wrong fix (a CPU timer armed
against the tick counter) would go red on, and this series has now paid twice for a counterfactual
that could not distinguish "absent" from "right".

### Named gaps this does not close

- **`CLOCK_THREAD_CPUTIME_ID` (3) stays `EOPNOTSUPP`.** Its clock belongs to the thread that called
  `timer_create`, while the timer hangs off the group (`PosixTimer` lives in the leader's table) and
  outlives that thread. Linux freezes the dead thread's clock and the timer then expires against
  frozen time; this table cannot express a frozen reading without walking every timer at every
  thread's exit. A CPU-time timer that quietly started counting a *sibling's* CPU after its thread
  died would be the "nearly right" shape `AGENTS.md` §3 rules out, so the half that cannot be honest
  stays a refusal with its reason written down.
- **`TIMER_ABSTIME`** is still `EINVAL` (one clock, no absolute domain) - unchanged here.
- **The alarm clocks (8, 9)** answer `EOPNOTSUPP` where Linux answers `EPERM`; there is no
  `CAP_WAKE_ALARM` in this kernel's capability model, and no wake-from-suspend to arm.
- An expiry of a CPU-time timer is delivered through the same per-task pending set as a wall one,
  with the same group-directed choice of target (`§2.11`); no overrun semantics changed.

### The rule

**A deadline is meaningless without the clock it was measured on, and the clock is part of the
timer, not part of the arithmetic at each door.** The old code had one reading - `hal::ticks()` -
woven through four places that each looked right on its own; the defect was that the reading and
the timer it was subtracted from were stored in different objects. Storing the clock *with* the
timer, and making the arm take a value rather than a deadline, is what turned four opportunities
for the same mistake into one place the compiler insists on.

## §6.124 The `timer_*` doors judge in Linux's order, and their flags carry one bit

§6.123 gave a POSIX timer a clock. This section is about the *shape of a refusal*: the five calls
of the family each answer two questions - is the request well-formed, and is the thing it names
there - and a man page does not say which is asked first. It matters because a wrong errno sends
the caller down a fallback for a problem it does not have. All of it was measured at the raw-syscall
level on a host 7.2.14 before any code was touched
(`build/scratch/timerorder/{final,truth,gaps,both,oldarm,abstime,edges}.c`); several early probes
had to be thrown away because a `timer_t` corrupted by an earlier bad `create` made a later
`settime` read as success. The confound-free probes each use a fresh timer for every row.

### What Linux does, measured

`timer_create`: an unreadable `sevp` is `EFAULT` **before** the clock is judged.

```text
create(999 bogus clock,    sevp=1,  out ok/bad)  -> EFAULT(14)   not EINVAL(22)
create(4  MONOTONIC_RAW,   sevp=1,  out ok/bad)  -> EFAULT(14)   not EOPNOTSUPP(95)
create(999, NULL, out=0/1)                        -> EINVAL(22)
create(4,   NULL, out=0/1)                        -> EOPNOTSUPP(95)
create(4,   bad notify/signo/tid, out=1)          -> EOPNOTSUPP(95)  clock before sigevent meaning
create(0,   bad notify/signo/tid, out=1/0)        -> EINVAL(22)      sigevent meaning before out
create(0,   NULL, out=1)                          -> EFAULT(14)
create(0,   NULL, out=0)                          -> EFAULT(14)
```

So the order is **sevp pointer readability -> clock -> sigevent content -> `timer_t*`**. A refused
clock outranks a broken sigevent's *meaning* but not the unreadability of its bytes; a broken
sigevent's meaning outranks a bad output pointer. This kernel judged the clock first, then `out`,
then the sigevent content - the first and third of those are reversed.

`timer_settime`'s `flags` is a 32-bit `unsigned int` whose only meaningful bit is
`TIMER_ABSTIME` (bit 0). Every other bit is **ignored** and the timer is armed (`final.c`):

```text
flags = 0, 2, 4, 6, 8, 0x7ffffffe, -2 (0xfffffffe)  -> 0, timer_gettime remaining ~0.099999 s
flags = 2^32, 2^32|2                                  -> 0   (truncated to 0 and 2, armed)
flags = 1 (ABSTIME), -1 (0xffffffff), 2^32|1          -> 0, but an ABSOLUTE value already past
flags = 2^32|ABSTIME                                   -> 0, truncated to ABSTIME
```

This kernel refused **every** word carrying a bit past `TIMER_ABSTIME` with `EINVAL`, so a caller
that set a harmless future-flag bit could not arm a timer at all. `TIMER_ABSTIME` itself Linux
*arms* (for a `CLOCK_REALTIME` timer, +1 h reads back `remaining 3599.999996794`); this kernel
still answers `EINVAL` for it, but that is now a **named gap** with a truthful reason - one
monotonic clock, no `CLOCK_REALTIME` epoch to name an absolute deadline against - not a claim
that other bits are invalid.

`timer_settime`'s judgement order, and a side effect a probe would miss:

```text
settime(bad id, 0, new=1)             -> EFAULT(14)   new pointer before id
settime(bad id, 0, new ok)            -> EINVAL(22)
settime(good,   0, bad nsec 2e9)      -> EINVAL(22)
settime(good,   0, new ok, old=1)     -> EFAULT(14)   BUT the timer is already armed
settime(bad id, 0, new ok, old=1)     -> EINVAL(22)   id before old_value
settime(good,   0, bad nsec, old=1)   -> EINVAL(22)   timespec before old_value
```

`oldarm.c`: `settime(good, it_value=3600 s, old=1)` answers `-1 errno=14`, yet the very next
`timer_gettime` reads `remaining=3599.999944756` - Linux armed the timer *before* it tried to copy
the old state out to a broken pointer. `both.c` pins that the `TIMER_ABSTIME` decision does not
outrank the new pointer, the id or the timespecs either (`ABSTIME + good + new ok + old=1` is 14,
`ABSTIME + bad id` is 22, `ABSTIME + bad nsec` is 22). This kernel checked `old_value` *before*
the timespecs and before `timer_arm`, so a good arm with a bad `old_value` answered `EFAULT`
**without ever arming the timer** - a refusal with the wrong side effect, not just the wrong
errno.

`timer_gettime`: the id is judged **before** the output buffer.

```text
gettime(bad id, curr=1)   -> EINVAL(22)
gettime(bad id, curr=0)   -> EINVAL(22)
gettime(good id, curr=1)  -> EFAULT(14)
```

This kernel asked the buffer first, so `gettime(bad, curr=1)` answered `EFAULT`. `timer_getoverrun`
and `timer_delete` already answer `EINVAL` for a bogus id on both, and a deleted id is not reused
as a live one.

### What this kernel answered before

`handle_timer_create` matched the `clockid` at the very top, then checked `timer_t*`, then copied
and validated the `sigevent`. `handle_timer_settime` began with two checks - `flags &
!TIMER_ABSTIME != 0` and `flags & TIMER_ABSTIME != 0` - refusing every unknown bit and ABSTIME
alike, then checked `new_value`, then `old_value`, then the timespecs, then `timer_arm`.
`handle_timer_gettime` checked `curr_value` before `timer_state(id)`. None of the individual
answers was a wild invention; the defect was the *order*, which is exactly the sort of wrong shape
that sends musl's `timer_create` helper or a program probing capabilities down an unrecoverable
fallback.

### The fix

- `src/syscall/linux/abi.rs`: `pub(crate) fn timer_settime_flags_verdict(word: usize) ->
  Result<(), isize>` truncates to `u32` and refuses only bit 0 (`Err(err::EINVAL)`), returning
  `Ok(())` for every other word. It lives in the host-compiled `abi.rs` because `proc.rs` is
  `#[cfg(target_os = "none")]` and cannot be host-tested; a `#[test]`
  `timer_settime_flags_ignore_every_bit_but_abstime` pins the eight accepted words and the four
  refused ones. The 32-bit truncation is measured, not assumed (`edges.c`: `2^32` and `2^32|2`
  are served, `2^32|1` is ABSTIME).
- `src/syscall/linux/proc.rs::handle_timer_create`: the `sigevent` pointer is now checked for
  readability and its four words copied **at the top** (before the `clockid` match), the clock is
  judged second, the sigevent content third, and `timer_t*` last - with `timer_alloc` after all of
  them. The wording that split "readability" from "meaning" is the point: Linux answers `EFAULT`
  for unreadable bytes but `EOPNOTSUPP` for a refused clock even when those bytes would have been
  nonsense.
- `src/syscall/linux/proc.rs::handle_timer_settime`: the two flags checks are gone, replaced by the
  verdict after the `new_value` pointer check but **before any arm** (so an `EINVAL` never leaves a
  relative timer running behind it - a self-correction made after an early draft put it after
  `timer_arm`). The order is now `new_value` -> flags -> the two `timespec_ticks` -> `timer_arm`
  (which validates the id and arms) -> `old_value` -> `write_itimerspec`. Putting `old_value`
  after `timer_arm` is what reproduces Linux's `EFAULT`-with-an-armed-timer side effect.
- `src/syscall/linux/proc.rs::handle_timer_gettime`: `timer_state(id)` is now called before the
  `curr_value` buffer check.
- The `TIMER_ABSTIME` doc comment moves its reason to the named gap and points here; §6.124 does
  not implement an absolute deadline, it stops claiming that other flag bits are refusals.

### The pin and its counterfactual

`user/tests/src/bin/t-timer-order.rs` (23 claims, raw `libc::syscall` with self-spelled
`SigEvent`/`Itimerspec` wire records, errno compared as plain numbers so neither arm's `Errno`
spelling decides a verdict): host 23/23, machine 23/23 (`build/boot-timerorder1.log`). A no-op
`SIGALRM` handler is installed because `timer_create(..., NULL)` defaults to that signal and the
*host* arms an already-expired `ABSTIME` timer that fires at once - without it the host probe died
of its own default disposition mid-run. This kernel refuses `ABSTIME`, so only the host needs it.

`git stash push -q -m cf-timerorder -- src/syscall/linux/abi.rs src/syscall/linux/proc.rs`, kernel
**and** the UEFI loader that embeds it rebuilt, the same pin booted (`build/boot-timerorder-cf1.log`):
**11 of 23 red** -

```text
FAIL: an unreadable sigevent outranks a bogus clock (got 22, want 14)
FAIL: an unreadable sigevent outranks a refused clock (got 95, want 14)
FAIL: a broken sigevent outranks a bad timer_t* (got 14, want 22)
FAIL: an unknown flag bit is ignored and the timer is armed (set Err(22), ...)
FAIL: many unknown flag bits are ignored and the timer is armed (set Err(22), ...)
FAIL: the flags word is 32-bit, so its high bits are ignored and the timer is armed (set Err(22), ...)
FAIL: a bad id outranks a bad old_value (got 14, want 22)
FAIL: a malformed tv_nsec outranks a bad old_value (got 14, want 22)
FAIL: and that EFAULT still armed the timer (read Ok(0), left 0 ms)
FAIL: a bad id outranks a bad curr_value buffer (got 14, want 22)
FAIL: a bad id outranks a NULL curr_value too (got 14, want 22)
```

The `arm-before-copy` claim is the one that would pass on a kernel that merely returned the right
`EFAULT`: it is **red on the broken kernel** (`left 0 ms`) because the old code checked `old_value`
before `timer_arm` and never armed at all. Four shape/control claims stay green on both kernels by
design (the two plain doors, the refused-clock-before-meaning row, ABSTIME's named gap), and are
stated as such. Restored, rebuilt, 23/23 green again (`build/boot-timerorder2.log`).

### Named gaps this does not close

`TIMER_ABSTIME` stays `EINVAL` (one monotonic clock, no `CLOCK_REALTIME` epoch). `timer_create`
still stops signal numbers at `SIGRTMIN` (32) and the alarm clocks still answer `EOPNOTSUPP`
where Linux answers `EPERM`; the table is still eight fixed slots (`EAGAIN` on full, `edges.c`'s
`RLIMIT_SIGPENDING=0` corner) rather than Linux's rlimit-backed allocation, and ids are the lowest
free slot rather than never-reused. None of those is a refusal *order*, so none is touched here.

### The rule

**A refusal is part of the ABI: which question a door asks first is as much a promise as the errno
it returns.** Four independent doors here each held a locally-sensible check in the wrong position,
and one of them changed a side effect (arm) as well as an answer. Measure the order with a probe
whose rows cannot contaminate each other, then reorder the checks; a verdict about a *word* belongs
in one host-testable pure function, but a verdict about *which of two bad things wins* belongs in
the handler's statement order, pinned by a program that hands both bad things at once.

## §6.125 `setitimer`/`getitimer`: `which` is an `int`, the order is Linux's, and the arm survives an `EFAULT`

Two old interval-timer calls answer three questions at once — *is this request well-formed*,
*which clock does it name*, *what did the previous timer look like*. Which question is asked first
is a promise, and one of the four checks changes a **side effect** as well as an errno. Everything
here was measured on a host Linux 7.2.14 with raw `SYS_setitimer`/`SYS_getitimer` probes in
`build/scratch/itimer/` before this kernel was touched; a man page does not say which half of a
refusal the caller branches on.

`src/syscall/linux/proc.rs`'s `handle_setitimer` doc had been wrong for some time. It said the
virtual and profiling timers "would need per-task CPU accounting, which does not exist yet". That
reason went stale in §6.95/§6.99/§6.123: `Task::cpu_user`/`cpu_system` and `Mm::cpu_all()` *do*
charge CPU. The truth is smaller and worse: `check_itimers` (`src/sched/runtime/task.rs:2084`)
scans **only** `itimers[ITIMER_REAL]`, so `ITIMER_VIRTUAL`/`ITIMER_PROF` are installed and read
back faithfully but never expire. Three stale doc claims (that `proc.rs` doc, the `itimers` field's
"for want of per-task CPU accounting", and `runtime/mod.rs`'s "checked where that time is counted"
— there is no such code) were corrected to that honest, named gap. Closing the expiry scan is a
larger change that also collides with §2.13 (a handled signal is delivered only on a syscall
return), so it is **not** done here.

### Linux's answers (host 7.2.14, `build/scratch/itimer/`)

`which` is a 32-bit `int`, not the `usize` register it arrives in. `width.c` passes the full 64-bit
word as a vararg so the kernel does the `(int)` truncation:

```
setitimer(which=2^32=0x100000000, new, NULL) = 0 errno=0
   after 2^32 arm: armed=yes it_value=9.999976
setitimer(which=2^32+3=0x100000003, new, NULL) = -1 errno=22
getitimer(which=2^32=0x100000000, ok) = 0 errno=0
getitimer(which=2^32+3=0x100000003, ok) = -1 errno=22
setitimer(which=(int)-1 sign-extended=0xffffffffffffffff, new, NULL) = -1 errno=22
setitimer(which=3 + new=BAD=0x3, new, NULL) = -1 errno=14
setitimer(which=-1 + new=BAD=0xffffffffffffffff, new, NULL) = -1 errno=14
```

The whole-register comparison this kernel used (`which > ITIMER_PROF`) refused `2^32` where Linux
armed a timer — and a large word indexed `itimers[which]` past the three-slot array in
`itimer_swap`. `final.c` (`BAD = ((void*)1)` — never the address of a stack variable, which is
readable and turns every `EFAULT` claim into a green `rc=0`, the lesson of the discarded `order.c`)
measured the refusal order:

```
== refusal order (host 7.2.14) ==
setitimer(which=3 bogus, new=BAD, old=0)                  rc=-1 errno=14   (new beats which)
setitimer(which=3, new ok, old=0)                         rc=-1 errno=22
setitimer(which=3, new=0, old=BAD)                        rc=-1 errno=22   (which beats old)
setitimer(which=3, new ok, old=BAD)                       rc=-1 errno=22
setitimer(0, new=BAD, old=0)                              rc=-1 errno=14
setitimer(0, new ok, old=BAD)                             rc=-1 errno=14
setitimer(0, new=0, old=BAD)                              rc=-1 errno=14
setitimer(0, new=bad usec 2e6, old=BAD)                   rc=-1 errno=22   (timeval beats old)
setitimer(0, new=bad usec -1, old ok)                     rc=-1 errno=22
getitimer(which=3, ptr=BAD)                               rc=-1 errno=22   (which beats buffer)
getitimer(0, ptr=BAD)                                     rc=-1 errno=14
```

So `do_setitimer`'s order is `copy_from_user(new)` [`EFAULT`] → `timeval_valid` [`EINVAL`] →
`which` switch [`EINVAL`] → **arm** → `copy_to_user(old)` [`EFAULT`]. The timeval check and the
`which` check both return `EINVAL`, so their relative order cannot be discriminated by errno; the
two that *can* be are "bad new pointer beats bogus which" and "bad timeval beats bad old pointer".
`getitimer` judges `which` before its output buffer (already right here). And `side.c` is the
effect that makes the last step load-bearing:

```
   before: it_value=0.000000 armed=no
setitimer(0, {10s}, old=BAD)                              rc=-1 errno=14
   after EFAULT: it_value=9.999998 armed=yes
```

Linux armed the timer before failing to write `old_value`. This kernel checked `old_ptr` up front:
the same `EFAULT`, a disarmed machine. `nullvalue.c` also overturned a remembered man-page line —
a NULL `new_value` **disarms** and still reports the old timer (`armed=no` afterwards); the kernel
already did this, so those are control claims, not a new behaviour.

### What changed

- `src/syscall/linux/abi.rs`: new host-testable `pub(crate) fn itimer_which_verdict(word: usize)
  -> Result<usize, isize>` = `let which = word as u32 as i32; if (0..=2).contains(&which) { Ok(..) }
  else { Err(err::EINVAL) }`. It uses literal `0..=2` because `crate::sched::ITIMER_*` are exported
  only under `#[cfg(target_os = "none")]`, and `proc.rs` (which calls it) is likewise bare-metal
  only while `abi.rs` is host-compiled. A host `#[test]` covers `0,1,2,2^32,2^32+1,2^32+2 → Ok`
  and `3,2^32+3,usize::MAX,0xffff_ffff → Err(err::EINVAL)`. Host unit tests `759 → 760`.
- `handle_setitimer` reordered to exactly the measured sequence: `new` pointer + `itimerval_ms`
  (so `EFAULT`/`EINVAL` come first) → `itimer_which_verdict(which)` (`EINVAL`, and its `Ok` value
  is the truncated index that `itimer_swap` actually uses) → build `new` → **`itimer_swap` (arm)**
  → `old_ptr` check (`EFAULT`) → `write_itimerval`. The two leading checks (`which > ITIMER_PROF`
  and the `old_ptr` pre-check) are deleted. Now a bad `old_value` answers `EFAULT` *after* the
  timer is armed, matching `side.c`.
- `handle_getitimer` calls `itimer_which_verdict` before `buffer_ok(ptr)`, keeping its existing
  which-before-buffer order and adding the 32-bit truncation.
- Three stale comments now name the real gap (`check_itimers` scans one slot; CPU time *is*
  charged, the scan just does not consult it) and point here.

### Pin — `user/tests/src/bin/t-itimer-order.rs` (27 claims)

Raw `libc::syscall` with a self-spelled, size-asserted `struct itimerval` (musl has no binding for
these), the bad pointer `1`, and `Report`'s `ok:`/`FAIL:` per claim. Two plain doors, six `which`
truncation claims, five new-pointer/which and bogus-which order claims, two timeval claims, the
three-step **arm-before-old witness** (disarm → `setitimer(0, hour, BAD)` = `EFAULT` → `getitimer`
still armed), four `getitimer` order claims, and five NULL-`new_value` controls. Host 27/27
`PASS`; machine 27/27 `PASS` (`build/boot-itimer1.log`, `exit=0`, `=== END`).

Counterfactual = stash the four source files, rebuild the kernel **and** the UEFI loader that
embeds it, boot the same binary (`build/boot-itimer-cf1.log`): **8/27 red** —

```
FAIL: 2^32 names ITIMER_REAL and arms it (got 22, want 0)
FAIL: 2^32 + 1 names ITIMER_VIRTUAL and is served (got 22, want 0)
FAIL: getitimer(2^32) is served (got 22, want 0)
FAIL: an unreadable new_value outranks a bogus which (got 22, want 14)
FAIL: an unreadable new_value outranks a sign-extended -1 (got 22, want 14)
FAIL: an unreadable new_value outranks 2^32's clock too (got 22, want 14)
FAIL: a malformed tv_usec outranks a bad old_value (got 14, want 22)
FAIL: and that EFAULT still armed the timer (read Ok(0), armed false)
```

The `arm-before-old` claim is the one a kernel that merely returned the right errno would still
miss. Rows that are green on both kernels are stated as honest boundaries: `2^32+3` and the
sign-extended `-1` are `EINVAL` either way, and all four `getitimer` order rows were already right
(only the truncation was missing). Restored, rebuilt both, 27/27 green again
(`build/boot-itimer2.log`).

### Named gaps this does not close

`ITIMER_VIRTUAL`/`ITIMER_PROF` are installed and read back but never expire — the expiry scan
covers `ITIMER_REAL` only. Doing them properly needs a CPU-time deadline in `check_itimers` and a
delivery path that does not wait for a syscall return (§2.13). `TIMER_ABSTIME` for the `timer_*`
family stays `EINVAL` (§6.124). `ITIMER_REAL`'s expiry still delivers `SIGALRM` at whichever door
the interrupted task next reaches.

### The rule

**A word's width is as much an ABI promise as its value.** `which` and `flags` arrived in a 64-bit
register and were compared as `usize`; Linux truncates both to 32 bits first, so a caller passing
`2^32` names a real clock, not an error. A verdict about a *word* belongs in a host-testable pure
function, but a verdict about *which of two bad things wins* — and above all about whether a
refusal leaves a side effect behind — belongs in the handler's statement order, pinned by a
program that hands both bad things at once and then reads the machine's state.

## §6.126 `/proc/uptime` was two wrong shapes, and a "flake" was the bug (LANDED 2026-11-11)

**Correction to §6.108 and §6.125's ledger.** Twice this session a red on `t-starttime`'s
"a tick per centisecond" claim was written down as a timing artefact — "the child is born up to
180 ms before `born_at` on a contended TCG lane" (§6.108) and "the §6.96/§6.108 two-moving-records
shape under single-core TCG contention, not a regression" (the §6.125 gate ledger). **Both were
wrong.** The red was a real bug in `/proc/uptime`, and the reason it looked like a flake is that
the bug fires on only ~9% of reads. This section closes it and carries the correction into the two
places the wrong diagnosis was committed.

### What Linux prints, measured

`/proc/uptime` is two columns, each `%lu.%02lu`: seconds plus a **zero-padded two-digit**
centisecond fraction. The second column is the sum of per-CPU idle time, a live counter — not a
constant. `build/scratch/uptime/shape.c` (1200 reads across a second boundary, host 7.2.14):

```
after 1200 reads: every first fraction had exactly 2 digits
start up=20299.67 frac_digits=2 idle=304616.14 frac_digits=2 idle<=up=no
after 1s sleep up=20300.67 idle=304630.73
```

`probe.c` (3000 reads): `one-digit=0 two-digit=3000 other=0`, `backwards=0 biggest-forward=0.010 s`.
The idle column grew by 14.59 across that one second of sleep on a many-CPU host.

This kernel rendered the file as `format!("{}.{} 0.0\n", secs, centis)` — the first column's
fraction was **not** zero-padded and the second column was the **literal `0.0`**. Two wrong shapes,
one of them the `AGENTS.md` §3 violation: a live number served as a constant zero.

### The mechanism, and why it read as a flake

`uptime()` computes `centis = (ticks % 250) * 100 / 250` (TICK_HZ = 250 on both machines), which
walks `0..=99`. Nine of those hundred values are one digit. For those nine, the file printed `S.4`
and a caller's `atof("S.4")` reads `S + 0.4`, where Linux's `S.04` is `S + 0.04` — **360 ms too
high** (`90 ms * centis`, with centis = 4). The other 91 values happen to print two digits and read
correctly. So the error is a ~9% probabilistic over-read, not a clock that runs fast.

`t-starttime` brackets a child's `starttime` (in centisecond ticks from `since_boot_nanos()`) against
two `/proc/uptime` millisecond readings. When both bracket endpoints fall inside the same bad
centisecond window but the two "before" readings do not, the whole bracket shifts by exactly
`90 * centis` ms. The §6.125 gate's `150 ticks inside 1860 ms` is `1500 + 360` (centis 4); §6.108's
`150 ticks against 1320 ms` is `1500 - 180` (centis 2, the endpoints falling on opposite sides of
the wrap). Neither is fork latency, neither is TCG contention: both are this file's missing zero.
Running `t-starttime` on the *broken* kernel (`build/boot-starttime-cf.log`) passed with
`Some(151) ticks inside Some((1500, 1520)) ms` — the flake is the 9%, which is exactly why two
sessions mistook it for load.

### What changed

- `src/vfs/procfs.rs`: `uptime()` now returns four words `(secs, centis, idle_secs, idle_centis)`;
  the idle pair comes from `crate::sched::cpustat::counters().idle` — the same counter `/proc/stat`'s
  idle column has read since §6.100, charged by `charge_cpu` on both machines (Linux sums per-CPU
  idle; this machine has one CPU, so that counter *is* the sum). The host version returns zeros so
  the pure renderer is still callable from host tests.
- A host-testable pure function `fn uptime_line(secs, centis, idle_secs, idle_centis) -> String`
  renders `format!("{secs}.{centis:02} {idle_secs}.{idle_centis:02}\n")`. Its long doc keeps the
  `S.4` / 360 ms arithmetic, the `t-starttime` numbers, the correction above, and the §3 reason a
  literal `0.0` cannot stand. A host `#[test]` asserts `5.04 304616.14\n`, `5.40 304616.14\n`,
  `0.00 0.00\n`, `9.09 9.09\n`, `1.99 2.99\n` byte-for-byte, plus "two columns, each fraction
  exactly two digits, ends in `\n`" — the same shape as `the_diskstats_row_is_linuxs_twenty_fields`.
  Host unit tests `760 -> 761`.
- The `/proc/uptime` generator now calls `uptime_line` on those four words. No reader expected the
  literal `0.0` (`t-procstat`'s `uptime_seconds`, `t-starttime`'s `uptime_ms` both `parse` the first
  column only), so nothing else moves.

### Pin — `user/tests/src/bin/t-uptime-shape.rs` (8 claims)

Two columns; a shape loop of `130 batches x 20 reads`, each batch followed by a 10 ms sleep, so
2600 reads cover a **whole 1.3 s centisecond cycle** from any starting point; both columns' fractions
always exactly two digits; the first column never goes backwards; the idle column grows across a
200 ms sleep; the first column's delta stays inside its own `Instant` sleep bracket (40 x 5 ms,
±300 ms); and `Report::finish`. Host 8/8 `PASS`, machine 8/8 `PASS` (`build/boot-uptime3.log`,
`exit=0`, `=== END`).

Counterfactual = stash `src/vfs/procfs.rs`, rebuild the kernel **and** the UEFI loader that embeds
it, boot the same binary (`build/boot-uptime-cf2.log`): **4/8 red** —

```
FAIL: every first-column fraction has exactly two digits (2600 reads)
FAIL: every idle-column fraction has exactly two digits
FAIL: the first column never goes backwards
FAIL: the idle column grows while the machine has nothing to run (Some(0) -> Some(0))
```

The four control rows (two columns, loop ran, delta bracket, `finish`) are green on both kernels by
design and are stated as such: the delta bracket is too short to reliably enter the 9% window, so
the discrimination lives in the shape loop and the idle growth, not in a timing comparison. Restored,
rebuilt both, 8/8 green again (`build/boot-uptime3.log`).

Two pin mistakes worth their lesson. (1) An `idle_nonzero` claim reddened on the *fixed* machine:
`0.00` is a legitimate reading before any idle tick has been charged, so that claim pinned nothing —
the one-digit `0.0` width and the growth claim are what discriminate a literal from a counter.
(2) The first shape loop was a tight 3000-read burst; on the broken kernel it stayed inside one tick
and the first-column width claim passed. A burst of reads is not a span of time: batching across
1.3 s of sleeps makes the bad window deterministic.

### The gate

`build/gate-uptime.log`, `GATE EXIT=0`: stage 1 `1262 tests, 0 failed` (68.98 s, the +1 is the host
renderer test); x86_64 `pass=290 fail=0 (290 built)`, `mpos-kunit 35 passed, 0 failed` (52.16 s,
total 69.35 s, -1.0%); aarch64 `pass=289 fail=0 (289 built)`, `mpos-kunit 35 passed, 0 failed`
(130.08 s, total 221.65 s, +65.0% against the single-arm quick green). The pin ran and passed on
**both** arms (`build/test-lane-x86_64-1/serial.log:4571`,
`build/test-lane-aarch64-1/serial.log:4433`).

### Named gaps this does not close

The idle counter is a per-tick accumulator, so the second column advances in 4 ms steps rather than
Linux's finer accounting; the first column is still the tick counter, not `since_boot_nanos()`. Both
are honest readings of the one clock this machine has. §2.13's delivery path and the
`ITIMER_VIRTUAL`/`ITIMER_PROF` expiry scan (§6.125) are untouched.

### The rule

**A formatting bug is a behaviour bug.** `{centis}` and `{centis:02}` return the same *number* to a
reader who parses a decimal point as a decimal point only when the number has two digits; on the
other nine values out of a hundred they differ by 360 ms. A file that is wrong 9% of the time is not
a flake to be waited out — it is the thing to measure. And a diagnosis written into the ledger after
a green single-arm rerun is a claim like any other: §6.108 and §6.125's ledger said "TCG contention"
without a number that could only come from that cause; `1860 - 1500 = 360 = 90 * 4` was the number
that named `/proc/uptime` instead.

## §6.127 `sched_rr_get_interval` answers a slice for a task on the runqueue and zero for one that is not (LANDED 2026-11-11)

**Correction to §6.122's named gaps.** That section recorded this door's difference from the
reference host as *"`sched_rr_get_interval` still answers this kernel's own slice (`1_000_000_000 /
TICK_HZ`, 4 ms) where the reference host answers its own (2 ms) - both are 'this machine's
round-robin timeslice', and the host's value is tuned, not fixed."* The value being machine-specific
is true; treating the **whole** difference as the value was wrong. Linux answers **zero** for any
target that is not on the runqueue, and this kernel answered 4 ms for every number it accepted. Zero
is not a small slice — it is the answer "that task cannot run", and a program polling this door to
see whether a child is making progress reads a constant as "running" forever.

### What Linux answers, measured

`build/scratch/rrslice/` — eight probes on host 7.2.14 (`nproc` 16, `sched_rr_timeslice_ms = 100`),
raw syscall, uid 1000:

| probe | shape it varies | answer |
|---|---|---|
| `a.c` | the door's own arguments | self = `2000000` ns; `pid 1` = `0`; `999999` = `ESRCH(3)`; `-1` = `EINVAL(22)`; `(0, NULL)` = `EFAULT(14)`; `(999999, NULL)` = `ESRCH(3)` |
| `d.c` | **only the target's state** | blocked in `read()` = **0**; parent (running) = `2000000`; spinning child = `2000000`; zombie not reaped = **0**; after the reap = `ESRCH(3)` |
| `e.c` | `SIGSTOP` / `SIGCONT` | a child held at a stop = **0**; after `SIGCONT` still **0** (it stays blocked in `pause()`) |
| `b.c` | buffer length, via guard pages | 4/8/12-byte windows = `EFAULT(14)`; **16 bytes succeeds** |
| `c.c` | scheduling policy | `SCHED_BATCH`, `SCHED_IDLE` still `2000000` (`sched_setscheduler` itself is `EPERM` for uid 1000) |
| `f.c` | nice | 0/19/-20 and a runnable child at nice 0/19: all `2000000` |
| `g.c` | runqueue length, pinned to cpu 0 | 1..9 runnable tasks: all `2000000` |
| `h.c` | pid vs tid | a thread's tid is accepted exactly like a process's pid |

So the rule is one question — **is the target on the runqueue** — and nice, policy, queue length and
which number the caller passed are all irrelevant to it. `b.c` also confirms this kernel's 16-byte
`buffer_ok` was already the right width, and `a.c` confirms the refusal order this door already had
(no shared NULL-buffer guard, pid lookup first: `(0, NULL)` = `EFAULT`, `(999999, NULL)` = `ESRCH`).

### What this kernel did

```rust
let hz = crate::hal::TICK_HZ as u64;
unsafe {
    (tp as *mut i64).write_unaligned(0);
    ((tp + 8) as *mut i64).write_unaligned(1_000_000_000 / hz as i64);
}
```
The `tv_nsec` was the constant `1_000_000_000 / TICK_HZ` for every target — a blocked read, a
`SIGSTOP`ped child, a zombie and a running task all read `4000000`. Its doc comment was wrong too:
"the 10ms the timer runs at", where `TICK_HZ` is 250 (4 ms).

### The fix: the state letter, derived once

The runnability question is the same one `/proc/<pid>/stat`'s `state` field answers, and this kernel
had exactly one derivation of it — inside `snapshot_of`. Writing a second one in the syscall layer
would have created a door that could tell a program two different things about its own child, so the
derivation moved out:

- `src/sched/runtime/procfs.rs` — `pub(crate) fn state_of(t, id, task) -> char` holds the `Z`/`T`/`S`/`R`
  ladder that `snapshot_of` used to inline; `snapshot_of` now calls it. New
  `#[cfg(target_os = "none")] pub fn task_state(id: TaskId) -> Option<char>` asks it from outside the
  table, answering `None` for a number that no longer names a live task (that caller is owed `ESRCH`).
- `src/sched/mod.rs` — `task_state` joins the `pub use runtime::{…}` list.
- `src/syscall/linux/abi.rs` — `pub(crate) fn rr_interval_nanos(runnable: bool, hz: u64) -> u64`
  (`if runnable { 1_000_000_000 / hz } else { 0 }`), host-tested:
  `a_runqueue_answer_is_the_slice_and_everything_else_is_zero` pins 4 ms at 250 Hz, 1 ms at 1000 Hz,
  10 ms at 100 Hz, and **zero at every rate** — the half that must not scale.
- `src/syscall/linux/posix.rs` — `handle_sched_rr_get_interval` resolves the pid as before, judges
  the 16-byte buffer as before, then asks `task_state`: only `'R'` gets the slice, `S`/`T`/`Z` get
  `tv_nsec = 0`, and `None` is `ESRCH`. Its doc now carries the `d.c`/`e.c`/`f.c`/`g.c` readings.

### The pin — `user/tests/src/bin/t-rr-interval.rs`, 12 claims

One child read in three states, plus controls, so a red line names which question was answered
wrongly. Each state claim prints the letter the kernel reported, which is what makes a red
diagnosable rather than merely red:

```
ok:    a running task is answered a slice (got 0, 4000000 ns)
ok:    a child blocked in a read is answered zero (state S, got 0, 0 ns)
ok:    and the caller is answered a slice in the same breath (0, 4000000 ns)
ok:    a child held at a stop is answered zero (state T, got 0, 0 ns)
ok:    a zombie is answered zero (state Z, got 0, 0 ns)
ok:    and the same number after the reap is ESRCH (got 3)
ok:    a runnable child is answered a slice (state R, got 0, 4000000 ns)
```

The **runnable-child control** is load-bearing: without it a kernel answering zero for everything
would pass all three state claims. The four shape claims (`null buffer`, `nobody`, `negative pid`,
`unreadable buffer`) are controls too — they were already right and stay right.

The **value** is deliberately not claimed. This machine's slice is 4 ms and the reference host's is
2 ms; both are that machine's round-robin timeslice, and asserting the number would turn a tuning
difference into a failure while passing happily on a kernel that answered a constant forever. What
is a contract is zero versus non-zero.

The syscall number comes from `libc::SYS_sched_rr_get_interval` (148 on x86_64, **127** on aarch64) —
a literal 148 would have been silently wrong on the second arm.

### Counterfactual

`git stash push -q -m cf-rr -- src/syscall/linux/abi.rs src/syscall/linux/posix.rs
src/sched/runtime/procfs.rs src/sched/mod.rs`, kernel and loader rebuilt, `build/boot-rr-cf1.log`:
**3 of 12 red**, exactly the three state claims —

```
FAIL:  a child blocked in a read is answered zero (state S, got 0, 4000000 ns)
FAIL:  a child held at a stop is answered zero (state T, got 0, 4000000 ns)
FAIL:  a zombie is answered zero (state Z, got 0, 4000000 ns)
FAILED / === t-rr-interval exit=1
```

The other nine — including all four shape claims and the runnable control — are green on both
kernels, because the refusal order and the slice value were never the defect. Restored, rebuilt,
`build/boot-rr2.log`: 12/12 `PASS`, `exit=0`.

### Host and machine

Host (`MPOS_ON_HOST=1`): 12/12, and the states read `S`, `T`, `Z`, `R` exactly as Linux's own
`/proc/<pid>/stat` reports them — the same letter this kernel now derives. Machine
(`build/boot-rr1.log`): 12/12, `exit=0`, slice `4000000` ns (this machine's one tick at 250 Hz).
Host unit tests `761 → 762`.

### Named gaps this does not close

The slice **value** stays this kernel's own (one tick). Linux's is `sched_rr_timeslice_ms`
(sysctl-tunable, 100 ms on the reference host) divided by its HZ; this kernel has no tunable and
one round-robin band, so serving its tick is the honest reading. That is a different gap from this
one, which was about *which* answer a target's state earns. §2.13's delivery path is untouched.

### The rule

**A constant that is right for one state is a wrong answer for every other state.** The door looked
like a getter for a machine constant, and a machine constant has no reason to vary — so nobody
checked whether Linux varied it. The tell was in the existing notes: the difference was written down
as "the host's value is tuned, not fixed", which explained the *number* while quietly assuming the
*answer* was the same shape. When a divergence is explained by "the figure differs per machine", the
next question is whether anything besides the figure differs.

## §6.128 `getcpu`'s two output pointers are two writes, and neither cancels the other (LANDED 2026-11-11)

`getcpu(cpu, node, tcache)` answers two numbers. On these boards there is nothing to watch move — the
image configures `qemu.cpus = 1` (`assets/configs/machine.toml:45`) and no machine model here has a
NUMA node — so both numbers are `0`, and both were `0` before this change too. The defect was not in
the values. It was in **how many of the two pointers get written when one of them cannot be**.

### The shape this kernel had

```rust
for ptr in [cpu, node] {
    if ptr != 0 && !buffer_ok(ptr, 4, true, from_user) {
        return err::EFAULT;
    }
}
for ptr in [cpu, node] {
    if ptr != 0 {
        unsafe { (ptr as *mut u32).write_unaligned(0) };
    }
}
```

Two loops: judge both pointers, then write both. A single unreadable pointer answers `EFAULT` and
**neither word is written** — including the one that was perfectly readable. That is the shape
`AGENTS.md` §3 rule 4 names: refusing is fine, but refusing a call does not license the kernel to
skip the part of it that was legitimate, because the caller cannot tell the difference between "your
number is stale" and "your number is what I would have told you".

### What Linux does — measured

`build/scratch/getcpu/p.c`, `q.c`, `r.c` on the host (7.2.14), raw `syscall(SYS_getcpu, …)`, both
buffers reset to a sentinel before each call so "was it written" is a comparison rather than an
assumption:

```text
== getcpu(ptr_cpu, ptr_node, tcache) ==
(good, good):  rc=0  errno=0  cpu=0x0000000e node=0x00000000
(good, BAD):   rc=-1 errno=14 cpu=0x0000000e node=0xbbbbbbbb   -> cpu WRITTEN
(BAD, good):   rc=-1 errno=14 cpu=0xaaaaaaaa node=0x00000000   -> node WRITTEN
(BAD, BAD):    rc=-1 errno=14 cpu=0xaaaaaaaa node=0xbbbbbbbb   -> neither
(NULL, NULL):  rc=0  errno=0  cpu=0xaaaaaaaa node=0xbbbbbbbb
(NULL, good):  rc=0  errno=0  cpu=0xaaaaaaaa node=0x00000000
(good, NULL):  rc=0  errno=0  cpu=0x0000000e node=0xbbbbbbbb
tcache=BAD with both NULL:     rc=0 errno=0
tcache=BAD with both good:     rc=0 errno=0
2000 reads: cpu range 14..14  sysconf(_SC_NPROCESSORS_ONLN)=16
```

`r.c` is the clean re-run of the two discriminating rows, printing only the verdict:

```text
(good,BAD) ret=-1 errno=14 cpu=0x0000000b node=0xbbbbbbbb  -> cpu WRITTEN
(BAD,good) ret=-1 errno=14 cpu=0xaaaaaaaa node=0x00000000  -> node WRITTEN
```

Four facts fall out, and only the first is the bug:

1. **Two `put_user`s, or'ed errors.** A bad `node` does not stop `cpu` being written, and a bad `cpu`
   does not stop `node` being written. The errno is `EFAULT` in every half-bad case; the side effect
   is what differs.
2. **Both unreadable is still one `EFAULT`** and neither word moves — so the fix must not turn a
   total refusal into a partial write.
3. **NULL is not an error.** `(NULL, good)` answers `0` and writes only `node`; `(NULL, NULL)`
   answers `0` and writes nothing. "Tell me only the half I asked for" is a legal query, and the
   pre-existing code already got that right.
4. **`tcache` is never read.** A `tcache` of `1` answers `0` both with readable outputs and with
   NULL ones. The pre-existing code already ignored it, and that is now pinned so a later "let us
   validate every pointer" change cannot regress it.

A fifth fact is worth writing down because it is the reason the *value* half is not this entry's
business: the host answers `cpu = 14` on a 16-CPU machine, so `getcpu` reports a real processor
number rather than a constant — but a kernel that answered the literal `0` would be indistinguishable
from a correct one **on a one-CPU machine**, which is what this image is. The value half therefore
gets no claim, only a stable-across-calls one.

### The fix

`src/syscall/linux/posix.rs::handle_getcpu`: one loop, Linux's order (`cpu` then `node`), each
pointer attempted independently, the failures accumulated and answered at the end.

```rust
let mut failed = false;
for ptr in [cpu, node] {
    if ptr == 0 { continue; }
    if !buffer_ok(ptr, 4, true, from_user) { failed = true; continue; }
    unsafe { (ptr as *mut u32).write_unaligned(0) };
}
if failed { err::EFAULT } else { 0 }
```

The handler's doc now carries the measured table and says why the two writes are independent, so the
next person to "tidy" the loop into a pre-check has the reading in front of them.

**No pure function was extracted, and that is deliberate.** §6.127's fix had a genuinely pure half
(`rr_interval_nanos(runnable, hz)`) and put it in `abi.rs` with a host test. Here the whole rule is
"attempt each pointer, or the errors" — the only thing standing between the code and a host test is
`buffer_ok`, which answers `true` on the host for every pointer (`src/uaccess.rs:118`), so a host
unit test could not tell the fixed kernel from the broken one. This rule's witness has to be a
machine.

### The pin: `user/tests/src/bin/t-getcpu.rs`, 15 claims

| claim | before | after |
|---|---|---|
| `getcpu(self, self) answers 0` | green | green |
| `and it filled both words` | green | green |
| `an unreadable node beside a good cpu is EFAULT` | green | green |
| **`and the good cpu was written anyway`** | **`0xaaaaaaaa` (sentinel)** | **`0x00000000`** |
| `an unreadable cpu beside a good node is EFAULT` | green | green |
| **`and the good node was written anyway`** | **`0xaaaaaaaa`** | **`0x00000000`** |
| `two unreadable pointers are still just EFAULT` | green | green |
| `a NULL cpu is not an error` / `and it still filled node` | green | green |
| `a NULL node is not an error` / `and it still filled cpu` | green | green |
| `two NULLs are a legal query` | green | green |
| `an unreadable tcache … is ignored` (×2) | green | green |
| `the processor is stable across calls` | green | green |

Every claim is written as the **pair** (errno, side effect) because either half alone is satisfied by
the wrong kernel: the broken one answers the right `EFAULT`, and a hypothetical kernel that wrote
both words unconditionally would satisfy the side-effect half. The sentinel is `0xAAAA_AAAA`, not
`0`, because `0` is a legal answer and would make "written" indistinguishable from "untouched".

**Host 15/15** (`cpu 0x00000002` — on a 16-CPU host that is the processor the run happened to be
scheduled on, which is why the value is read into a buffer and never asserted against a constant).
**Machine 15/15** (`build/boot-getcpu1.log`, `cpu 0x00000000`, `exit=0`).

**Counterfactually 2 red** (`git stash push -q -m cf-getcpu -- src/syscall/linux/posix.rs`, both
kernel and loader rebuilt, `build/boot-getcpu-cf1.log`):

```text
FAIL:  and the good cpu was written anyway (0xaaaaaaaa)
FAIL:  and the good node was written anyway (0xaaaaaaaa)
```

The other 13 claims are green on both kernels — the honest boundary, and exactly the shape this
entry is about: the refusal half was never wrong, only the half that survives it.

### Gate

`build/gate-getcpu.log`, `GATE EXIT=0`, both arms in one command with the lane count forced to two:
stage 1 `1263 tests, 0 failed` (69.01 s — unchanged, this change adds no host-testable half);
x86_64 `pass=292 fail=0 (292 built)` over 2 lanes in 51.67 s, `mpos-kunit 35 passed, 0 failed`,
total 69.37 s (-2.9%); aarch64 `pass=291 fail=0 (291 built)` over 2 lanes in 133.81 s,
`mpos-kunit 35 passed, 0 failed`, total 225.09 s (-5.2%), both lanes on `=== END`. The new program
ran on both arms: `build/test-lane-x86_64-0/serial.log:3825` / `:3842` and
`build/test-lane-aarch64-1/serial.log:4907` / `:4924` (`RUN` / `PASS`).

### Named gaps this does not close

- **The processor number is a constant because the machine has one CPU.** A second vCPU is what makes
  this field a reading rather than a correct-looking zero; `qemu.cpus` is a machine-config question,
  not a syscall one.
- **The node is always zero** for the same reason — no machine model here exposes a NUMA topology.
- **`tcache` is accepted and ignored.** Linux ignores it too (measured above), so this is not a
  divergence; `rseq` is where the per-thread cache actually lives on Linux, and this kernel's `rseq`
  support (§6.90 area) is separate.

### The rule

*A refusal is not permission to skip the rest of the call.* When a syscall reports `EFAULT` for one
of several output pointers, the caller still receives the pointers it did not ask about being
wrong — and a kernel that pre-checks them all converts a partial success into a total failure the
program has no way to detect. The measured table above is the specification: attempt each, or the
errors, answer the accumulated verdict.

## §6.129 `/proc/<pid>/stat`'s `tty_nr`, `tpgid` and `session` are three answers, not two literals and a copy (LANDED 2026-11-11)

Fields 6, 7 and 8 of `stat` are `session`, `tty_nr` and `tpgid`. This file answered field 6 with
`pgid`, field 7 with the literal `0`, and field 8 with the literal `-1`. The two literals carried a
comment saying why: *"no controlling terminal — everything on this machine types at the one
console"*. The copy carried a second: *"no `setsid` has ever been answered with a group of its own"*.

Both reasons had expired, and neither had noticed:

- `setsid` **is** served, and `TIOCSCTTY` with it, so a task's session and its group are two stored
  numbers now (`src/sched/runtime/task.rs:1212 session`, `:1218 ctty`).
- The machine has a console, and it also has ptys — `t-TIOCGPTPEER`/`t-aa-ptyprobe` open them — so
  "everything types at one console" was a claim about the *boot*, not about a file a caller reads
  per process. `src/vfs/tty.rs:2546 by_dev_id` exists precisely to find the terminal a device id
  names, and `:2237 foreground()` reads its foreground group.

### What Linux answers — measured

`build/scratch/ttynr/b.c` on the host (7.2.14). The probe forks, because a container's shell is
already a process-group leader and `setsid` then answers `EPERM`; it also sets
`setvbuf(stdout, NULL, _IONBF, 0)`, because a child that exits with buffered output loses exactly the
lines the measurement needs. The first word after the **last** `)` is field 3, so field `n` is at
zero-based index `n − 3` of that split (the first attempt indexed `n − 3` off a list that began at
field 1 and so read one field to the left everywhere).

```text
no controlling terminal          tty_nr=0        tpgid=-1       pgrp=54880  session=54880  state=R
slave /dev/pts/1 rdev=0x8801 major=136 minor=1 -> Linux tty_nr encoding 34817
--- child: after setsid + TIOCSCTTY ---
child with a controlling terminal tty_nr=34817    tpgid=54894    pgrp=54894  session=54894  state=R
the child (fg holder)            tty_nr=34817    tpgid=54894    pgrp=54894  session=54894  state=R
grandchild pid=54895
the grandchild (same tty, not fg) tty_nr=34817    tpgid=54894    pgrp=54895  session=54894  state=S
tcgetpgrp before = 54894
tcsetpgrp(grandchild) = ok
tcgetpgrp after  = 54895
the child after handing fg away  tty_nr=34817    tpgid=54895    pgrp=54894  session=54894  state=R
the grandchild now foreground    tty_nr=34817    tpgid=54895    pgrp=54895  session=54894  state=S
the dead grandchild              tty_nr=34817    tpgid=54895    pgrp=54895  session=54894  state=Z
tcgetpgrp with a dead fg group = 54895
```

Four facts come out of it, and each one is a different question:

1. **`tty_nr` is the terminal's device id recoded, not its `st_rdev`.**
   `(minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)` — `0x8801` becomes `34817`. This is
   `new_encode_dev`, the same word `/proc/self/mountinfo` and `statx`'s `st_rdev` users expect.
2. **No terminal means `0` and `-1`.** Both literals were right *for that case* — which is why they
   survived: every process that never opened a pty reads them as the truth.
3. **`tpgid` belongs to the terminal, not to the reading task.** The grandchild holds group `54895`
   and still prints `tpgid=54894`; after `tcsetpgrp` both files print `54895`, the leader's
   included. It is one number with as many readers as the terminal has.
4. **`session` is not `pgid`.** The leader reads its own id in both fields; the grandchild reads the
   leader's id in 6 and its own in 5. A death does not move either — the dead foreground group still
   reads `tpgid=54895`, which is why `tcgetpgrp` agrees with it.

### The shape this kernel had

```rust
Cow::Owned(alloc::format!("{}", reader_number(task.pgid))), // 6: session
Cow::Borrowed("0"),                                          // 7: tty_nr
Cow::Borrowed("-1"),                                         // 8: tpgid
```

The copy is the interesting half. Field 6 printed the *group* number in a file whose field 5 is the
group number — so for every process that never called `setsid` the two columns agree, and the file
looks right. It stops looking right exactly where the answer matters: a session leader's grandchild.
That is the shape §6.129's counterfactual produced, `session 12` where the leader is `11`.

### The fix

- **`crates/abi/src/lib.rs`** — `pub const fn tty_nr(dev: Option<(u32, u32)>) -> u32`, the recoding
  above and `0` for `None`. It lives in the leaf because two layers need the same word: `sched`
  produces the snapshot, and a future `statx` caller would want it too, and one definition is
  cheaper than two that must agree.
- **`src/sched/runtime/procfs.rs`** — `TaskSnapshot` gains `tty_nr: u32` (from `task.ctty` through
  that function) and `terminal_foreground: Option<usize>` (the **slot** of the terminal's foreground
  group, found with `task.ctty.and_then(crate::vfs::tty::by_dev_id).and_then(|tty| tty.foreground())`).
  It stores the slot, not the number, because the file renders numbers through the *reader's* pid
  namespace and only `vfs` knows which namespace that is.
- **`src/vfs/procfs.rs`** — field 6 renders `task.session`, field 7 `task.tty_nr`, field 8
  `Some(slot) => reader_number(slot)` else `-1`. The three comments were rewritten to say what the
  fields *are*, rather than why they were constants.

### The pin — `user/tests/src/bin/t-stat-tty.rs`, 12 claims

It mints a pty pair (`/dev/ptmx` + `TIOCGPTN` + `TIOCSPTLCK`), forks a child that `setsid`s and
takes the slave with `TIOCSCTTY`, then forks a grandchild that `setpgid`s itself, and moves the
foreground to it. Two mistakes were paid for writing it:

- **The grandchild must not call `tcsetpgrp`.** A background process that touches the terminal's
  foreground gets `SIGTTOU`, whose default action is *stop* — so the first version did not measure
  anything, it hung until the harness killed it. The move is made by whoever currently holds the
  foreground; the grandchild only sleeps.
- **"different from the naive encoding" is not a claim.** The recoding only diverges past minor 255,
  and a pty's number is small, so on the host the two encodings are equal and the assertion passes
  vacuously. What is testable is that the answer is derived from *this pair's* device id.

A third assertion was removed rather than repaired: *"session and group are two different questions"*
holds for a program the runner forks into its own group (host) and fails for the machine's pid 1,
which leads both its group and its session — a fact about the launcher, not about the file. Field 6's
discriminator is the grandchild line, which is green on the fix and red without it.

**Host 12/12**, `slave 136/1, want tty_nr 34817`, `session 60014 pid 60014`, `pgrp 60015`,
`moving the foreground moves the grandchild's tpgid (60014 -> 60015)` and the leader's file with it.
**Machine 12/12** (`build/boot-stattty2.log`, `exit=0`): `slave 136/7, want tty_nr 34823`,
`session 11 pid 11`, `pgrp 12`, `(11 -> 12)` on both files.

### Counterfactually 8 of 12 red

`git stash push -q -m cf-stat-tty -- crates/abi/src/lib.rs src/sched/runtime/procfs.rs
src/vfs/procfs.rs`, both kernel and loader rebuilt, `build/boot-stattty-cf1.log`:

```text
FAIL:  the child acquired the terminal (slave 136/7, want tty_nr 34823)
FAIL:  tty_nr is the recoded device id, not its st_rdev (0, want 34823)
FAIL:  and the answer is this pair's device, not a literal (0 vs 136:7)
FAIL:  and tpgid is the terminal's foreground, which it just took (tpgid -1, pgrp 11)
FAIL:  its session is the leader's, its group its own (session 12, pgrp 12, leader 11)
FAIL:  and its tpgid is the terminal's, not its own group (tpgid -1, pgrp 12)
FAIL:  moving the foreground moves the grandchild's tpgid (-1 -> -1)
FAIL:  and the leader's file follows the terminal too (-1 -> -1)
```

The `session 12 … leader 11` line is the one this entry is about: the old file printed the
grandchild's own group in the session column, and nothing else in the run would have said so. The
four green-on-both claims are the honest boundary — no terminal still reads `0`/`-1`, and the
grandchild still shares its leader's `tty_nr` (both `0` before the fix).

### Gate

`build/gate-stattty2.log`, `GATE EXIT=0`, both arms in one command with the lane count forced to two:
stage 1 `1265 tests, 0 failed` (53.01 s — the +2 are `tty_nr`'s host test in the `abi` leaf and the
`stat` rendering test in `vfs::procfs`); x86_64 `pass=293 fail=0 (293 built)` over 2 lanes in 53.37 s
(-23.1%), `mpos-kunit 35 passed, 0 failed`; aarch64 `pass=292 fail=0 (292 built)` over 2 lanes in
188.25 s (-14.8%), `mpos-kunit 35 passed, 0 failed`, `16 not run - see tests.toml`. The new program
ran on both arms: `build/test-lane-x86_64-1/serial.log:1442` and
`build/test-lane-aarch64-0/serial.log:1573`.

**The first gate run failed, and not on the machine.** `build/gate-stattty.log` came back
`GATE EXIT=1` with every kernel-side number green (`mpos-kunit 35 passed`, `pass=293 fail=0`,
`exit 33`) — the red was the *host user suite's build*: this gate compiles `user/tests` for
`x86_64-unknown-linux-gnu` as well as musl, and `libc::ioctl`'s request parameter is `c_int` in the
musl bindings and `c_ulong` in the gnu ones. A cast that satisfies one fails the other, which is why
every existing probe in this tree that needs an ioctl goes through `libc::syscall(SYS_ioctl)` —
`t-pty`'s helper spells the reason out. The pin now uses that helper. The lesson is not "cast
wider": it is that **a program the gate builds two ways has to compile both ways**, and a
machine-only build (`cargo build -p mpos-user-tests --target x86_64-unknown-linux-musl`) cannot see
it.

### Named gaps this does not close

- **`tpgid` for a task whose terminal has no foreground group** reads `-1`, which the measurement
  above does not cover on Linux (a pty with no `tcsetpgrp` yet). It is the same word a terminal-less
  task prints, so the two cases are indistinguishable from the file.
- **`stat`'s field 8 is a `int` in Linux, so a group id past `INT_MAX` would wrap.** This kernel's
  ids come from a task table, so the case is unreachable here and untested rather than handled.
- **`tty_nr` recoding is `new_encode_dev`; the "old" encoding Linux used before 2.6.25 is not
  produced.** Nothing that runs here asks for it.

### The rule

*A column that copies its neighbour is a column nobody can test.* Field 6 was wrong for as long as
`setsid` worked, and it stayed invisible because the process that reads a file is almost never the
process that called `setsid`. A literal is at least honest about being one; a copy looks like an
answer.

## §6.130 `/proc/<pid>/stat`'s minor-fault columns are counters, and `getrusage` reads the same book (LANDED 2026-11-11)

Fields 10 and 11 of `stat` are `minflt` and `cminflt`; offsets 64 and 72 of `struct rusage` are
`ru_minflt` and `ru_majflt`. All four were written as constants — `Cow::Borrowed("0")` in the file,
an untouched zero in `write_rusage`. Unlike §6.129's literals these were not *wrong* about a machine
that never faults, which is exactly why they lasted: a process's own minor faults are dominated by
the ones it took starting up, and nothing this kernel runs reads its own count differently for
having thirty thousand.

What made them fixable was that the counting half already existed in the shape of a counter:
`Mm::nvcsw`/`nivcsw` (§6.105) proved the pattern — an `AtomicU64` on the address space for the group,
one on the task for the thread, and a reap that folds a dead child's total into the parent.

### What Linux answers — measured

`build/scratch/statflags/` on the host (7.2.14). `field(pid, n)` is the zero-based index `n - 3` of
the split that starts after the **last** `)`; the first attempt in this session indexed from field 1
and read one column to the left everywhere.

```text
j.c  at start                                  minflt=4997 majflt=0
     after mmap of 3 MiB (untouched)            minflt=4997
     after writing 3 pages                      minflt=4999
     after writing 3 more pages                 minflt=5000
k.c  child's own minflt 23 -> 282 for 256 COW writes (delta 259)
     parent's minflt 5257 -> 5266 (delta 9) -- child COW does not appear here
m.c  48 fresh stack pages:                 minflt 5003 -> 5051 (delta 48)
l.c  parent at start                       minflt=5002 cminflt=4312
     a child reporting minflt=99, then reaped: minflt=5008 cminflt=4411
     RUSAGE_CHILDREN: minflt=4411 majflt=0
o.c  struct rusage offsets: 32 = maxrss, 64 = minflt, 72 = majflt
     RUSAGE_SELF 5227 / RUSAGE_THREAD 4557 / RUSAGE_CHILDREN 4281
r.c  worker faulted 400 pages in its own thread; main faulted none
     /proc/<pid>/stat            5401
     /proc/<pid>/task/<leader>   4525
     /proc/<pid>/task/<worker>    403
     /proc/<worker-tid>/stat     5402     <- the tid spelling is the GROUP's
```

Five facts, and each one is a separate thing the file has to get right:

1. **Mapping is not faulting.** 3 MiB untouched moved nothing; three pages moved two.
2. **The count is the kernel's, not the caller's.** Frame allocation is batched, so a claim of "one
   per page" is a claim about the wrong machine.
3. **Two accounts, not one.** `/proc/<pid>/stat` and `/proc/<tid>/stat` read the group's count;
   `/proc/<pid>/task/<tid>/stat` reads that thread's own. A worker's 400 faults left the leader's own
   column at its old figure.
4. **`cminflt` is `RUSAGE_CHILDREN`.** One reap moved both to `4411`, and no other event moves either.
5. **A copy-on-write split and a grown stack page are minor faults**, charged where they happen.

`majflt` and `cmajflt` (fields 12, 13) stayed zero, and that is still honest — see the gaps below.

### What changed

- **`Mm` gained `minflt`/`cminflt`** (`src/sched/runtime/child_mm.rs`), the group's own count and what
  its reaped children added, plus `charge_minor_fault()`, `minor_faults_own()`,
  `minor_faults_both()`, `absorb_child_faults()` and `has_child_faults()` — the same five shapes
  `nvcsw`/`nivcsw` already had. `Task` gained `minflt` for the thread's own column, initialised at all
  six construction points.
- **A fault is charged at the three places one is taken**, not at the page-fault entry: `populate_in`
  for an anonymous frame, `split_cow` for a copy-on-write split, `resolve_stack_growth` for a grown
  stack page. Charging at the entry would count the frame-backed case, which this kernel cannot
  classify — see the gaps.
- **The thread's column is charged beside the group's**, at those same three sites. `split_cow` has no
  `Table`, so `resolve_cow_fault` charges the thread after the split returns; `resolve_stack_growth`
  had to finish its mutable borrow of the task before calling the charging helper — the borrow
  checker found that, not a review.
- **The reap folds.** `reap` already collected `(child, cpu, switches)` per dead task; the tuple grew
  a `faults` word and `absorb_child_faults` adds it to `cminflt`. `Retired` and `WaitEvent::Exited`
  carry `minflt` so a `wait4`/`waitid` caller gets the same number `getrusage` would.
- **`TaskSnapshot` carries three**, not two: `minflt` (group own), `cminflt`, and `minflt_own` (this
  thread). Fields 10 and 11 pick by the `whole` flag `cpu_fields` already used, so `/proc/<pid>/stat`
  and `task/<tid>/stat` read different columns without the file knowing anything about threads.
- **`getrusage` splits three ways.** `sched::current_faults()` answers `(group own, group children,
  thread own)`; `RUSAGE_THREAD` takes the third, `RUSAGE_CHILDREN` the second, everything else the
  first. `write_rusage` now fills `wire[64..72]` and `wire[72..80]`.

### The pin, and the four things it got wrong first

`user/tests/src/bin/t-stat-faults.rs`, 14 claims. Host 14/14 `PASS`; machine 14/14 `PASS`
(`build/boot-faults4.log`); counterfactually **5 red** of 14 (`build/boot-faults-cf2.log`) — the
faulting claims and the child's own reading, which is what a kernel with four literals can answer.
Both arms green in `build/gate-faults2.log`.

Four claims were wrong before they were right, and all four for the same reason: **a counter that
the test itself charges is not a constant the test can compare against.**

- *Equality between the two spellings* failed by one (`Some(31) / Some(32)`) because opening and
  reading the file allocates in the measuring process. The claim became `|difference| <= 4`.
- *That wider bound then failed on the gate's loaded lane by six* (`Some(1) / Some(7)`) — the
  `task/<tid>/` path resolves two directory levels where the process spelling resolves one, so the
  control case pays more than the case it is a control for. The two readings are now six samples
  each of the smallest value seen, compared within sixteen: measurement cost only ever adds, so the
  minimum is the count with the tax taken off, and what remains is slack for the path's own depth.
- *A `Vec` as the untouched mapping* failed because a `Vec` of 3 MiB can be cut out of heap the
  process has already touched, so writing it faults nothing and the measurement reads as a kernel
  that does not count. The fixture became `mmap` + `write_volatile` per page (`struct Fresh`), which
  has no history.
- *The `cminflt` bound was `a >= b + seen`*, where `seen` was the child's own reading — so a kernel
  that counted nothing passed by reporting nothing. The bound is now the 64 pages the child was told
  to touch; the child's own reading is asserted separately, where a zero is already red.

The same gate run also caught a **stale contract in another test**: `t-wait4` asserted that eleven
of `struct rusage`'s counters were zero, and `ru_minflt` had just stopped being one of them. That
claim now covers the block-I/O and IPC counters (`counters[1..4]` and `counters[5..12]`) and asserts
`ru_minflt > 0` beside them — the third time this session a served field had to be moved out of an
"unmeasured" set (§6.105, §6.113 and now here), which is the cost of writing a shrinking set as a
range rather than as a list of names.

Reading the worker's `task/<tid>/` file had the same shape as `statflags/o.c`'s trap: it has to be
read **inside** the worker and before it returns, because a returned thread has no `task/<tid>/` left
to read.

### Named gaps this does not close

- **`majflt`/`cmajflt` (fields 12, 13) and `ru_majflt` stay `0`.** A major fault is one the kernel
  answered by reading the backing device, and nothing on this kernel's frame path carries that fact —
  `MappedObject::frame_at` answers *which frame*, never *how it got there*
  (`src/mm/vma.rs:56`). Splitting them would need a hit/miss signal on the block cache, which is a
  feature and not a field. The zero is honest: this machine's only file-backed frames come from an
  image that is resident, so a real major fault is not a case it can reach either.
- **Frame-backed faults are not counted at all.** `populate_in`'s `VmaKind::Frames` branch charges
  nothing, because the same missing signal is what would decide whether it was minor. A file-backed
  page brought in without I/O is a minor fault on Linux; this kernel cannot tell, and counting it as
  one would be the guess the "refuse rather than approximate" rule forbids.
- **The per-page count is not Linux's.** Linux batches frame allocation, so three pages can cost two.
  This kernel charges once per taken frame, which is its own machine's truth; the pin asserts movement
  and never a page-for-page delta.

### The rule

*A counter the reader cannot compare against itself is a counter with no pin.* Every wrong version of
this fixture compared two numbers that the act of measuring moved — and the file it was measuring was
the one whose reading cost the fault. The claim had to become a bound or a movement, and the two
literals that had stood for four phases were never once asked to move.
*A counter the reader cannot compare against itself is a counter with no pin.* Every wrong version of
this fixture compared two numbers that the act of measuring moved — and the file it was measuring was
the one whose reading cost the fault. The claim had to become a bound or a movement, and the two
literals that had stood for four phases were never once asked to move.

## §6.131 `/proc/<pid>/stat`'s flags is a task-state bitmask, and every input was already there (LANDED 2026-11-11)

Field 9 of `stat` is `flags`, and this kernel printed `0`. It is the last of the columns §6.101–§6.130
worked through, and unlike the counters before it this one is not a number at all: it is a bitmask of
what the task *is* at this moment, which is why a literal cannot be approximately right.

### What the reference host prints

`build/scratch/statflags2/flags2.c` and its siblings, at the raw `/proc` level, decoding field 9 as a
**decimal** word (`flags3.c` first read it as hex and produced `0x4194304`, an artefact of the probe,
not of the kernel):

```
self (exec'd by the shell)      flags=0x400000
forked, never exec'd child      flags=0x400040
forked, then exec'd child       flags=0x400000
zombie killed by SIGKILL        flags=0x40844c  state=0
after reaping it                field9=-1
child stopped by SIGSTOP        flags=0x400040  state=0
task/<tid> spelling of it       flags=0x400000
```

`flags4.c` runs the same program under `setarch -R` (ASLR off) and every word loses exactly `0x400000`;
`flags5.c` separates the deaths: `SIGTERM` gives `0x40840c` and `SIGSEGV` `0x40864c`, a difference of
`0x200`; an exec'd child that is merely stopped reads `0x0` with ASLR off — **a stop is not a death**.
`flags3.c` reads a worker thread's own `task/<tid>/stat` as `0x400040` while its leader reads
`0x400000`: the thread was made by `clone` and has never exec'd.

So the word is five facts: `PF_FORKNOEXEC` (`0x40`, set between fork/clone and exec), `PF_RANDOMIZE`
(`0x400000`, a property of the machine's layout), the death bits (`0x4|0x8|0x8000`), `PF_SIGNALED`
(`0x400`, only when a signal killed it) and `PF_DUMPCORE` (`0x200`, only when that signal was a
core-dumping one). `build/scratch/statflags2/nolimit.c` confirms the dump bit follows the *signal*, not
the file: `RLIMIT_CORE` 0 and unlimited both read `0x40864c` and both report `WCOREDUMP`, which is the
same rule `wait_status` already encodes (§6.112).

### Why this was contained rather than a feature

Nothing new had to be counted. All four inputs existed:

* **forked-not-execed** is `Task::user_resume.is_some()` — `src/sched/runtime/task.rs:1126`, set by
  `fork`, cleared at exactly one place, `src/sched/runtime/child_exit.rs:1748 task.user_resume = None`,
  which is the exec path;
* **dead** is `Task::exited` (`src/sched/runtime/task.rs:1110`);
* **signalled/dumped** come from the wait-status word already computed for field 52 — `retired_status`
  (§6.112) — so `signalled` is `(status & 0x7f)` neither 0 nor `0x7f`, and `dumped` is its `0x80` bit;
* **randomized** is this kernel's own ASLR, which it really does (`src/sched/runtime/task.rs:392 fn
  random_pages`, `:454 fn new_layout`, called from three places), so it is reported as `true`: honest
  for every image this tree builds, and the reason it is a named gap rather than a proof.

The encoding is one leaf function, `pub const fn stat_flags(forked_not_execed: bool, dead: bool,
signalled: bool, dumped: bool, randomized: bool) -> u32` in `crates/abi/src/lib.rs`, beside `tty_nr`
(§6.129) and `wait_status` (§6.112) and for the same reason: `sched` and `vfs` both need the number and
neither should own the definition.

### Two arguments with my own test, both wrong in the same direction

The leaf's unit test is a set of literals copied from the host's output, so it is only as good as the
copy. Two mistakes surfaced while writing it, and both were the test being wrong rather than the
function:

1. The first version passed `forked_not_execed = false` for the `SIGTERM` and `SIGSEGV` zombies, and
   failed with `left: 34316, right: 34380` — `0x860c` against `0x864c`. Re-deriving all four measured
   words showed every one of them carries `0x40`: `flags2.c`'s zombies are children that forked and
   never exec'd, so the bit is rightly set. The function was right and the expectation was not.
2. The first constants were `PF_DEAD_BEGIN`/`PF_DEAD_END` covering only `0x4|0x8`, which made every
   dead word `0x8000` short. The bit is real and in every dead task's word in every probe, but this
   repository cannot verify Linux's name for it, so it became one `PF_DEAD = 0x0000_800c` with the
   naming gap recorded in the doc comment rather than guessed at.

### The pin, and the trap that cost the most time

`t-stat-flags` has 16 claims: a live process is not zero and carries no fork or death bit; a forked
child carries `PF_FORKNOEXEC` and *only* that bit more than its parent; a stopped child carries no death
bit and still carries the fork bit; `SIGTERM` sets `PF_SIGNALED` and not `PF_DUMPCORE`; `SIGSEGV` sets
both and the difference between the two words is exactly `PF_DUMPCORE`; a worker thread carries the fork
bit while its leader does not; a reaped child has no file.

The fixture's hardest bug was not in either kernel. The `SIGSEGV` claim hung **the host** — not failed,
hung — and `strace -f` showed `kill(…, SIGSEGV) = 0`, the child still alive, and the parent parked in
`wait4`. The cause is that **`fork` copies signal dispositions**: Rust's standard library installs a
`SIGSEGV` handler for stack-overflow reporting, the child inherited it, the signal was caught, the
handler returned, and the child went back to `pause()`. `build/scratch/statflags2/inherit.c` isolates
it: without a reset the child survives the signal, with `signal(SIGSEGV, SIG_DFL)` it dies with
`signaled=1 sig=11 core=128`. The fixture now resets the disposition explicitly, and says in a comment
why a test that wants the default death has to ask for it.

Counterfactually the pin is **7 red of 16**, all reading `Some(0)`: the live word, the fork bit, the
death bits, `PF_SIGNALED`, `PF_DUMPCORE`, the stop-is-not-an-exec claim, and the thread's fork bit. The
nine that stay green either way are the negative halves — "no fork bit here", "no death bit on a live
task", "no file after the reap" — which a literal zero satisfies, and that is the honest boundary of
this shape.

### The rule

*A bit is a claim about now, and a zero is a claim about never.* The counters before this one were
wrong in a way only a busy process noticed; this one was wrong for every process at every instant, and
it went unseen because a reader that does not compare the column cannot tell a mask from a zero.

## §6.132 `sched_rr_get_interval`'s blocked child is a machine claim, because the host's is not stable (LANDED 2026-11-11)

§6.131's gate went red on one line, on one arm:

```
FAIL:  a child blocked in a read is answered zero (state S, got 0, 4000000 ns)
```

`state S` and `4000000 ns` in the same line is the shape the pin existed to catch — §6.127 built this
door to answer zero for a task that cannot run, and here a blocked child was answered the slice. The
same line had reddened §6.127's own gate a few hours earlier, which by this session's standing rule
(a flake re-measured is a bug) meant it had to be chased down rather than re-run.

It was not a bug in the kernel. It was a bug in the claim, and the reference host is what proved it.

### The measurement

`build/scratch/rrstate/p.c` is the pin's fixture transplanted to the host: fork a child that blocks in
a `read` on a pipe nobody writes to, sleep 150 ms, then per iteration read `/proc/<pid>/stat` field 3,
call `sched_rr_get_interval`, and read field 3 again.

```
DISAGREE  S/S -> 2000000 ns
samples=20000  S/S answered zero=19872  S/S answered a slice=128  state moved=0  other=0
```

128 of 20000 samples answered the 2 ms slice for a child whose letter read `S` **both before and after
the call**. `q.c` asked only the letter, 20000 times, and got `S` every time — so "the letter moved to
`R` between the reads and back" is not the explanation. `r.c` classified the same way over three
30000-sample runs under different loads:

```
n=30000 zero=24414 slice=5586 (of which both-reads-S=5586, letter moved=0)
n=30000 zero=29311 slice=689  (of which both-reads-S=689,  letter moved=0)
n=30000 zero=28015 slice=1985 (of which both-reads-S=1985, letter moved=0)
```

Every slice was answered while both reads said `S`, and the letter never moved. The rate itself is a
function of load: 2% to 19% depending on what else the box was doing.

`build/scratch/rrstate/s.c` then sampled the blocked child *beside* a spinner in the same window, eight
runs of 200 samples each:

```
blocked zero=200 slice=0 | spinner zero=0 slice=200
blocked zero=164 slice=36 | spinner zero=0 slice=200
```

and `z.c` sampled a zombie 200 times per round over five rounds:

```
round 0: zombie zero=200 slice=0
round 3: zombie zero=0 slice=200
```

The runnable and the stopped child are stable (`w.c`: 5/5 rounds `stopped zero=200 slice=0`, 5/5 rounds
`spinner zero=0 slice=200`). The blocked child and the zombie are not: their answers come in whole
runs of one shape or the other, and a 200-sample run here once read 0 zeros and 200 slices.

### Why the host is allowed to do that, and what the ABI actually says

`/proc/<pid>/stat`'s letter and `sched_rr_get_interval`'s answer are two reads of one moving record.
Every one of the host's slices was answered while the letter said `S`, so the disagreement is not a
race the reader lost — the kernel's own `task_is_running`-style test at the instant of the call is a
different instant from the `/proc` render, and for a `TASK_INTERRUPTIBLE` task the two can differ for
as long as a scheduler tick. Sampling strategy cannot fix that: with a 19% rate at the top end and 0%
at the bottom, no threshold over one letter and one number is stable, and a threshold picked to pass
the loud case fails the quiet one.

What *is* an ABI promise is the asymmetry, and it needs a control in the same window to be a claim at
all: a target that can run is never answered zero, a target that cannot run is answered zero far more
often than not. The first half held in 8/8 pairable runs and 5/5 rounds; the second half is what moves.

### The fix, and where each half is pinned

- `t-rr-interval`'s blocked-child claim is **machine-only** (`!host_run()`), and asserts `blocked_zero
  == SAMPLES` — 200 of 200. That is legitimate because this kernel's answer is not a sample of a moving
  record: `handle_sched_rr_get_interval` asks `task_state` inside the same table lock that decides the
  answer, so there is no window between the letter and the number. `build/scratch/rrstate/m.c` read
  1500 of 1500 zeros on the machine.
- The **paired** claim replaces it as the host-visible one: the runnable child beside the blocked one
  must read `runnable_zero == 0 && runnable_slice == SAMPLES`, and the blocked child's counts are
  printed in the same line so a reader sees them without a second run. That line fails a kernel that
  answers one constant to everything — which is what this kernel did before §6.127.
- The **zombie** claim is machine-only for the same measurement: `z.c` read 0 of 200 zeros in one host
  round and 200 of 200 in others.
- The stopped child's claim stays unconditional: 5/5 rounds stable on the host, and the machine agrees.

### The rule

*Two reads of one moving record are not an invariant, and a pin that needs a control in the same
window is telling you which half is the contract.* The direction is the promise; the rate is the
host's load average wearing a syscall's name.

## §6.133 `read`/`write` ask about the descriptor before they ask about the buffer (LANDED 2026-11-11)

`read(-1, bad_pointer, 8)` has two things wrong with it and only one answer. Linux answers `EBADF`:
the descriptor is resolved first, and a caller that got its buffer from an arithmetic bug is told
about the descriptor it also got wrong. This kernel answered `EFAULT`, because all four of
`handle_read`/`handle_write`/`handle_readv`/`handle_writev` validated the caller's buffer in their
first statement — before they had looked at the descriptor at all. A caller told to fix a pointer
never learns that the number it passed is not open.

### What Linux does, measured

`build/scratch/rworder/{o,z,p,v,w,x,y,z2,copy,copy2}.c`, a current host (Linux 7.2.14), `BAD =
(void *)1`, every call at the raw-syscall level:

```text
read(-1, BAD, 8)        = -1 errno=9    (EBADF)
read(fd, BAD, 8)        = -1 errno=14   (EFAULT)
read(dir, BAD, 8)       = -1 errno=21   (EISDIR)   <- the descriptor wins
read(dir, BAD, 0)       = -1 errno=21              <- kind is judged before the length
read(mut, BAD, 0)       =  0 errno=0               <- length zero short-circuits the buffer
read(-1, BAD, 0)        = -1 errno=9               <- but never the descriptor
write(-1, BAD, 8)       = -1 errno=9
write(rw=/dev/null, BAD, 8) =  8 errno=0           <- the node copies nothing
readv(-1, bad1, 1)      = -1 errno=9               <- descriptor before the array
readv(mut, bad1, 1)     = -1 errno=14
readv(dir, BAD, 1)      = -1 errno=14              <- the array before the kind
readv(dir, one, 1)      = -1 errno=21
readv(-1, one, 1025)    = -1 errno=9               <- descriptor before the count
readv(mut, one, 1025)   = -1 errno=22              <- the count before the array
readv(mut, BAD, 1025)   = -1 errno=22
pread64(ro, BAD, 8, -1) = -1 errno=22              <- the offset before everything
pread64(-1, BAD, 8, 0)  = -1 errno=9
pread64(ro, BAD, 8, 0)  = -1 errno=14
pwrite64(mut=/dev/null, BAD, 8, 0) = 8 errno=0
write(null, BAD, 8)     =  8 errno=0
write(full, BAD, 8)     = -1 errno=28              (ENOSPC, still no dereference)
read(null, BAD, 8)      =  0 errno=0
read(empty pipe, BAD, 8)=  0 errno=0
read(zero, BAD, 8)      = -1 errno=14              <- /dev/zero does write the buffer
```

Four questions in a fixed order, and the answers are separable because the host distinguishes them:

1. **the offset**, when the call has one — `offset < 0` is `EINVAL` before the descriptor is even
   resolved (`pread64(-1, BAD, 8, -1)` is `EINVAL`, not `EBADF`);
2. **the descriptor** — `EBADF`, including the wrong mode (`O_WRONLY` read, `O_RDONLY` write) and
   including length zero;
3. **the kind** for the scalar doors (`EISDIR`), but *after* the iovec array for the vectored ones;
4. **the caller's buffer** — and only a node that actually moves bytes ever asks. `/dev/null`'s
   write answers its length without reading the buffer, `/dev/full` answers `ENOSPC` without reading
   it, an empty pipe's read answers 0 without writing it. `/dev/zero` does write it, and faults.

### Why this kernel was wrong, and what the fix moved

The VFS layer was already right and said so in a comment: `File::read` checked readability, then
`refuses_file_io`, then `NodeKind::Dir`, and only then looked at `buf.is_empty()`. The syscall doors
above it jumped the queue with `buffer_ok(...)` in their first statement, so the layer that knew the
order never got asked. Two things were missing rather than wrong:

- the prefix was **inlined** in `File::read`/`File::write_reporting`, so no caller could ask for it
  without also performing the transfer;
- there was no way to ask for **just the mode**, which is what the vectored doors need, because Linux
  judges their array before their kind.

So the fix splits the prefix out of both:

- `File::pre_read` / `File::pre_write` (`src/vfs/file.rs`) hold the whole scalar prefix in Linux's
  order; `File::read` and `File::write_reporting` now open with `self.pre_read()?` /
  `self.pre_write()?`, so the two doors and the two transfer paths cannot drift apart.
- `crate::vfs::pre_read`/`pre_write` (the full prefix) and `pre_read_mode`/`pre_write_mode` (mode
  only) are the two questions the syscall layer can ask, re-exported from `src/vfs/mod.rs`.
- `handle_read`/`handle_write` ask `pre_read`/`pre_write` first and move `buffer_ok` to after the
  zero-length short-circuit; `readv_inner`/`writev_inner` ask the *mode* first, then the count, then
  the array; `handle_pread64`/`handle_pwrite64` keep the offset first and then ask the full prefix;
  `io_at_position` (the `preadv`/`pwritev` pair) asks the mode after the offset, and the count and
  array follow inside `readv`/`writev`.
- `abi::iov_count_verdict` (`src/syscall/linux/abi.rs`) is the count question, host-tested: the
  register carries a `usize`, so a caller's `-1` arrives as `usize::MAX` and a plain `> 1024` test
  calls it too large rather than negative. The answer is the same `EINVAL` either way — by accident,
  which is not a rule. The verdict reads it as `i32`, keeps `0..=1024`, and returns the count so
  every caller uses the vetted number rather than re-deriving it.

### The pin, and the claim that had to become an observation

`user/tests/src/bin/t-rw-precedence.rs`, eleven claims, each written as an `(errno, value)` pair
because the errno alone does not say whether the call did any work: descriptor-before-buffer for
`read` and `write`, directory-before-buffer, buffer-is-`EFAULT`-when-the-descriptor-is-good, the same
call with a real buffer, both zero-length `EBADF`s, descriptor-before-array for `readv`, array-is-
`EFAULT` when the descriptor is good, and the same `readv` with a real vector.

Two of them moved during the work:

- the real-`readv` claim was `got.1 == 8`; on the machine it reads 2, because the scalar read before
  it already advanced `/proc/uptime`'s position. A door promises a short read, not a particular
  count, so the claim is now `got.1 > 0 && got.1 <= 8`.
- the `/dev/null` claim started as `write(rw, BAD, 8) == (0, 8)`, which is Linux's answer and this
  kernel's *named gap* (§6.133, below). It is now a printed observation that accepts both answers,
  so the divergence stays visible in the log instead of being hidden by a document. A claim that
  passes on both kernels pins nothing, and pretending otherwise would be the worse error.

Counterfactually, with the four source files reverted and both binaries rebuilt, **4 of 11 are red**:
the three descriptor-before-buffer claims and `readv`'s descriptor-before-array — every one reading
`(14, -1)` where Linux answers `(9, -1)`/`(21, -1)`. The other seven are green on both kernels and
are controls, not evidence.

### The named gap this leaves

`write(/dev/null, BAD, 8)` still answers `EFAULT` here where Linux answers 8, and the reason is not
the door: this kernel validates the caller's buffer **once, up front**, and a node that copies
nothing still passes through that validation. Closing it means moving the fault-in to the copy site —
every reader and writer would own its own faulting copy, which is a VFS-wide change and not a field.
It stays named rather than approximated, and the pin's observation line is what will turn green when
it closes.

### The rule

*Which question a door asks first is part of the answer it gives.* Two things can be wrong at once
and the caller branches on the errno; a kernel that judges the cheap argument first hands back a
refusal about something the caller had not yet committed to.

## §6.134 `/proc/<pid>/wchan` is the wait a task is in now, and the sweep's root had no room to say so (LANDED 2026-11-11)

`/proc/<pid>/stat`'s field 35 records the *last* park a task entered - it is the stuck-machine
instrument, and it is meant to stay set after the task wakes. `/proc/<pid>/wchan` is the other
question: what is this task waiting on *now*. This kernel had no such file at all, and the two
answers are measurably different (host 7.2.14, `build/scratch/wchan/`):

```
running (self)                     n=1  text="0"
blocked in a pipe read             n=14 text="anon_pipe_read"
blocked in nanosleep               n=17 text="hrtimer_nanosleep"
SIGSTOPped                         n=14 text="do_signal_stop"
ptrace-stopped                     n=11 text="ptrace_stop"
zombie (unreaped)                  n=1  text="0"
reaped                             open errno=2 (ENOENT)
spinning (runnable)                n=1  text="0"
```

and the two files on one task that slept and then woke (`stale.c`, `f35.c`):

```
asleep in a pipe read            f35=1   wchan="anon_pipe_read"
woken, running                   f35=1   wchan="0"      <- field 35 is stale, and right to be
zombie                           f35=1   wchan="0"
```

Three shapes the file has, each measured:

* **No trailing newline.** The content is the word and nothing else - 14 bytes for
  `anon_pipe_read`, 1 for `0`. `/proc/<pid>/comm` ends in `\n`; this file does not (`perm.c`).
* **A target the caller may not inspect reads `0`, not `EACCES`.** `perm3.c`: a child dropped to
  uid 65534 and `pid 1` both read `0` from an unprivileged parent. The same target makes
  `maps`/`smaps` answer `EACCES`, so refusing here would be copying the wrong neighbour's rule.
* **Per-thread, not per-group.** `threaded.c`: with a worker blocked in a pipe read and the leader
  running, `/proc/<pid>/wchan` and `task/<leader>/wchan` both read `0` while `task/<worker>/wchan`
  reads `anon_pipe_read`.

### The change

`src/sched/wchan.rs` (new, host-compiled) holds the vocabulary: `pub enum Wait { Nothing,
Parked(u8), GroupStop, TraceStop }`, `wchan_word`, and `PARK_SITES` - the park-site table **moved
out of `src/sched/runtime/task.rs`**, because the stall dump and this file were about to be two
readers of one list. `src/sched/runtime/procfs.rs` answers it as `task_wait(id)`: an exited task is
`Nothing`, an uninspectable one is `Nothing` (not a refusal), a trace-stop is `TraceStop` or
`GroupStop`, a sleeping task is `Parked(park_site)`, and a running one is `Nothing`. `src/vfs/procfs.rs`
adds the file at both spellings - `process_dir` and `thread_dir` - through one `wchan_text`.

### The pin (`user/tests/src/bin/t-wchan.rs`, 12 claims)

Shape and movement, not this kernel's vocabulary: the file exists; carries no newline; a running
task reads `0`; a child parked in a pipe read names a wait that is neither empty nor `0`; a sample
of the two files together finds the task asleep *with* its wait named; `stat` field 35 records a
number for the same park; both spellings exist and agree; a zombie reads `0`; a collected task has
no file; a task the caller does not own is answered rather than refused; and every word is either
`0` or a single name with no space.

Two claims moved while the pin was being written, both because the machine disagreed with the host:

* **"a task we may not inspect reads 0"** was false on the machine: the payload runs as root, so it
  *may* inspect `pid 1` and reads that task's real wait - which is also Linux's answer for root. The
  claim that holds for any caller is the rule itself, "answered, not refused", plus "the word is one
  of the two Linux has".
* **"and it is asleep, not running, while it says so"** was a race, not a state. This kernel's pipe
  read is a *timed* park, so a blocked reader alternates between parked and briefly runnable;
  sampling `stat` and `wchan` in two reads caught `block_until_nanos` beside `R`. It now samples the
  pair until a sample agrees, which is the property the two files exist to describe and not a
  promise a timed park makes.

Host 12/12; machine 12/12 (`build/boot-wchan4.log`); counterfactual with the file removed and the
scheduler changes stashed, `7/12` red (`build/boot-wchan-cf1.log`) - every `None` answer: the file
absent, the running read, the parked child, the thread spelling, the two spellings agreeing, the
zombie, and the uninspectable target.

### The gate found the second bug, and it was not in this change

The first both-arm gate went red on `t-mmap-file-lazy` on aarch64 with `FAIL: the file is written in
full`. The arithmetic is exact: `build/aarch64-sweep-ext4.img` was a 196 MiB filesystem holding a
196.58 MiB tree, leaving **485 free blocks = 1.89 MiB**, and that program writes 512 pages (2 MiB) of
scratch under `/`. Adding this pin's 423 KiB binary pushed it over. The pin had existed since
`4930158` and no gate had reached it, because the earlier sweeps did not stage it.

The cause is a constant that drifted from its contents: `SWEEP_DISK_BYTES` was `96 MiB` with a
comment saying the payload was "~57 MB today", and the payload is now 111.82 MiB of `/bin` in a
196.58 MiB tree. `mkfs.ext4` only refuses when the tree does not fit at all; a root that fits
*exactly* leaves nothing for what a test writes, and that is the failure that was invisible.

The fix sizes the filesystem from the tree (`xtask/src/disk.rs`): a new `tree_bytes` walks the
staging tree summing file lengths plus one block per directory, `ROOT_SLACK_BYTES` (32 MiB) is the
room a writable root leaves over, and `root_bytes = disk_bytes.max(tree_bytes + ROOT_SLACK_BYTES)` -
so the caller's constant is a **floor** rather than the answer. The medium's fingerprint still
carries the caller's `disk_bytes` and not the measured one: the tree is a function of the entries,
overlays and archive the fingerprint already names, so a measured size in it would make every run
stale against a value it cannot recompute before deciding. After the fix the same image holds 58598
blocks with **8371 free (32.7 MiB)**, and `t-mmap-file-lazy` passes on both machines.

Host test `disk::tests::a_roots_size_is_measured_from_the_tree_and_leaves_room_over` pins the
measurement: a tree of two files (10 MiB and 4097 B) in three subdirectories measures its files plus
one block per directory, rounded to blocks, and a floor below the tree is raised rather than honoured.

### The rule

*A column that says "now" and a column that says "last time" are two answers, and a machine with
only the second is missing the one a debugger reads first.* The gate's second finding is the other
half of the lesson: a size constant with a comment saying what the payload was is a measurement with
an expiry date, and the way to keep it honest is to derive it from the thing it is supposed to hold.

## §6.135 `/proc/<pid>/io` reports the bytes and calls a task made (LANDED 2026-11-12)

This kernel had no `/proc/<pid>/io` at all (`grep -c '"io"' src/vfs/procfs.rs` was 0), which meant a
program asking how much it had read got `ENOENT` where Linux gives it seven numbers. The file is one
answering five questions, and the answers are not the ones a kernel gets by counting at the obvious
places (host 7.2.14, `build/scratch/pidio/`):

```
rchar: 2026894      <- bytes this task asked to receive
wchar: 23323        <- bytes it asked to send
syscr: 772          <- read-family calls it made
syscw: 58           <- write-family calls it made
read_bytes: 0       <- bytes that came from storage
write_bytes: 24576  <- bytes that went to storage
cancelled_write_bytes: 0
```

Seven lines, `name: value`, one space after the colon, a newline after every line **including the
last**, no alignment, and the whole file is mode `0400`: `/proc/1/io` read by uid 1000 answers
`EACCES` (`perm.c`).

### What the numbers are

Three rules, each a place a plausible kernel is wrong. Measured with a child performing one call per
round while its parent reads `/proc/<pid>/io` around it (`charge3.c`; the child's own read of its
command byte is the `+1` that every row shares, so the *differences* are the measurement):

```
read(8) from a file           ret=8   drchar=9   dsyscr=2
read(fd, buf, 0)              ret=0   drchar=1   dsyscr=2
read(8) bad buffer (EFAULT)   ret=-1  drchar=1   dsyscr=2
read(dir) (EISDIR)            ret=-1  drchar=1   dsyscr=2
read(-1) (EBADF)              ret=-1  drchar=1   dsyscr=1
readv(2 x 2 bytes)            ret=4   drchar=5   dsyscr=2
write(/dev/null, 8)           ret=8   dwchar=25  dsyscw=3
write(ro fd) (EBADF)          ret=-1  dwchar=17  dsyscw=2
```

1. **`rchar`/`wchar` are the return value, `syscr`/`syscw` are the calls.** A read that answered
   `EFAULT` or `EISDIR` still moved the call column by one and the byte column by zero; one that
   answered `EBADF` moved neither, because it never got past the descriptor. So the charge belongs
   **after** the descriptor and mode checks and **at the point the answer is known** - not at the
   entry of the call, and not inside the VFS layer where a short transfer is not yet a return value.
2. **A vectored call is one call.** Two iovecs of two bytes moved `rchar` by four and `syscr` by one.
   Charging per iovec would be the plausible wrong answer and reads as `syscr` growing with the
   vector's shape.
3. **The group and the thread are two books.** `/proc/<pid>/io` is the process's totals and
   `/proc/<pid>/task/<tid>/io` is that thread's own: a worker that had read 352402 bytes left its
   process's file reading `rchar: 2026894` and its own reading `352402` (`shape.c`). A **child's** io
   does not fold into the parent, unlike `cminflt`/`cnvcsw` - so there is no `crchar`, and the absence
   is the answer: Linux has no such line either.

### The fix

`Task` gains `rchar`/`wchar`/`syscr`/`syscw` (the thread's book) and `Mm` gains the same four (the
group's), charged together by one `sched::charge_io(reading, chars)` so the two doors cannot disagree
about how many bytes there were - `charge_cpu`'s shape. The seven doors that move bytes
(`read`, `write`, `readv`, `writev`, `pread64`, `pwrite64`, and the vectored-at-position pair) were
restructured so the answer is computed into a binding first and charged at the end; the vectored pair
charges once, in `handle_readv`/`handle_writev`, which is also the body `preadv`/`pwritev` run
through.

`/proc/<pid>/io` renders through a pure `io_text`, so the seven lines are pinned by a host test that
compares the whole body byte for byte against the reading above. The file is `0400` through a new
`owner_file` beside `env_file`, which already existed for exactly this reason.

### The three storage lines are a named gap

`read_bytes`, `write_bytes` and `cancelled_write_bytes` print **zero**, and that is a refusal rather
than an answer. They count bytes that actually reached or came from a device, and nothing here counts
**device-layer** bytes per task: `crates/blk`'s `Counted` keeps `reads`/`writes`/`sectors_*` for
`/proc/diskstats`, which is per-device and not per-task, and the ext4 cache knows a fill from a hit
(`begin_fill`) but not how many bytes the fill cost. Linux's own measurement (`storage.c`): a cold
4 KiB read moved `read_bytes` by 4096, a second read of the same file moved it **again**, and
`read(/dev/zero)` moved `rchar` by 8528 while `read_bytes` stayed put. A kernel that guessed would be
printing a number it does not have; the pin asserts these three lines are *present and numeric* and
claims nothing about their value.

### The pin

`user/tests/src/bin/t-proc-io.rs`, 13 claims: the file exists with exactly Linux's seven names in
Linux's order; every value is plain decimal; the last line ends in a newline; the three storage lines
are present as numbers; the thread spelling exists; a child reports; and then **movement** - `rchar >=
returned > 0`, `syscr >= 12`, `wchar >= 1`, `syscw >= 1`, the group's book holding the thread's, and
`/proc/1/io` answered-or-refused by the caller's euid.

Two claims moved while writing it, both because **the fixture charges what it measures**:

- Reading the group's file first made the *thread's* figure exceed it, since reading either file is
  itself a read charged to both books. The fixture now reads the thread's book first, so the group's
  is the superset by construction.
- `readv(0, ...)` on the machine reads fd 0 as the console, which never returns EOF: the first boot
  printed no claim at all and the stall dump showed the task parked in `block_until_nanos` on syscall
  0. Both the vectored read and the refused-by-buffer read now name the regular file.

Counterfactually **10 of 13 are red** (`build/boot-pidio-cf1.log`): all six shape claims, the storage
lines, the thread spelling, and all four movement claims. The three that stay green either way are
honest boundaries - "every value is plain decimal" is true of the empty string, "the group's book
holds the thread's" is true of 0/0, and the `/proc/1` claim takes the root branch on a payload that
is root.

### The gate

```
stage 1  host unit tests   1275 tests, 0 failed            70.04 s
x86_64   pass=298 fail=0 (298 built)  kunit 35/0  total 70.39 s  (+33.1%)
aarch64  pass=297 fail=0 (297 built)  kunit 35/0  total 218.81 s (+13.6%)
```

`t-proc-io` ran on both arms (`=== PASS t-proc-io`). The x86_64 figure is the whole sweep's, not this
pin's: the aarch64 sweep had just been given a root sized from its tree (§6.134), so both arms built
and staged more programs than the previous green.

### The rule

*The bytes a caller was handed and the bytes a device moved are two different questions, and a file
that answers the second with the first is worse than one that refuses it.* The charge point is the
other half: it belongs where the answer is known, which is why a refused call still counts as a call
and a failed transfer still counts as zero bytes. And the fixture lesson is the one this file keeps
teaching - an instrument that reads the counter it is measuring charges what it measures, and the
reading is then true and useless.

## §6.136 `/proc/<pid>/auxv` is the vector the program was handed (LANDED 2026-11-12)

Linux hands a program its auxiliary vector on the initial stack and keeps a copy
in the address space (`mm->saved_auxv`) so `/proc/<pid>/auxv` can report the same
words later. This kernel wrote the vector onto the stack and kept nothing: the
file did not exist, and the only record of what a program was told was the copy
in its own memory, which nothing in the kernel could read back.

Measured on a current host first (`build/scratch/auxv/`, Linux 7.2.14, uid 1000):

```
mode 0400  size 0
read 368 bytes = 23 entries
  type 33 value 0x7f4049bb4000     AT_SYSINFO_EHDR
  type 16 value 0x178bfbff         AT_HWCAP
  type 6  value 0x1000             AT_PAGESZ
  type 17 value 0x64               AT_CLKTCK
  type 3  value 0x556eebed9040     AT_PHDR
  type 4  value 0x38               AT_PHENT
  type 5  value 0xf                AT_PHNUM
  type 7  value 0x7f4049bb6000     AT_BASE
  type 9  value 0x556eebeda0b0     AT_ENTRY
  type 11 value 0x3e8              AT_UID
  type 12 value 0x3e8              AT_EUID
  type 13 value 0x3e8              AT_GID
  type 14 value 0x3e8              AT_EGID
  type 23 value 0x0                AT_SECURE
  type 25 value 0x7fffca0af9e9     AT_RANDOM
  type 31 value 0x7fffca0b1ff4     AT_EXECFN
  type 0  value 0x0                AT_NULL
```

and the vector the program itself reads through `getauxval` agrees with it entry
for entry. Three more readings, each from its own probe:

```
whole read: 368 bytes (23 entries)
pread(16, off 0):  16 bytes type=33      a 16-byte window reads one pair
pread(16, off 16): 16 bytes type=51
pread at end:      0 bytes
st_size:           0                     the length is only known on the read
/proc/1/auxv by uid 1000: EACCES(13)
zombie's auxv:            ESRCH(3)       its mm is gone
/proc/<pid>/task/<tid>/auxv: present, same bytes
```

So the rules are:

* **The file is a read of a stored copy, not a rebuild.** The vector is made
  once, when the program is executed, and what a process reads later is those
  bytes - which is why a `setuid` after `exec` does not move the ids the file
  reports. A kernel that recomputed the vector on the read would answer the
  current ids: the same shape, a different file.
* **The words are native-endian `(u64, u64)` pairs, terminator included**, so the
  body is not text and a `String` cannot carry it.
* **`0400`, `st_size` 0, both spellings.** The vector carries the ids the process
  was executed with, so it belongs to its owner; the length is unknown until the
  file is read; and the thread's directory answers the same bytes because there
  is one copy per address space.

### What this kernel had

`src/sched/runtime/load.rs`'s `auxv()` built sixteen entries - `AT_PHDR`,
`AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`, `AT_ENTRY`, the four ids,
`AT_HWCAP`, `AT_CLKTCK`, the `rseq` pair, `AT_SECURE`, `AT_RANDOM`, `AT_EXECFN`,
`AT_NULL` - and pushed them into the argument block. Nothing kept them, so there
was no second door to open: the file was simply absent.

### The fix

One builder, one copy, four places that hand it over:

* `auxv()` now records what it wrote, into `Loaded::auxv` - the bytes on the
  stack and the bytes the file reports are the same bytes by construction, so the
  two doors cannot drift.
* `Mm::saved_auxv` is the address space's copy, set at the three places an image
  is installed (init, the spawn path, and `exec`) and copied at the one place a
  child inherits an address space (`fork`, beside `set_image_bounds` and
  `set_credentials`, which are inherited for the same reason).
* `/proc/<pid>/auxv` and `/proc/<pid>/task/<tid>/auxv` read it, `0400`.

The file's plumbing needed a change of its own: a generated procfs file's body
was an `Arc<str>`, so the node could carry text and nothing else - and `0x80` in
a `u64` value is not valid UTF-8, so `/proc/<pid>/auxv` could not have been
served by it at all. A `ProcFile` now renders `Arc<[u8]>`, the text helpers
convert their `String` without a copy (so no other file moved), and the two
nodes that were never files of bytes (`/proc/sys`'s knobs, the links) keep their
own text alias. The host test
`a_file_whose_body_is_not_text_is_served_byte_for_byte` pins the node's side of
it, since no host test can reach a real vector.

### The pin

`user/tests/src/bin/t-proc-auxv.rs`, sixteen claims:

* the body exists, is a whole number of pairs, ends in `AT_NULL`, and nothing
  follows the terminator;
* `AT_PAGESZ` is `sysconf(_SC_PAGESIZE)`, `AT_CLKTCK` is 100, and the four id
  words are the process's own;
* `AT_EXECFN` points into a readable mapping and the string there names this
  program; `AT_RANDOM` points at readable bytes; `AT_ENTRY` lands in executable
  code and `AT_PHDR` in readable memory (the ELF header table is in the image's
  first read-only segment - asking for it to be executable fails on a correct
  kernel, which is how the first draft of this claim was wrong);
* the file is `0400` and reports size 0;
* the thread's spelling is the process's bytes;
* a `fork` child that has not `exec`ed reads these bytes;
* a child executed by `/proc/self/exe` reads `/proc/self/exe` back from
  `AT_EXECFN` - the path as it was spelled, which is Linux's `bprm->filename`,
  not the resolved one - and that path is not the one this process was started
  by;
* a collected process has no vector.

Host: 16/16 `PASS`. Machine: 16/16 `PASS`, `AT_EXECFN` reading
`"/bin/t-proc-auxv"` and the four ids `0` (the payload is root).

**Counterfactual**: with the fix stashed and both binaries rebuilt, 15 of 16 go
red - the file is absent, so every claim that reads it fails. The two greens are
the honest boundaries: "and that is not the path this process was started by" is
trivially true when there is no path, and "a collected process has no auxv" holds
for a kernel that never had one.

### Named gaps

* **This kernel's vector is sixteen entries; Linux's is twenty-three.** The file
  reports what this kernel *actually handed each program*, which is the right
  answer for this kernel - `AT_SYSINFO_EHDR` (there is no vDSO),
  `AT_MINSIGSTKSZ`, `AT_HWCAP2`, `AT_PLATFORM` and `AT_FLAGS` are entries the
  loader does not push, and adding them is a change to what a program is handed,
  not to this file.
* **A zombie answers `ENOENT` here where Linux answers `ESRCH`.** Linux refuses
  because the `mm` is gone; this kernel's node reports the same "no vector" a
  collected task does, and the two collapse into one answer. The pin asserts the
  refusal, not the errno.

### The rule

A file that reports what a process was handed must read the copy, not rebuild
it. The stack and the file are two doors onto one vector, and the only way they
cannot disagree is for the second door to be the first one's own bytes.

## §6.137 `/proc/<pid>/timers` is the group's table, and a timer's number is not its slot (LANDED 2026-11-12)

The POSIX timer calls existed since §6.123 and §6.124, but nothing let a program
*list* what it held: a probe that wanted to know which numbers were live had to
remember every call it made, and a program that forgot one could not find out.
This kernel had no `/proc/<pid>/timers` at all. Writing the file also exposed a
second, worse defect: this kernel's `timer_t` **was the slot index**, so the
create after a delete handed the caller the deleted number back and its old
handle silently began naming a different timer.

Everything here was measured on a host Linux first (`build/scratch/ptimers/`,
7.2.14), with raw syscalls - no libc on Linux binds `sigevent`, `itimerspec` or
`timer_t`, the same reason `t-timer-create` goes through `libc::syscall`.

The file with no timers (`p.c`, `u.c`):

```
/proc/self/timers           0 bytes, mode 0444, st_size 0, no trailing newline
/proc/thread-self/timers    open errno=2 (ENOENT)
/proc/<pid>/task/<tid>/timers open errno=2 (ENOENT)
/proc/1/timers (uid 1000)    0 bytes - readable, it is 0444
```

and with three timers on one process (`p.c`):

```
ID: 2
signal: 12/0000000000000000
notify: signal/tid.17626
ClockID: -6
ID: 1
signal: 0/0000000000000000
notify: none/pid.17626
ClockID: 0
ID: 0
signal: 10/0000000000000000
notify: signal/pid.17626
ClockID: 1
```

Four lines per timer, in that order, with the case the man page does not spell
out: upper-case `ID:`/`ClockID:` beside lower-case `signal:`/`notify:`. The
`signal:` value is the `sigval` as sixteen lowercase hex digits, so a timer
created with `sival_ptr = 0xdead` prints `000000000000dead`. `notify:` names the
notification mechanism before the slash and the subject's kind after it, which is
why a `SIGEV_THREAD_ID` timer reads **`signal/tid.N`** and not `thread/...` - the
first guess, caught by the host run.

`ClockID:` is `timer->it_clock`, and that is the caller's own id for a wall
clock (`0`, `1`, `7`, `11` measured, one `timer_create` each) but the kernel's
encoded word for a CPU clock: `CLOCK_PROCESS_CPUTIME_ID` prints **`-6`**, which
is `(~0 << 3) | 2` truncated to `int`.

### The number is a counter, not a slot

The measurement that made this a defect rather than a new file (`t.c`):

```
create REALTIME=0  -> 0     create MONOTONIC=1 -> 1     create BOOTTIME=7 -> 2
create TAI=11      -> 3     create PROCESS_CPUTIME=2 -> 4
delete 1 ; create MONOTONIC -> 5          (not 1)
/proc/self/timers prints ids: 5, 4, 3, 2, 0
```

Linux keeps a per-process counter that only moves forward, so a deleted number is
never handed out again, and the file lists the table **descending**. This kernel
returned the slot, so `delete(1); create()` answered `1` and the caller's old
handle started naming a *different* timer - a silent retarget, which is the
failure shape `AGENTS.md` §3 rules 2 and 3 exist to prevent.

A fork inherits the counter's position, as it inherits the timers themselves: the
child's next number is not the parent's next number.

An `itimer` is not a POSIX timer and does not appear: `setitimer` on `ITIMER_REAL`
and `ITIMER_VIRTUAL` left the file byte-for-byte unchanged (`p.c`).

A zombie **keeps the file, empty** (`z.c`):

```
running child     /proc/<pid>/timers: 70 bytes (one timer)
killed, unreaped  /proc/<pid>/timers: 0 bytes
reaped            /proc/<pid>/timers: errno=2 (ENOENT)
```

because Linux holds the `signal_struct` until the task is collected and tears the
timers down at exit - so the empty list is not the record's last state, it is the
truth: a zombie has no timers.

### The fix

Four places, one idea: the number and the slot are two different things.

* `PosixTimer` gained `id: u32` and `clock_id: i32`, and `Task` gained a
  `next_timer_id: u32` counter (six construction points: init, spawn, thread
  spawn, exec, fork, and the fresh-task literal). `timer_alloc` still picks the
  lowest free slot but returns the counter and advances it.
* Every door that takes a `timer_t` - `timer_arm`, `timer_state`,
  `timer_overrun`, `timer_remove` - now resolves it through `timer_slot`, which
  scans the eight slots for a matching `id`. A deleted number therefore names
  nothing, and a live number names exactly one timer.
* `crates/abi` gained `TimerLine { id, signo, value, kind, subject, clock_word }`
  and the three `TIMER_NOTIFY_*` constants, so `sched` produces the answer and
  `vfs` renders it; the subject travels as a **task id**, because the file is read
  from another pid namespace and only the reader's namespace can name it.
* `src/vfs/procfs.rs` renders it with `timers_text` (four lines each, `hex16` for
  the `%px` word), and the node is `maybe_file` at `0444` - one node at
  `/proc/<pid>/timers`, deliberately **not** under `task/<tid>`, because Linux
  measured no per-thread spelling.

The file reads `Some(empty)` for a zombie and `None` only when no task holds the
number, which is what makes the two answers above.

### The pin

`user/tests/src/bin/t-proc-timers.rs`, 18 claims: the empty file's zero bytes;
one timer's whole body byte for byte; the subject is this process; the `sigval`
in the signal word; `ClockID` for all four wall clocks; `-6` for the process CPU
clock; `none/pid.N` for a silent timer; `signal/tid.N` for a thread-directed one;
the ids are what `timer_create` answered, listed descending; a deleted id is not
reused; an `itimer` changes nothing; the node is `0444` with size 0; there is no
per-thread spelling; a zombie keeps an empty file and the reap takes it away.

The counterfactual stashes the nine source files and rebuilds the kernel *and*
the UEFI loader that embeds it (`boot/src/lib.rs`'s `PACKED_KERNEL`), then boots
the same binary: **15 of 18 claims red**, every one that reads the file, because
the node does not exist; the two that stay green are the `itimer` byte comparison
and the reap claim, both of which are true of a kernel that never had the file.
Restoring and rebuilding returns 18/18.

### Gate

`build/gate-timers.log`, `MPOS_TEST_LANES=2 TMPDIR=$PWD/build/tmp cargo xtask
test --all-targets`: stage 1 `1278 tests, 0 failed` (70.08 s); x86_64 `pass=300
fail=0 (300 built)`, `mpos-kunit 35 passed, 0 failed`, `total 70.44 s` (-5.8%
against the previous green); aarch64 `pass=299 fail=0 (299 built)`, kunit 35/0,
`total 227.88 s` (-0.5%). `t-proc-timers` passed on both arms:
`build/test-lane-x86_64-0/serial.log:4425` and
`build/test-lane-aarch64-0/serial.log:4655`.

### The rule

A handle is a promise about *which object*, not about *where it is stored*. The
two coincide until something is deleted, which is why an index-as-handle passes
every test that never frees anything - and why the file that finally listed the
table could show the retarget.

## §6.138 `readahead` and `sync_file_range` judge a range, and the two laws differ (LANDED 2026-11-12)

Two neighbouring calls, both of the shape "here is a byte range, do something
sensible with it", both answered by the wrong thing for the same reason: this
kernel treated the *subject* as the whole question. `readahead(2)` was

```rust
match crate::sched::current_fds().clone_at(args[0] as i32) {
    Ok(_) => 0,
    Err(e) => err_of(e),
}
```

which is to say every descriptor that exists is a descriptor whose range will be
read. `sync_file_range(2)` had no dispatch arm at all, so it fell through the
`_ => ENOSYS` and told every caller Linux had never heard of a call it has served
since 2.6.17 - and there is no fallback for that answer, because the whole point
of the call is that the caller does not have to wait.

### What the host answers

Measured on Linux 7.2.14 (`build/scratch/advice/{p,q,r,t,u,v,w,x,y}.c`), with one
correction worth stating up front: the first probe passed the range words through
`syscall(2)`'s variable arguments as `int`, so the register held
`0x0000_0000_ffff_ffff` rather than `-1` and the kernel read a positive
4294967295. That probe reported "a negative offset is accepted" and the truth was
that it never sent one. Every reading below comes from a probe that declares its
arguments `long`:

```
readahead(regular, 0, 4096)                 = 0
readahead(regular, 0, 131073)               = 0        (no MAX_READAHEAD cap)
readahead(regular, 0, i64::MAX)             = 0
readahead(regular, 2^62, i64::MAX)          = 0        (a wrapping sum is fine)
readahead(regular, -1, 0)                   = EINVAL
readahead(regular, 0, 2^63)                 = EINVAL   (a signed negative count)
readahead(write-only regular fd)            = EBADF
readahead(regular, -1, 4096) on a write-only fd = EBADF  (mode before range)
readahead(dir) / (pipe) / (/dev/null) / (/dev/zero) / (socket) = EINVAL
readahead(999, -1, 4096)                    = EBADF    (descriptor before range)
```

```
sync_file_range(regular, 0, 4096, 0|1|2|4|7)   = 0
sync_file_range(regular, 0, 4096, 8|0x10|0x18|~0) = EINVAL
sync_file_range(pipe, 0, 4096, 8)              = EINVAL   (flags before kind)
sync_file_range(pipe, 0, 4096, 0)              = ESPIPE
sync_file_range(/dev/null) / (/dev/zero) / (socket)     = ESPIPE
sync_file_range(dir)                           = 0
sync_file_range(regular, -1, 4096, 0)          = EINVAL
sync_file_range(regular, 0, -1, 0)             = EINVAL
sync_file_range(regular, 2^62, 2^62, 0)        = EINVAL   (the sum wraps)
sync_file_range(regular, 0, i64::MAX, 0)       = 0
sync_file_range(regular, i64::MAX, 0, 0)       = 0
sync_file_range(999, 0, 4096, 8)               = EBADF    (descriptor first)
```

So the two doors ask four questions in a fixed order - and they ask them in an
order that is not the same, which is the part a shape cannot get right:

| door | 1 | 2 | 3 | 4 |
|---|---|---|---|---|
| `readahead` | descriptor exists (`EBADF`) | mode is readable (`EBADF`) | node is a regular file (`EINVAL`) | offset and count non-negative (`EINVAL`) |
| `sync_file_range` | descriptor exists (`EBADF`) | range non-negative and non-wrapping (`EINVAL`) | flag word is `1\|2\|4` (`EINVAL`) | node has something to flush (`ESPIPE`) |

Two contradictory witnesses pin the orders. `readahead(999, -1, 4096)` is `EBADF`
and not `EINVAL`, so the descriptor outranks the range. `sync_file_range(pipe,
flags=8)` is `EINVAL` while the same pipe with `flags=0` is `ESPIPE`, so the flag
word outranks the kind - but `(999, flags=8)` is `EBADF`, so the descriptor
outranks the flag word. A door that tested its arguments in a different order
would answer each of those callers with the wrong errno while agreeing on every
single-fault test.

The two laws also differ in one place, and only measurement finds it: `readahead`
accepts a range whose sum wraps (`2^62 + i64::MAX` is served) while
`sync_file_range` refuses one (`2^62 + 2^62` is `EINVAL`), because the latter's
`vfs_fsync_range` forms `end = offset + nbytes` and checks that it did not wrap.

### The shape here

`readahead`'s mode test is `File::check_readable()`, and its kind test needs the
`as_pipe()`/`as_duplex()` pair as well as the kind, for the reason `fsync`
already carries: a pipe end and a `socketpair` end report `NodeKind::File`,
because the filesystem's vocabulary has no word for an anonymous byte channel.
Reading the kind alone would let a pipe through as a regular file and answer 0
where Linux answers `EINVAL` - the same lie, in the same two families, as the one
`fsync` was fixed for.

`sync_file_range`'s flush is empty - every write here reached its medium before
the call returns - so like `fsync` the call is its admission. What is not empty is
the answer for a node with nothing behind it: pipes, sockets, character devices
and anonymous inodes are `ESPIPE`, which is Linux's word for "this medium has no
byte ranges to wait on".

The numbers are `277` on x86_64 and `84` on aarch64, both pinned in `nr.rs`'s
assert table. The asm-generic table spells `sync_file_range2` with the same `84`:
the C library picks the variant whose 64-bit arguments are aligned, and on a
64-bit machine both are the plain `(fd, offset, nbytes, flags)` order.

### The pin

`user/tests/src/bin/t-range-advice.rs`, 22 claims. It creates its own file under
`/tmp` (a regular file is what `readahead` will serve), opens the same file
write-only for the mode test, and uses `/tmp` itself, `/dev/null` and a `pipe(2)`
pair for the kinds. Two claims are order witnesses rather than answers -
`readahead(999, -1, 4096) = EBADF` and `sync_file_range(pipe, flags=8) = EINVAL`
- because the errno alone cannot tell an order apart; only a call with *two*
faults can.

Both arms passed: host 22/22 and the machine 22/22 (`build/boot-advice1.log`).

Against the kernel without the change (the six syscall-layer files stashed and
both the kernel and the loader rebuilt), **19 of the 22 claims fail**
(`build/boot-advice-cf1.log`):

```
FAIL: a descriptor this call cannot read from is EBADF (got 0, want 9)
FAIL: a directory has no page cache to fill (got 0, want 22)
FAIL: a pipe has no page cache either (got 0, want 22)
FAIL: and neither has a character device (got 0, want 22)
FAIL: a negative offset is refused (got 0, want 22)
FAIL: a count whose top bit is set is a negative count (got 0, want 22)
FAIL: a flush of a regular range is served (got 38, want 0)
FAIL: a flag bit outside the vocabulary is EINVAL (got 38, want 22)
FAIL: a pipe has nothing to flush, so the same pipe is ESPIPE (got 38, want 29)
FAIL: a descriptor this call ... (got 38, want 9)      [four more ENOSYS lines]
```

The three that hold on both kernels are the controls: a regular file's range is
served, a huge count is still a promise, and a missing descriptor was already
`EBADF`. That is the shape a counterfactual should have - the claims that move
are the ones the change is about, and the ones that do not are the ones the old
code already answered.

### The gate

`GATE EXIT=0`. Stage 1 `1281 tests, 0 failed` (70.73 s) - the +3 are
`readahead_judges_the_sign_of_its_range_and_nothing_else`,
`readahead_lets_its_range_wrap_and_sync_file_range_refuses_to` and
`sync_file_range_has_three_flag_bits_and_refuses_the_rest`. x86_64
`pass=301 fail=0 (301 built)` with `mpos-kunit 35 passed, 0 failed` (51.71 s over
2 lane(s), total `71.08 s`, `+0.9%` against the previous green); aarch64
`pass=300 fail=0 (300 built)` with kunit 35/0 (128.05 s over 2 lane(s), total
`221.48 s`, `-2.8%`). `t-range-advice` is `=== PASS` on both arms
(`build/test-lane-x86_64-1/serial.log:4171`, `build/test-lane-aarch64-1/serial.log:4641`).

### Named gaps

- **`MAX_READAHEAD`**. Linux truncates the range it will actually pull in to
  `MAX_READAHEAD` (128 KiB) and expands a read to `readahead_expand()`. This
  kernel reads eagerly, so there is no window to fill; the call is admission and
  the answer is 0. What is *not* implemented is any promise about the bytes
  arriving early - and the caller cannot tell, which is what makes this a gap in
  the mechanism rather than in the answer.
- **`sync_file_range`'s flush**. Every write here is synchronous, so nothing is
  deferred and nothing is flushed. A medium that starts deferring writes has to
  give this call a real answer, and `WAIT_BEFORE`/`WAIT_AFTER` have no meaning
  until then. `sync_file_range2` is the same call with the aligned-argument order
  and is not separately numbered here, because a 64-bit machine uses the plain
  order.

### The rule

An admission is a promise, and a promise about a *range* is not a promise about
its *subject*. "The descriptor exists" answers which object, never whether this
call can do anything with that object - which is why both doors here had to be
taught the four questions and the order between them, and why the two doors
turned out to order them differently.

## §6.139 `fadvise64` is a hint, and a hint still has an order (LANDED 2026-11-12)

The door was written on a sentence: "it is only a hint, so every advice on every
open descriptor is a success". Three separate answers came out of that one
sentence, and each belonged to a caller with something to lose:

- the **kind** was asked after the advice, so `fadvise64(pipe, advice=6)` was
  `EINVAL` where Linux answers `ESPIPE` - a caller told its advice word was bad
  when the advice was never the problem;
- the **range** was not asked at all, so `(-1, 4096)` and `(0, -1)` were
  answered 0 where Linux answers `EINVAL` - a hint about a range that does not
  exist;
- the **word width** was wrong, so `advice = 0x1_0000_0003` was refused while
  Linux reads the low 32 bits and sees `POSIX_FADV_NORMAL` - the same width
  question `§6.124` settled for `TIMER_ABSTIME` and `§6.138` settled for
  `sync_file_range`'s flags.

### What the host answers

Measured on Linux 7.2.14 (`build/scratch/advice/{f,g,h,i,j}.c`):

```
fadvise(regular, 0, 4096, 0..=5)          = 0
fadvise(regular, 0, 4096, 6 | 7 | -1)     = EINVAL
fadvise(regular, 0, 4096, 0x1_0000_0003)  = 0        (the word is an int)
fadvise(regular, 0, 4096, 0x1_0000_0006)  = EINVAL
fadvise(regular, -1, 0)                   = EINVAL
fadvise(regular, 0, -1)                   = EINVAL
fadvise(regular, -4096, 4096)             = EINVAL
fadvise(regular, 2^62, 2^62)              = 0        (a wrapping sum is fine)
fadvise(regular, 0, i64::MAX)             = 0
fadvise(pipe, 0, 4096, 9)                 = ESPIPE   (kind before advice)
fadvise(pipe, -1, 4096, 3)                = ESPIPE   (kind before range)
fadvise(socketpair | socket | /dev/null | dir, 0, 4096, 3) = 0
fadvise(999, 0, 4096, 9)                  = EBADF    (descriptor before all)
```

The order is the part that cannot be guessed from the call's shape, and it is the
opposite of the door next door. `sync_file_range` asks its flag word **before** the
node's kind (`(pipe, flags=8)` is `EINVAL`); `fadvise64` asks the kind **before**
its advice word (`(pipe, advice=9)` is `ESPIPE`). Two calls that both take `(fd,
offset, len, word)` and both answer `EINVAL`/`ESPIPE` order their four questions
differently - so a shared "validate then serve" helper would have had to pick one
and be wrong for the other.

The "only a pipe" clause is the other half of the kind question. A socket, a
`socketpair` end, a character device and a directory all answer 0; only a pipe is
`ESPIPE`. That is why the door tests `as_pipe()` and not `as_duplex()` - the two
`as_*` families that `fsync` has to treat *together* are treated differently here.

### The shape here

Two verdicts in `abi.rs`, one per word, each with its own measurement:

- `fadvise_range_verdict(offset, len)`: both signed, both non-negative, and a
  wrapping sum **accepted** - the same law as `readahead`, and unlike
  `sync_file_range`, which refuses `(2^62, 2^62)`.
- `fadvise_advice_verdict(word)`: `word as u32 > 5` is `EINVAL`, which makes
  `0x1_0000_0003` a `NORMAL` and a sign-extended `-1` an `EINVAL`.

`handle_fadvise64` then asks: descriptor, kind, range, advice - in that order, and
the order is the finding.

### The pin

`user/tests/src/bin/t-fadvise.rs`, 18 claims, host 18/18 and machine 18/18
(`build/boot-fadvise1.log`). Against the kernel without the change
(`build/boot-fadvise-cf1.log`, the two syscall-layer files stashed and both the
kernel and the loader rebuilt) **5 of 18 red**:

```
FAIL: the advice is the low 32 bits of the register (got 22, want 0)
FAIL: a negative offset is refused (got 0, want 22)
FAIL: so is a negative length (got 0, want 22)
FAIL: and so is a range that ends before it starts (got 0, want 22)
FAIL: the kind is judged before the advice (got 22, want 29)
```

The five are one per defect plus the width, and the thirteen that hold on both
kernels are the answers the old door already got right - which is what a
counterfactual should show: the descriptor first, the six-word vocabulary, and
"only a pipe is `ESPIPE`" were all true before this change.

### The gate

`GATE EXIT=0`. Stage 1 `1282 tests, 0 failed` (71.79 s; the +1 over the previous
green is `fadvise_has_six_advices_and_a_signed_range_that_may_wrap`). x86_64
`pass=301 fail=0 (301 built)` with `mpos-kunit 35 passed, 0 failed` (52.91 s over
2 lane(s), total `72.16 s`, `+1.5%`); aarch64 `pass=300 fail=0 (300 built)` with
kunit 35/0 (138.82 s, total `232.84 s`, `+5.1%`). `t-fadvise` is `=== PASS` on
both arms (`build/test-lane-x86_64-0/serial.log:3740`,
`build/test-lane-aarch64-0/serial.log:5178`).

### Named gaps

- **The advice does nothing.** `POSIX_FADV_WILLNEED` and `DONTNEED` are accepted
  and have no effect, because this kernel reads eagerly and caches nothing
  evictable. That is Linux's own contract for a hint - best effort, no promise -
  so the answer is truthful, but a caller cannot tell that nothing was done.
  `RANDOM`/`SEQUENTIAL`/`NOREUSE` likewise have nothing to tune.
- **`preadv2`/`pwritev2`'s `RWF_*` words** are the same family of "word with a
  closed vocabulary" and are handled in their own doors; they are not covered by
  this item.

### The rule

"A hint has nothing to refuse" is false in a specific way: a hint still names a
*subject*, a *range* and a *word*, and each of the three can be malformed. The
order in which a door asks about them is part of its ABI - the caller who has two
of them wrong is the only caller who can tell, and every single-fault test will
agree with a door that asks in the wrong order.

## §6.140 `name_to_handle_at` answers what a filesystem without handles answers (LANDED 2026-11-12)

The pair `name_to_handle_at(2)`/`open_by_handle_at(2)` exists so a caller can
name an object by something a rename cannot invalidate - the mechanism behind
`tar --xattrs --acls`'s handle cache, `rsync`'s `--copy-devices` fallback, and
every backup tool that wants to remember an inode across a tree walk. Both
numbers were undispached here, so both doors answered `ENOSYS`.

That is the wrong answer even though this kernel cannot do the thing. `ENOSYS`
means *the call does not exist here*; a caller that hears it concludes the
kernel predates the interface and has no fallback for the platform. What this
kernel actually is, for both calls, is a filesystem whose filesystems have no
export operations - and Linux has a word for exactly that, which callers
already know how to handle: `EOPNOTSUPP`. Measured on a current host (Linux
7.2.14, `build/scratch/nhandle/{p,q,r,t,u,v,w}.c`), on `/proc/self/stat`, whose
filesystem is one of those:

```
plain call                                = EOPNOTSUPP(95)
null handle / null mount_id / ptr = 1     = EOPNOTSUPP(95)   <- no write, no check
flags 0x100 (AT_SYMLINK_NOFOLLOW)         = EINVAL(22)
flags 0x800 (AT_NO_AUTOMOUNT)             = EINVAL(22)
flags 0x200 / 0x400 / 0x1000              = 0 / EOPNOTSUPP / EOPNOTSUPP
bad dirfd, relative name, flags 0         = EBADF(9)
bad dirfd, relative name, flags 0x100     = EINVAL(22)       <- flags first
bad dirfd, absolute name                  = EOPNOTSUPP(95)   <- name first
null path                                 = EFAULT(14)
missing path                              = ENOENT(2)
missing path with a null handle            = ENOENT(2)       <- name first
open_by_handle_at, handle_bytes 0          = EINVAL(22)
open_by_handle_at, handle_bytes 8, no cap  = EPERM(1)
```

and, on `tmpfs`, which *does* export, the same calls answer `0` with a 12-byte
handle, `EOVERFLOW(75)` with the needed size written back for every short
buffer, and `EFAULT` for the three bad pointers `procfs` ignored. The contrast
is the whole finding: **the pointer and buffer questions belong to a filesystem
that can answer them**, and a filesystem that cannot is not allowed to borrow
their errno.

### What the door does now

```
name_to_handle_at(dirfd, path, handle, mount_id, flags):
    flags & !(0x200 | 0x400 | 0x1000)     -> EINVAL
    the name (dirfd, pointer, existence)  -> EBADF / EFAULT / ENOENT
    otherwise                             -> EOPNOTSUPP
```

Neither `handle` nor `mount_id` is read or written: an answer of
`EOPNOTSUPP` promises nothing was placed there, and validating the destination
would be a check with no write behind it - the `EFAULT` Linux gives only where
it is about to fill the buffer in. The `0x200` bit is `AT_HANDLE_FID`, and it
reads the same in this call and in `AT_REMOVEDIR` in the family that removes
things: the flag word is a vocabulary per call, not a shared mask, which is why
it is a verdict of its own (`abi::name_to_handle_at_flags_verdict`).

```
open_by_handle_at(mount_fd, handle, flags):
    handle_bytes == 0         -> EINVAL        (before the privilege)
    !CAP_DAC_READ_SEARCH      -> EPERM
    mount_fd not open         -> EBADF
    otherwise                 -> EOPNOTSUPP
```

The capability is the honest discriminator here, and the pin uses it as one:
it reads `CapEff` out of `/proc/self/status` and computes which refusal it
should meet, so the same program says `EPERM` on a host that runs it
unprivileged and `EOPNOTSUPP` in this kernel's guest, where the payload is
root. The second is the case that cannot be measured on a development host at
all - a user namespace holding the whole set is still refused, because the
check is against the initial namespace - so it is decided by reasoning and
fixed by the pin rather than observed.

### Two divergences this records

* **`AT_HANDLE_FID` succeeds on Linux and `EOPNOTSUPP`s here.** Linux builds a
  fid for that flag on *any* inode, export operations or not (`procfs` measured
  `0`), because the fid is deliberately non-decodable: it exists to be handed
  to `fanotify`, which this kernel does not have. Building one is a feature,
  not a field; it is a named gap.
* **With the capability, Linux answers `ESTALE(116)`** for a handle its
  filesystem cannot decode. This kernel answers `EOPNOTSUPP`, because the
  decode path is unreachable here - there is no handle in existence to decode -
  and `ESTALE` would claim a staleness this kernel never observed. The pin
  fixes that answer so a later implementation has to change it on purpose.

### The pin

`user/tests/src/bin/t-handle-at.rs`, 18 claims: the served-call answer, the
three-bit vocabulary with its two refusals, the flag-before-descriptor witness
(`(999, relative, 0x100) = EINVAL`), the name-before-descriptor witness
(absolute name through `999`), the null-pointer-is-not-an-error pair that makes
the `tmpfs`/`procfs` distinction visible, `handle_bytes == 0`, the
capability-decided answer, and a claim that neither door answers `ENOSYS`.

Host 18/18 and machine 18/18. Counterfactually, with the six syscall-layer
files stashed: **17 of 18 red**, every one of them reading `ENOSYS(38)`. The
single green is `flag 0x200 is part of the vocabulary`, whose claim is "not
`EINVAL`" - and `ENOSYS` is not `EINVAL`, so it holds on both kernels. That is
the honest boundary of a vocabulary claim: it pins that the bit is *known*,
not that the door exists.

### The gate caught one thing the compiler did not

The measured table at the top of this section first sat in
`name_to_handle_at_flags_verdict`'s doc comment inside a bare fenced block, and
`cargo test -p mpos --doc` compiles every bare fence as Rust: `0 = 0` is not an
item, so the gate went red on a doctest while both machines were green
(`build/gate-handleat.log`). A table that is a *measurement* is fenced `text`;
the gate's doctest stage is the reader that notices, and only on a bare fence.

### The rule

`ENOSYS` and `EOPNOTSUPP` are different claims about the world, and the one a
caller can act on is the second. A kernel that cannot do a thing but knows the
shape of the thing owes the caller the refusal that names *what* is missing -
here, a filesystem with no handles - rather than the refusal that says the
kernel has never heard of the question.

## §6.141 `/proc/<pid>/fdinfo/<n>` exists, and its four lines are Linux's relations (LANDED 2026-11-12)

`/proc/<pid>/fd` answers *which* descriptors a process has and what each one
names; it does not answer where one is positioned or what flags it carries. That
is what `/proc/<pid>/fdinfo/<n>` is for, and this kernel built none of it: a
tool that opened the file got `ENOENT`, the same answer as for a process that
does not exist. It is not a debugging convenience - `F_GETFL` and `F_GETFD`
answer only for a descriptor the *asking* process holds, so a debugger, a
supervisor or `lsof` has no other door to ask about somebody else's descriptor.

Linux renders it from `seq_file`: four generic lines every kind has, then
whatever the node's own `show_fdinfo` adds. Measured whole on Linux 7.2.14
(`build/scratch/fdinfo/*.c`):

```text
pos:	7
flags:	0100000
mnt_id:	634
ino:	27001799
```

### Four lines, four radixes, four relations

Each line is a *relation* to a door this kernel already serves, which is why the
pin claims the relation rather than the number - a host and this guest do not
agree on any of the four values, and both agree on every relation:

| line | Linux's shape | what it is a relation to |
|---|---|---|
| `pos:` | decimal | `lseek(SEEK_CUR)`'s answer; a `read` moves it, and a `dup` shares it |
| `flags:` | **a literal `0` then octal** | `F_GETFL` **or** `O_CLOEXEC` when `FD_CLOEXEC` is set |
| `mnt_id:` | decimal | the **first column** of `/proc/self/mountinfo`'s line for that mount |
| `ino:` | decimal | `fstat`'s `st_ino` |

Two of those hold more than they look like they do.

**`flags` is not `F_GETFL`.** It is `F_GETFL | (FD_CLOEXEC ? O_CLOEXEC : 0)`, and
the two halves have different owners: the access mode and the status bits live on
the *description*, `FD_CLOEXEC` on the *descriptor*. Measured
(`build/scratch/fdinfo/cloexec.c`): `open(O_RDONLY|O_CLOEXEC)` answers
`02100000` from this line while `F_GETFL` answers `0100000`, `F_SETFD(0)` takes
the bit back out, and `dup` of an `O_CLOEXEC` descriptor gives a number without
it. A renderer that printed `F_GETFL` would lose the half that tells a tool the
descriptor will not survive an `exec`; a renderer that printed the *open's* saved
word would keep claiming it after the caller cleared it.

**The `0` before the octal digits is a literal.** Linux's format string is
`"flags:\t0%o\n"`, not `"flags:\t%o\n"`, which is why `O_RDWR` prints `02` and a
zero word `00` rather than `2` and `0`. This was the one real bug the host unit
tests caught while writing it: `generic_lines` used `{:o}` and produced
`0100000` for a word whose octal spelling already began with zero, then `100000`
for one that did not - so the line was right for some descriptors and wrong for
others. The measured table above is now a `#[cfg(test)]` claim on the whole
string rather than a substring a shared prefix would satisfy.

**`mnt_id` is the *reused* id, not `statx`'s.** Measured: for the same file,
`/proc/self/fdinfo` printed `634` and `statx(STATX_MNT_ID_UNIQUE)` answered
`2147491621`. They are different number spaces on purpose, and this file is the
one that takes the mountinfo spelling because correlating the two files is what
the number is for.

### What an anonymous descriptor reports

An `epoll`, an `eventfd`, a pipe end, a socket and a `memfd` have no mount, and
Linux reports a **pseudo** id there rather than 0 - measured, `19` for every
`anon_inodefs` kind, `18` for `pipefs`, `12` for `sockfs`, and `0` for a `memfd`
whose `shmem` inode has no mount at all. None of the four appears in
`/proc/self/mountinfo`, which is why they are constants in
`vfs::fdinfo` rather than a table lookup: a reader that compared them against the
mount table would find nothing, and Linux's own answer is that they are not
mounts.

The kinds are decided through the accessors the tree already had
(`as_eventfd`, `as_epoll`, `as_timerfd`, `as_inotify`, `as_pidfd`,
`as_fanotify`, `as_pipe`, `as_duplex`, `socket_handle`), plus one this item
*added* - `as_signalfd` - because the `signalfd` node was the only one of the
five `anon_inodefs` kinds the tree could not name. The `memfd` and `io_uring`
checks are split by `target_os` behind small helpers: both modules are gated on
the machine build (they hand out physical pages), so the host build has no
`memfd` to find - which is the honest answer rather than a missing one.

### The directory is not the `fd` one

Measured (`build/scratch/fdinfo/{edge,size}.c`), Linux's two directories have
different words, and both halves matter:

| | `/proc/<pid>/fd` | `/proc/<pid>/fdinfo` |
|---|---|---|
| mode | `0500` | `0555` |
| entries | symlinks | regular files, `0444` |
| `st_size` | the descriptor count (`3 -> 6 -> 3` as three were opened and closed) | `0`, always |

The name rule is not "parses as a number": `abc`, `-1`, `00`, ` 3`, `3x`, `0x10`
and `4294967296` all answer `ENOENT`, and `00` in particular parses to `0` while
Linux still refuses it. So the test is that re-rendering the decimal gives the
name back, which is the same rule the listing produces names by.

The directory is wired into both builders - `/proc/<pid>` and
`/proc/<pid>/task/<tid>` - because Linux answers `task/<tid>/fdinfo/<n>`
measured, and it is the same descriptor set the `fd` beside it lists.

### The per-kind half

Each node answers for itself through a new `Inode::fdinfo_lines` hook, for the
reason `fd_name` is a hook: the state a line reports lives on the object the
descriptor opens, and the node is the only thing procfs has. Four kinds serve a
block, each measured line by line:

| kind | block | measured details |
|---|---|---|
| `eventfd` | `eventfd-count`, `-id`, `-semaphore` | the count is **hex**, right-aligned in **sixteen** columns; the semaphore word is decimal |
| `signalfd` | `sigmask:` | sixteen zero-padded hex digits of the wire `sigset_t` (signal *n* in bit *n-1*) |
| `timerfd` | `clockid`, `ticks`, `settime flags`, `it_value`, `it_interval` | `it_value` is the time **remaining**, not the stored deadline - a 100 s arm read `(99, 999987917)` moments later |
| `epoll` | one `tfd:` line per watch | five widths and two radixes; the gap before `pos:` is **two** spaces because Linux's `seq_printf` is two adjacent literals |

`eventfd-semaphore` is always `0` here, and that is honest rather than stale:
this kernel's `eventfd2` refuses `EFD_SEMAPHORE` with `EOPNOTSUPP`, so the shape
it would report cannot exist. `eventfd-id` reports the inode's own number, which
has the property Linux's id has (it names one instance for its lifetime) rather
than Linux's numbering; a second counter existing only for a debug line was the
rejected alternative. `timerfd`'s `settime flags` needed a new field on
`TimerState`: the arm's *word* is what the line reports, and the state machine
kept only the `cancel_on_set` boolean it acted on.

**`inotify` is a named refusal, and it is the interesting one.** Linux prints one
line per watch whose tail is a **file handle** built with `exportfs_encode_fid` -
and this kernel's filesystems have no export operations at all, which is
`name_to_handle_at`'s finding (item 254, §6.140) one item later. On such a
filesystem Linux omits only that tail. But the *head* has a column this kernel
cannot fill honestly: `sdev`. The watch table keys a watch by
`(fs_id_of_inode, inode number)`, and that first element is the filesystem
*instance* id - `0` for every inode that is not some mount's root - not the
`st_dev` Linux prints. Printing it under Linux's field name would be a private
dialect. So the whole block is refused rather than one column approximated, and
what a reader sees for a watched inotify is the four generic lines: Linux's shape
for a zero-watch instance. That cost is named here and in the code, with its
missing prerequisite (the watch table would have to keep the watched inode's
`st_dev`, which is `key_of`'s second question rather than its first).

### How it is verified

Host unit tests cover the renderers: `vfs::fdinfo` (four), `vfs::eventfd::tests`,
`vfs::signalfd::tests`, `vfs::timerfd::tests`, `vfs::epoll::tests` (two) and
`vfs::inotify::tests` - each comparing **whole measured lines**, because a
substring claim lets a prefix-valid mistake through (and did: the flags bug above
was green under a `contains("flags:")` shape until the whole string was
compared).

The machine pin is `user/tests/src/bin/t-fdinfo.rs`, 24 claims, green on the host
first: the four relations (including the `FD_CLOEXEC` round trip and the
`F_GETFL`-does-not-report-it half), a `dup` sharing the position while a second
open does not, the `mnt_id`-against-mountinfo correlation, the six refused
spellings, the two modes, the file-not-a-link claim, and the per-kind values
(`eventfd` hex, `signalfd` wire mask, `timerfd` against `timerfd_gettime`, and
the two *different* pseudo ids a pipe end and a socketpair end report).

**One claim was written wrong and the host caught it**: `timerfd`'s `it_value`
was first pinned *equal* to a later `timerfd_gettime`, and the two doors are read
at two instants of a running clock - the host answered `99999976165` ns against
`99999957450`, both correct and both different. The claim is now the relation
that actually holds (the earlier read is the larger, and both are inside the
arm's span), which is the same lesson §12.4a already records: a test asserting a
relationship between two live clocks must not invent either of them.

**Counterfactual** (the twelve changed source files stashed, kernel and loader
rebuilt, `build/boot-fdinfo-cf1.log`): the file does not exist, so the pin dies
at its first claim - `fdinfo/<fd> of 10 is readable` - and every later claim that
can still be made (`a dup reports the same position...`, `a read moves the
position both names report`, `a second open has its own position`) reads `None`
and reddens with it. Restored and rebuilt, 24 of 24 green
(`build/boot-fdinfo1.log`).

### The guest caught one the host could not see, and clippy caught another

The **socketpair** claim went red on the machine while every host test was green:
`/proc/<pid>/fdinfo` for a `socketpair(2)` end reported the pipe pseudo id
instead of `sockfs`'s. The classifier had put `as_pipe` and `as_duplex` in one
arm, on the reasonable-looking reading that a pipe and a socketpair are two
spellings of a two-way channel - and they are two different *filesystems*:
`pipe()` mints a `PipeEnd`, `socketpair(AF_UNIX, SOCK_STREAM)` mints a
`DuplexEnd`, and Linux answers `18` and `12` respectively. No host unit test
could have caught it, because the host build has no `socketpair` to classify; it
took the machine. The classifier now separates them, and a host test pins the
pair (`a_pipe_end_and_a_socketpair_end_get_different_pseudo_ids`, which builds
both nodes through their real constructors).

That is the shape to remember: a *classification* can be wrong in a way that a
rendering test cannot see, because the test's own fixture supplies the class.



`cargo clippy` found `timerfd`'s `fdinfo_lines` sitting in the **inherent**
`impl TimerFd` rather than `impl Inode for TimerFd`, where the trait's default
`None` shadowed it - the machines would have rendered no `timerfd` block while
every host test that called the function directly stayed green. That is the value
of the "never used" warning being a `warn` in a tree whose dead code is normally
intentional: a hook that is *defined* somewhere no dispatch can reach is exactly
what it is for.

## §6.142 `personality` and `oom_adj`: two views, one owner each (LANDED 2026-11-12)

Both of these `/proc/<pid>` files are **views** - one of `personality(2)`'s word,
one of the modern `oom_score_adj` under the deprecated fifteen-step name - and
both are the kind of file that is easy to serve *wrongly* by keeping a second
copy of a fact that already has an owner. Neither existed here: a program that
opened either got `ENOENT`.

Measured on Linux 7.2.14 (`build/scratch/procsyscall/{pers,oomadj}.c`):

```text
/proc/self/personality     00000000          mode 0400, read-only
/proc/self/oom_adj         3                 mode 0644, read/write
/proc/self/oom_score_adj   200
```

### `personality`: eight hex digits, and the syscall's own answer

The file is the execution-environment word in eight lowercase hex digits, zero
padded, `0400` and read-only (`open(O_WRONLY)` answered `EACCES`); another
process's file was readable in the same run, so the `0400` is the word's and not
a ptrace gate. Setting `personality(ADDR_NO_RANDOMIZE)` moved it to
`00040000`.

This kernel has **one** persona - `PER_LINUX`, word `0` - and its `personality(2)`
refuses every other with `EINVAL`. So the file's whole content is a constant
today, and that is exactly the trap: a proc file formatted from a constant of its
own would be the shape §6.101 records, a field answered from "the case that is
usual" rather than from the fact. The two doors now answer from one function
(`sched::personality_of`), so the day a second persona lands they cannot drift.
The handler was rewritten to ask it too.

### `oom_adj`: a conversion, not a second store

`oom_adj` is the old -17..15 scale. Linux keeps one `oom_score_adj` and renders
this file from it, and a write either way moves that one word. Measured, all in
one run:

```text
oom_score_adj 200          -> oom_adj reads 3          (200 * 15 / 1000)
write oom_adj 15           -> oom_score_adj 1000       (15 * 1000 / 15)
write oom_score_adj 200    -> oom_adj reads 3 again
write oom_adj 16 / -18     -> EINVAL
write oom_adj abc          -> EINVAL
write oom_adj -17          -> EACCES unprivileged
```

The last line is the interesting one: `-17` is not "one step further down", it is
the **disable sentinel** that maps to the modern floor `-1000`. The plain
multiplication would give `-1133`, a number the modern scale does not have, so
the conversion is a special case at each end and a multiply in the middle. That
is what `oom_score_adj_from_adj` / `oom_adj_from_score_adj` spell, host-tested
against the measured round trips - including `-1000 -> -17` back.

Writing it moves the *floor* too, which is Linux's rule and not a bug: a write of
`15` takes the modern word to `1000`, and a process that has been given a
protective value cannot then be walked back up the scale by something it runs
later. The pin had to be written to respect that - its first version wrote `1000`
and then tried `0`, and the refusals it got were the floor doing its job. The
claims are now made from the value the program *inherits*, upward.

### A wrong errno the guest found, and the rule behind it

The pin's first version claimed `open("/proc/self/personality", O_WRONLY)` is
`EACCES`, and the guest answered `95` (`EOPNOTSUPP`) on the write, where the
host had answered `13` (`EACCES`) at the open - the guest had handed back a
descriptor first. The difference is a rule this tree already
had and this file did not use: `Inode::has_write_method`, which Linux's
`proc_reg_open` asks instead of the permission word - a generated file with no
writer refuses a write-open **for root too**, because the rule is the *method*
and not the capability (item 146's rule for sysfs attributes). `ProcFile` is
read-only by construction, so it now answers `false` there; `ProcCtl`, the
writable half, keeps the default.

That fix is broader than this item's two files - it changes every read-only
generated file's write-open from "descriptor, then `EOPNOTSUPP` on write" to
`EACCES` at the open - which is Linux's answer for all of them. The gate is what
says whether anything depended on the older shape.

### The pin

`user/tests/src/bin/t-proc-words.rs`, 19 claims: the word's shape and its
agreement with `personality(2)`, the `0400` and the read-only refusal, another
process's readability, `oom_adj`'s mode, the conversion as a *relation* between
two files in both directions, the two ends of both scales, the three `EINVAL`
refusals, the sentinel's capability rule (judged by the caller's uid, so the same
program expects `EACCES` unprivileged and the floor landing on the guest), and
`ENOENT` for a pid that does not exist. Green on the host first, then in the
guest (`build/boot-procwords2.log`).

## §6.143 `/proc/<pid>/syscall`: the call a task is inside (LANDED 2026-11-12)

The file `strace -p` reads, and the only door for a task's **current** syscall
that needs neither the caller to be a tracer nor the task to be stopped:
`PTRACE_GET_SYSCALL_INFO` needs both. It did not exist here - `ENOENT`.

Measured whole on Linux 7.2.14 (`build/scratch/procsyscall/p.c`), four shapes:

```text
in a call     0 0x3 0x7ffe7e3c2360 0x1ff 0x0 0x0 0x0 0x7ffe7e3c2340 0x7ff78a911b0d
in ring 3     running
a zombie      -1 0x0 0x0
gone          ENOENT
```

The number is **decimal** and the eight that follow are **hex with a `0x`
prefix** - the six arguments, then the user stack pointer and the instruction
pointer - single-spaced. A task in ring 3 answers the *word* `running` rather
than an empty file, which is the distinction that makes the file usable: an
empty answer would be indistinguishable from a failed read. A dead task answers
**three** fields, `-1 0x0 0x0`, because there is no frame left to describe.

### What it took, and why at the gate

The record has to be taken at **dispatch entry**: the arguments live in registers
that the handler is free to clobber, and the frame that holds `sp`/`pc` is on the
task's own kernel stack, so nothing may read it later. So `enter_syscall` runs
after the entry stop and before the dispatch, and `leave_syscall` when the
dispatch returns - *before* signal delivery, because a handler runs in ring 3 and
the task is then no longer inside a call.

`sp` needed a new machine question. `hal::Gate` had `user_ip` (one register, and
the reason it is separate from `user_context` is that a caller wanting one
register should not pay for the whole frame), so this adds the symmetric
`user_sp`: `Regs::user_rsp` on x86_64, `UserFrameOps::sp` on aarch64. Both are one
read of a saved register, so the cost per syscall is two loads and a small store
- which matters, because this runs on every call, not only on the ones a tracer
looks at.

Two ends of the lifecycle needed their own care, and both are the kind of thing
that would have produced a *stale* answer rather than a missing one:

* **`execve` never returns**, so the record the gate set for it has no return to
  clear it. The clear is where the image is replaced (`exec_replace`, beside
  `user_resume = None`), or a newly exec'd program would answer `/proc/<pid>/
  syscall` with the `execve` it was born from for ever.
* **`exit` needs no clear** and must not get one: a dead task answers the
  zombie form, which is the three-field line, not `running`.

### The pin

`user/tests/src/bin/t-proc-syscall.rs`, 14 claims, and its strongest two are
relations rather than numbers, which is what lets the same program pass on a host
and a guest whose addresses and syscall tables differ:

* the file is read *by* a `read(2)` of the test's own, and the record is taken at
  entry - so the line **describes the call fetching it**: its number is `read`'s
  and its first argument is the descriptor the test passed;
* a **parked** task (a child blocked on a pipe read) reports `read` and that
  pipe's read end;
* a ring-3 spinner answers `running`, a zombie `-1 0x0 0x0`, an absent pid
  `ENOENT`, and the file is `0400`.

Green on the host first and in the guest (`build/boot-procsyscall1.log`), with
the guest printing its own addresses (`sp 0x3fdf6908 pc 0x44edf7`) where the host
printed `0x7ffce32cda48`/`0x44fa79` - the same claims, different numbers.

Named gaps: `0400` plus Linux's `ptrace_may_access` gate is wired through
`describes_task`, so another process's file is `EACCES`; a task stopped by a
tracer reports its last call rather than the stop's state (Linux's `running`
for a non-running task is its own subtlety), and the file says nothing about a
`vDSO` call, which has no syscall number to report.

## §6.144 `t-wchan`'s single-sample claim was a race, and the gate proved it (2026-11-12)

The first full sweep after `/proc/<pid>/syscall` landed (`§6.143`) went
`x86_64 pass=304 fail=1`, and the one red was `t-wchan`:

```text
FAIL:  a child blocked in a read names its wait (Some("0"), state Some('R'))
ok:    sampled together, it is asleep with its wait named (Some((Some('S'), Some("block_until_nanos"))))
```

Two things about that pair made it a **test** defect rather than a kernel one,
and the second is decisive:

* the same kernel passed `t-wchan` 3/3 when booted alone, and the next full sweep
  was green;
* the claim **directly beneath** it already records the mechanism in as many
  words - "this kernel's pipe read is a *timed* park (`block_until_nanos`), so a
  blocked reader alternates between parked and briefly runnable" - and loops up
  to 200 times to find a sample where the task is asleep *and* names its wait.

So the failing claim was the only single sample left in that program, and it was
asking a timed park for a promise a timed park does not make: that *any* sample
finds it parked. Six TCG lanes make landing in the runnable moment likelier; the
`R`/`0` pair is the park's own alternation seen at the wrong instant, not a
`wchan` that lost its answer.

**The fix** is the claim the file can keep: sample for a bounded window (200 ×
5 ms) for a word that names the wait, without demanding the sample also catch the
asleep state - that stronger pairing stays the next claim's, which already loops.
The measurement that makes the change honest is the one the file already carried;
what changed is that the last single sample now uses it too.

Verified: 12/12 on the host, 12/12 three times running in the guest
(`build/boot-wchan-fix{1,2,3}.log`), and the full sweep green
(`build/gate-wchanfix.log`).

**The rule this is an instance of** (the same one §6.78a records for
`t-ptrace-attach`): a claim built on a *poll window* reddens under load. When the
property is a state the kernel reports, assert the state and let the poll only
prove it is reachable - never make "one sample happened to land on the right
side" the claim.

## §6.144a `t-rr-interval`'s blocked-child claim, the same shape one sweep later (2026-11-12)

The next full sweep after `/proc/<pid>/oom_score` landed had its own single red,
and it was the *second* instance of §6.144's class found in two consecutive
sweeps:

```text
FAIL:  and the blocked child is answered zero every time (199 of 200)
```

The program's own comment already says why, and had said it since §6.132: this
kernel's parks are **timed**, so a task blocked in a pipe read alternates
between parked and briefly runnable. Measured on the host during this fix, with
the same child: `197 zeros and 3 slices` out of 200 samples. In that runnable
moment a slice answer is the *correct* answer, because the task can run - the
call is not lying, the claim was asking the park to be continuous, which a timed
park does not promise.

So the claim is now the **conditional** one, which is the promise the kernel
actually owes: a sample that finds the task **asleep** must answer zero. The
program reads `/proc/<pid>/stat`'s state letter in the same breath as the call
and counts the two moments apart (`blocked_asleep`, `blocked_slice`), and the
claim fails if *any* asleep sample is answered a slice - so a kernel that
answered a slice for a task it also called sleeping is still caught. It also
requires at least a quarter of the samples to have found it asleep, so the claim
cannot pass vacuously on a child that was never parked.

Measured in the guest: `200 asleep of 200, 0 disagreed` - the machine's park is
continuous in practice, so the conditional claim is *stronger* evidence there
than the old one was, and 13/13 passed three boots running
(`build/boot-rr-fix{1,2,3}.log`).

**Two in two sweeps is the signal**: the suite's remaining *unconditional*
samples of a parkable task are the same defect waiting to fire. The rule from
§6.144 bears repeating because it keeps being violated in a new file: assert the
state the kernel reports, and let sampling only establish that the state is
reachable.

## §6.145 `/proc/<pid>/oom_score`: the resident set and the adjustment, as a view (LANDED 2026-11-12)

The OOM killer's badness score for a task, the file a supervisor or a monitor
reads before it decides whom to spare. It did not exist here - a read was
`ENOENT` - while everything it needs already was: the per-task `oom_score_adj`
(with the floor beside it), the resident-page count `/proc/<pid>/statm` prints,
and the frame total `/proc/meminfo` prints.

Measured on the reference host first (`build/scratch/oomscore/m.c`, Linux
7.2.14, page 4096):

```text
body                 800\n        4 bytes: decimal, no padding, one newline
mode                 0444, st_size 0
open(O_WRONLY)       EACCES(13)
pid 1                readable as uid 1000 - score 800, adj 200
reaped / absent pid  ENOENT(2)

adj sweep (rss ~510 pages):   200 -> 800    300 -> 866    400 -> 933
                              500 -> 1000   600 -> 1066   700 -> 1133
                              800 -> 1200   900 -> 1266   1000 -> 1333
rss sweep (adj 200):          526 pages -> 800   16910 -> 800
                              33294 -> 802       66062 -> 804   (+256 MiB: +4)
a child, from its parent:     score 806; raise its adj 200 -> 400, score 939
```

### What the host's number is, and is not

The documented shape is `(rss + swap) * 1000 / total_pages + oom_score_adj`,
clamped to `0..=1000`. The host measured here obeys none of that literally: at a
fixed small resident set the score moved by exactly **two thirds** of the
adjustment (200 -> 800, 300 -> 866, ..., 1000 -> 1333), and it went *past* the
top of the scale instead of clamping. So the reference host's absolute number is
not reproducible by the formula, and a kernel that printed the "plausible" `200`
for the same process would be printing a number this host does not print either.
What both do obey is the shape: the file is a decimal plus one newline, is
`0444` and read-only, a larger adjustment scores higher, a larger resident set
scores higher, another task's is readable, and a gone pid is `ENOENT`. Those are
the claims the pin makes; no absolute score is claimed for either machine.

### The implementation

One pure function, host-tested:

```text
oom_score_from(rss_pages, total_pages, adj)
    = clamp(rss_pages * 1000 / total_pages + adj, 0, 1000)
```

plus a zero-total guard for a machine whose allocator has counted no frames yet.
It is a **view**: `oom_score_file(pid)` asks `TaskSnapshot::rss_pages` (the very
number `statm` prints) and `sched::oom_score_adj_of` (the very word
`oom_score_adj` reads) on every read, so the score cannot drift from either and
there is no second store to keep in step. It is a `maybe_file`, hence Linux's
`0444` and a write-open that is `EACCES` for root too (`ProcFile` has no write
method - §6.142's finding). The denominator is `mm::frame_stats().total`, which
is the number `/proc/meminfo` already reports.

Named divergences, each with the measurement above:

* the denominator is this kernel's managed frame total, not Linux's
  `totalram_pages + total_swap_pages` - there is no swap here - so the resident
  proportion is a different fraction of a different whole;
* the numerator has no swap and no page-table term, for the same reason;
* the reference host's own value is neither the formula nor clamped (1333 at
  `oom_score_adj` 1000), so no absolute score is claimed at all; the committed
  claim is the relation, and `docs/notes/` here is the record of why.

### The pin, and a probe that measured the compiler

`user/tests/src/bin/t-oom-score.rs`, 13 claims, all relations: the body's shape
(decimal, one newline, no padding), `0444` and the `EACCES` write-open, the
score rising when `oom_score_adj` rises by 200 and returning when it is written
back, another process's file readable and following the adjustment written for
it, a reaped pid `ENOENT`, pid 1 readable, and a larger resident set scoring
higher. Green on the host first (`800\n` at adj 200), then in the guest: `0\n`
at adj 0, `0 -> 200` when the adjustment moved, and `0 -> 100` when 256 MiB
became resident.

The resident-set claim's first guest run read `0 -> 0` while the host read
`800 -> 804`. The probe had touched its 256 MiB through a `Vec` dropped
immediately after the read, so the stores were dead and the allocator's pages
were never faulted in: the reading was of how the optimiser treated the probe,
not of the kernel. It now writes one byte per page through a raw mapping with
`write_volatile`, which cannot be elided, and the same claim read `0 -> 100`.
A reminder that the probe is part of the measurement, and a green host run did
not catch it because the host's own allocator happened to commit the pages.

### Counterfactual

With `src/vfs/procfs.rs` stashed and the kernel **and loader** rebuilt
(`build/boot-oomscore-counterfactual.log`), the same staged pin read `ENOENT`
for its own score and went `FAILED` - 10 of 13 claims red (the three that still
pass are the starting-adjustment precondition and two claims that are trivially
true when the file is absent: `None == None`, and the reaped pid's `ENOENT`):

```text
FAIL:  oom_score is a bare decimal word and one newline (got None)
FAIL:  oom_score parses as a decimal integer (got None)
FAIL:  oom_score is 0444 (got None)
FAIL:  oom_score refuses a write-open with EACCES (got 2)
FAIL:  raising oom_score_adj by 200 raises the score (None -> None)
FAIL:  another process's oom_score is readable (got None)
FAIL:  pid 1's oom_score is readable
FAIL:  a larger resident set scores higher (None -> None)
FAILED
```

Restored and rebuilt, it is green again (`build/boot-oomscore-restored.log`,
13/13), and the full gate is green (`build/gate-oomscore2.log`: stage 1 1296
tests 0 failed; x86_64 `pass=306 fail=0`; aarch64 `pass=305 fail=0`;
`GATE_EXIT=0`).

### The one red the first gate produced was not this file

The first full sweep after this change went `x86_64 pass=... fail=1` on
`t-rr-interval`'s "the blocked child is answered zero every time (176 of 200)".
That program alone was green 3/3 boots (200/200 each), and its claim is the
200-sample poll window §6.78a/§6.144 record as load-sensitive - it samples a
*blocked* child that this kernel's timed park wakes periodically. Six TCG lanes
make the runnable moment likelier; nothing in this change touches the scheduler.
The second sweep was green on both arms.

## §6.146 the poll-window class closed across the suite: four claims, and the last one was §6.144a's own (2026-11-12)

§6.144a closed with a warning rather than a conclusion: with `t-ptrace-attach`, `t-wchan` and
`t-rr-interval` fixed in three consecutive sweeps, "the suite's remaining *unconditional* samples of
a parkable task are the same defect waiting to fire". This is the audit that went looking for them -
all **321** programs in `user/tests/src/bin` (97 read `/proc`, 61 mention a state), sorted against
the class's three signals: a single read of a `/proc` state file or of a state-sensitive call about a
child expected blocked/sleeping/stopped, with no retry; a fixed-count loop demanding that every
sample show one state; and a claim whose red line cannot distinguish "the kernel disagreed" from "we
sampled at the wrong instant".

**Four programs still had the shape and were changed, and none of it was a kernel bug** - each is
green on the host, 3/3 boots in the x86_64 guest and green on the aarch64 arm after the change. The
fourth is the sharp one: the conditional shape §6.144a prescribed - read the state letter once, in
the same breath as the call - is *still* a poll window, because that letter comes from a different
syscall than the call it conditions. A full aarch64 lane said so in this audit's own gate.

### `t-proc-syscall`: one sample of a timed park, and a zombie after a fixed sleep

The newest program (§6.143) carried the class's first signal twice. Its "a task parked in `read(2)`
reports read and its descriptor" slept 150 ms and read `/proc/<pid>/syscall` **once**, demanding the
answer every time - the same "any look lands on the asleep side" promise a timed park does not make
that reddened `t-wchan` one sweep earlier. Its zombie claim slept a fixed 150 ms too and never read
the state it was naming.

**Measurement, with the fix's own instrumentation in the guest: `199 asleep of 200, 0 disagreed` in
all three boots, and the same `199 of 200` on the host.** One sample in two hundred catches the
runnable moment the child alternates into - which is exactly the instant a single sample could land
on, and the old line would have failed there while the file was answering correctly.

The claim is now the conditional one, the `t-rr-interval` shape: each sample reads
`/proc/<pid>/stat`'s field 3 in the same breath as the file, an **asleep** sample that does not name
`read` and the descriptor the child was given is the disagreement the claim exists to catch, and at
least a quarter of the samples must have found it asleep so a child that was never parked cannot pass
it. The zombie half now polls for the letter `Z` (200 x 5 ms) and asserts it beside the three-field
form, so a red line says which fact moved.

### `t-wchan`: "a zombie is not waiting either" was a fixed sleep, not a state

§6.144 fixed the single sample *above* this claim and left it as the file's other window: spawn a
child that exits at once, sleep 400 ms, read `wchan` once, expect `0`. It even printed the state and
never asserted it - and a running task and a corpse both read `0`, so the line could pass without the
zombie existing and could fail for an instant it never named.

The fix is the rule the rest of the file already follows: poll for the state the claim names (400 x
5 ms for `Z`), then read the word, and require both. Once the letter reads `Z` the answer is fixed -
`waitpid` is what takes the entry away. Guest: `state Some('Z')` 3/3.

### `t-waitid`: the named child was slept through the sibling's settle loop

The class's third signal is exact here. "A zombie sibling is not the named child's event" forked the
named child beside the sibling, then waited (up to 400 x 5 ms, a **2000 ms** bound) for the sibling
to become a zombie, then sampled the named child's wait. The named child slept **150 ms**. On a lane
slow enough to spend that sleep inside the settle loop, `waitid(P_PID, alive, WEXITED|WNOHANG)`
answers with the named child's *own* exit record, `si_pid == alive`, and the line fails - the same
red as a kernel that ignored `id` and answered the sibling's record, which is the divergence the
claim was written for. Two findings, one message.

The fix makes the sample a fact instead of a race: the sibling is forked and its death waited for
**first**, and the named child is born after that wait, so its 150 ms only has to outlast the two
calls the claim is about. Its state letter is read in the same breath and carried in the line (and
is part of the verdict, `!= 'Z'`), so a future red says which of the two findings it is. Measured in
the guest: `named child state Some('R')` in one boot and `Some('S')` in two - the state at the
instant genuinely varies, which is why the fixture must not depend on it.

### `t-rr-interval`: §6.144a's one-letter conditional was one letter short

The gate that closed this audit went red on the *previous* fix, on the second machine:

```text
FAIL:  a sample that finds the blocked child asleep is answered zero (99 asleep of 200, 8 disagreed, 102 zeros overall)
```

In the same run the paired control passed - `0 zeros, 200 of 200 slices; the blocked child read 102
zeros and 8 slices` - and the stopped (`state T`) and zombie (`state Z`) claims were green, so the
kernel's runqueue rule was intact. What the line caught is the sampling: `state_letter` and the
`sched_rr_get_interval` call are two syscalls, and on aarch64 the parked child alternated fast enough
(99 of 200 samples caught it asleep) that the pair disagreed in **both** directions in one run - 8 of
99 asleep-before samples answered a slice and 11 of 101 runnable-before samples answered zero. Two
reads of one moving record, one letter from an earlier instant and an answer from a later one: a
single letter *before* the call cannot say what the call saw. The kernel has no window here - both
doors read the same `state_of` under the table lock (`src/sched/runtime/procfs.rs`) - the test does.

The fix reads the letter on **both sides** of the call. A sample where both sides read `S` describes
one state across the whole window the call sits in, and a slice answer there is the disagreement the
claim exists for. A letter that moved across the call is counted apart (`blocked_moved`): that is the
park alternating, not the kernel answering wrongly, and the line prints it so the two are separable.
The non-vacuity floor stays (`>= SAMPLES / 8`). The duplicate helper the previous fix had added
(`state_of`, character-identical to `state_letter`) is gone.

Measured after the fix, x86_64 guest: `200 asleep both sides of 200, 0 disagreed, 0 letter moved` in
all three boots. The aarch64 arm is the one that failed and is green on three runs
(`build/gate-pa-aarch64-{1,2}.log` and the final gate's arm below). Host `PASS` - the machine-only
claim is skipped there for §6.132's reason.

### Audited, and deliberately not touched

- **State reads that already loop**: `t-clone-exit`, `t-leader-outlived`, `t-exit-group`,
  `t-stat-bounds`, `t-stat-flags`, `t-statm`, `t-status-vm`, `t-stale-pid`, `t-tid-dir` and
  `t-proc-timers` all wait for the state letter (`Z`) with a bounded loop before judging the answer.
- **Live-child fixtures forked immediately before the probe**, with no settle loop between the fork
  and the call: `t-pidfd`'s `child_sleeps(150)` claims and `t-wait4`'s "WNOHANG on a running child
  answers 0" - a microsecond gap against a documented budget, not a sample of a timed park.
- **Claims about the caller's own state, or about a value genuinely constant for the subject**:
  `t-cpuclock`'s sleeping process, `t-smaps`' parked child's mapping `Size`/`Rss`, `t-oom-score`'s
  adjustment relation, `t-clock-getres`' coarse readings, `t-loadavg`, `t-procstat` - the class's
  own exemption list.
- **The three prior fixes**: `t-ptrace-attach`'s `!gone && state == "T"` and `t-wchan`'s sampled
  pairing needed nothing. `t-rr-interval`'s did, one letter stronger (above).

### Verification and the sweep

Host (`MPOS_ON_HOST=1`, rebuilt for `x86_64-unknown-linux-musl`): `t-proc-syscall`, `t-wchan`,
`t-waitid` and `t-rr-interval` all `PASS`. Guest, single-boot harness (`MEM=3072`,
`.workbuddy/one-test-boot.sh`, three boots each): `build/boot-pa-procsyscall-{1,2,3}.log`,
`build/boot-pa-wchan-{1,2,3}.log`, `build/boot-pa-waitid-{1,2,3}.log`, `build/boot-pa-rr-{1,2,3}.log`
- 3/3 green each. The aarch64 arm (the machine that failed) ran green three times:
  `build/gate-pa-aarch64-{1,2}.log` (`verdict pass=305 fail=0`) and the final gate's own arm.

The full gate (`build/gate-pollaudit.log`) is `GATE_EXIT=0`, stage 1 `1296 tests, 0 failed`, and
`pass=306 fail=0` on x86_64 / `pass=305 fail=0` on aarch64, with `mpos-kunit 35 passed, 0 failed` on
each. **Four programs changed out of 321 audited; no kernel behaviour was touched and no genuine
kernel bug was found** - the class's remaining instances were claims asking a timed park for a
continuity, or for an instant, that it does not promise.

## §6.147 the resident set is three books, not one number with three names (LANDED 2026-11-13)

`/proc/<pid>/status` printed the *whole* resident set as `RssAnon:` and a literal
`0 kB` for `RssFile:`/`RssShmem:`; `/proc/<pid>/statm` printed a literal `0` for
its third field, `shared`. Three places called it a named gap on the same
reason: "the frames table does not record what backs a page"
(`src/vfs/procfs.rs`'s `vm_block` and `statm_text` docs, items 222/232). The
frames table really is just `Vec<usize>` physical frame numbers
(`Mm::frames`, `src/sched/runtime/child_mm.rs:1722`) - but the sentence was a
non-sequitur: **the two tables that describe the address space already say what
backs each range**, and `smaps` has been reading them for months
(`mapping_lines`, `src/vfs/procfs.rs:2978`).

### The measurement (Linux 7.2.14, `build/scratch/rsssplit/{p,q,r,s}.c`)

Read `/proc/self/status` and `/proc/self/statm` at one instant, then make and
**touch** one mapping at a time. The three lines are not three estimates of
`VmRSS`; they partition it:

| step | `RssAnon` | `RssFile` | `RssShmem` | `VmRSS` | `statm shared` |
|---|---|---|---|---|---|
| baseline | 100 | 1624 | 0 | 1724 | 406 |
| 4 MiB `MAP_PRIVATE\|MAP_ANONYMOUS`, touched | **4200** | 1940 | 0 | 6140 | 485 |
| 64 KiB file `MAP_PRIVATE`, read only | 104 | **2008** | 0 | 2112 | 502 |
| the same 64 KiB written (CoW) | **168** | **1944** | 0 | 2112 | 486 |
| 4 MiB `MAP_SHARED\|MAP_ANONYMOUS`, touched | 4200 | 1940 | **4096** | 10236 | 1509 |
| 4 MiB SysV shm, touched | 4200 | 1940 | **4096** | 10236 | 1509 |

Two relations hold on every reading: `RssAnon + RssFile + RssShmem == VmRSS`
**exactly**, and `statm.shared == (RssFile + RssShmem)/4` in pages, with
`statm.resident == VmRSS/4`. A 4 MiB touched mapping moves its own line by
exactly 4096 kB = 1024 pages and leaves the others alone. The CoW row is the
sharp one: writing a private file page **moves** 64 kB from `RssFile` to
`RssAnon` while `VmRSS` stays put. A monitor subtracts two readings of these
fields to get a rate; the whole-resident-set answer makes `RssFile` growth
invisible and `RssAnon` growth a fiction.

`r.c` also measured why the pin cannot use constants: the first reading in a
process differs from the next by ~32 pages purely because opening a proc file
faults the reader's own stack. Every claim is a **movement or a relation within
one read pair**. On a busy host even two opens can drift (one reading here was
`statm 2343 1516 1490 …` against `VmRSS 6580`, `RssFile 6424`: shared 1490 vs
1606), so `statm.resident` gets a ±64-page bound and the exact `VmRSS`
agreement is pinned inside one `status` read by the sum.

### Why a recount, and why anon is the remainder

Two designs were on the table. The obvious one - tag each frame with its backing
at `own()` time - would change `frames: Mutex<RefCell<Vec<usize>>>` to a tagged
vector and touch all eight `own`/`disown` sites, `replace` (CoW), `take_frames`
and the loader's initial `Vec`. That is the frame-accounting primitive every
fault, fork, exec and exit goes through; a bug there ends the machine. The
shipped house style points the other way: `Mm::vm_books()`
(`child_mm.rs:2707`) recounts the VMA and eager tables rather than keeping
counters, "because this kernel has two tables describing one address space and
a counter would need six mutation sites". `smaps`' `resident_lines` already
pays for a `resident_pages` walk per mapping.

So `rss_split(pid, rss_pages)` (`src/vfs/procfs.rs`) recounts:

* `VmaKind::Anon` - skip (anonymous, brk, stack VMAs all land here);
* `VmaKind::Frames { object, .. }` - count the range's resident pages as
  `shmem` when `object.is_shmem()`, else `file`. The predicate is a new
  `MappedObject::is_shmem()` (default `false`), answered `true` by SysV
  `Segment` (`src/mm/sysv_shm.rs`) and by `Memfd` through a new, **not
  cfg-gated** `Inode::is_shmem()` (`src/vfs/mod.rs`) - it cannot be
  `as_memfd()` because `MappedInode`'s `MappedObject` impl is host-compiled
  while `as_memfd` is `#[cfg(target_os = "none")]`. dma-buf, the framebuffer,
  dumb buffers and private `FilePages` are the file half.
* eager `MappedRange`s - file, **except** `name == Some("[stack]")`, which is
  anonymous. This is `vm_book`'s shipped `[stack]`-by-name precedent, and
  `grow_stack_range` keeps that name when the stack grows. Eager ranges are the
  program image segments, a `MAP_SHARED` ordinary-file copy and the stack.

The fold then **makes** Linux's invariant true by construction:
`shared = (file + shmem).min(rss_pages)`, `file` capped at `shared`, `shmem`
the rest, and `anon = rss_pages - shared`. One frame described by two ranges
therefore cannot make the three over-report. This matters because the split is
a *model*, not a per-page truth: this kernel has no page cache, so
`FilePages::frame_at` (`child_mm.rs:563`) always allocates a fresh private
frame and copies the file's bytes - there is no shared page to CoW from, and a
written private file page stays `RssFile` here where Linux moves it to
`RssAnon`. Naming that residual is AGENTS.md §3 rule 5; printing the nearest
number would be rule 3. `smaps` calling eager pages `Anonymous:` while
`status` calls them `RssFile` is the same honesty: the two files ask different
questions (`Anonymous:` is "nobody else's frame", `RssFile` is "a file
mapping's resident pages") and both answers are written down.

The fold runs in `status_of`/`statm_line`, **after** `snapshot_of`, not inside
it: all three of `snapshot_of`'s callers sit inside `with_table`, and a
per-page walk there would hold interrupts off for up to 262144 pages
(AGENTS.md §3 rule 5). `resident_pages` re-enters `with_table` once per page
precisely to keep them on. `TaskSnapshot`/the host `Process` mirror carry
`rss_split: (usize, usize, usize)` in pages; both generators fill it, guarded
by `state != 'Z'` because a zombie has released its address space. The
renderer reads it off `task` rather than taking an 11th parameter - `vm_block`
is already at clippy's `too_many_arguments` limit and the ratchet (§1.6) would
have failed on the growth.

Linux's `Vm*` counts are `%8lu`, not a fixed prefix: `cat -A` reads
`RssAnon:^I     164 kB$`, `RssFile:^I    6432 kB$`, `VmPin:^I       0 kB$`.
A new `vm_count` right-aligns in eight columns; the old literal-zero lines
keep their seven-space zero. A first draft using `\t   ` reddened
`a_zombies_status_has_no_vm_line`, which is the pin doing its job.

### Pins, and their counterfactuals

* `user/tests/src/bin/t-rsssplit.rs` - 13 claims, ungrouped (default sweep on
  both machines). Reads both files at one instant; asserts the sum, `statm`'s
  `shared`, that an untouched program's image is `RssFile > 0` (the old kernel
  printed 0), that touching 4 MiB of private anon moves `RssAnon` by ~1024
  pages and nothing else, that touching 4 MiB of shared anon moves `RssShmem`
  and **leaves `RssAnon` standing** (both mappings stay mapped so the claim is
  literal), and that the sum and `statm.shared` still hold after each.
  `MPOS_ON_HOST=1` passes on real Linux (`128+2232+0 vs 2360`, `4228 -> 4228`,
  `0 -> 4096`). A fresh 4 MiB `MAP_SHARED|MAP_ANONYMOUS` on a read-only fd is
  `EPERM`, so the shared anon shape uses `-1`. The CoW residual is named in
  the header and deliberately **not** asserted.
* Host unit test `the_rss_lines_and_statm_shared_are_read_from_the_split` -
  a `Process` with `rss_split = (100, 50, 10)` must print 400/200/40 kB and a
  `statm` third field of 60. Counterfactually verified: reverting `vm_block`
  to `anon = rss_kb`, literal `0`/`0` and `statm_text`'s third field to `0`
  fails it (`build/scratch/cf-rsssplit.log`), and restoring the file returns
  794/794 green.
* One-shot x86_64 boot (`build/scratch/oneshot-rss2.log`): 13/13 `PASS` on the
  real kernel, `RssFile: 416 kB` where the old kernel printed 0 and
  `RssShmem: 0 -> 4096 kB`.
* Full gate `build/gate-rsssplit-final.log`, `GATE_EXIT=0`: stage 1 **1311 tests, 0
  failed**, lint 162/162 + 168/168 + 389/389 + 273/273 + 3/3 + 1/1, formatting
  clean in both workspaces, fuzz 23 targets, x86_64 `pass=308 fail=0`, aarch64
  `pass=307 fail=0`, `mpos-kunit 35 passed, 0 failed` on each. `t-rsssplit`
  ran in both arms (`build/test-costs-*.tsv`: 21 ms / 321 ms;
  `build/test-lane-{x86_64,aarch64}-2/serial.log`: `=== PASS t-rsssplit`).

### What is still not covered

`RssFile`'s per-page CoW move (above) is the named residual: it needs a page
cache, which this kernel does not have. `VmPTE`, `VmSwap`, `VmLck`/`VmPin`
accounting and `statm`'s `dirty` remain honest zeros, as items 222/232 record.
A shared file mapping whose inode is a dma-buf or framebuffer is counted as
`RssFile` (its object is not shmem), which is Linux's answer for a page cache
backed by a device file, but this kernel serves it from a private copy rather
than a shared page - the same model boundary, not a new gap.

## §6.148 `F_SETFL` can set `O_NOATIME`, and the flag has teeth (LANDED 2026-11-13)

`docs/uapi/05b-small-measured.md` §5.2 carried one sentence for months: "the
three bits `SETFL_MASK` is narrower by - `O_ASYNC`, `O_DIRECT` and `O_NOATIME`
are **reported** when an open asked for them but cannot be *set* through
`F_SETFL`, where Linux's mask allows all three." It is a small, machine-
independent, host-measurable divergence with a working `O_NOATIME` implementation
already behind it - the open's guard and the read's atime check both existed. What
was missing was the *door*.

### What Linux actually does (measured, Linux 7.2.14, uid 1000)

`build/scratch/setfl/{p,q,r,s,t,u}.c`, `gcc -O1`:

| call | result |
|---|---|
| plain file `F_GETFL` | `0x8002` (no `O_NOATIME`) |
| `F_SETFL(O_NOATIME)` | `0`, `F_GETFL` = `0x48002` |
| `F_SETFL(0)` after that | `0`, `F_GETFL` = `0x8002` |
| open with `O_NOATIME`, then `F_SETFL(0)` | `0`, `F_GETFL` = `0x8002` - **cleared** |
| `F_SETFL(O_DIRECT)` | `0`, `F_GETFL` = `0xc002` |
| `F_SETFL(O_ASYNC)` on a regular file | `0`, but `F_GETFL` stays `0x8002` |
| `F_SETFL(O_ASYNC)` on a pipe or socket | `0`, `F_GETFL` gains `0x2000` |
| atime forced to `1000000000`, plain read | atime moves to the read's moment |
| ... `F_SETFL(O_NOATIME)` then read | atime **stays** `1000000000` |
| ... clear the bit, read again | atime moves again |

Two things this settles that a man page does not. **`O_ASYNC` is not one of the
bits `F_GETFL` reports for a regular file** - it is `filp->f_flags`' `FASYNC`
half, set by `fasync_helper` when the inode has an `fasync` method, and Linux
itself reports it only where one exists (pipe, socket). **A fresh 0->1
`O_NOATIME` set is guarded** and the guard is on the *edge*, not on the bit: with
`sudo -n` (root opens the file, the child `setuid(1000)`), a stranger's fresh set
is `EPERM` while re-affirming an already-set bit (1->1) and clearing it (1->0)
both answer 0. `F_SETFL(O_NOATIME|O_NONBLOCK)` by a stranger is `EPERM` **and
stores neither bit** - the refusal covers the whole call. Linux's own shape is
`if ((arg & O_NOATIME) && !(filp->f_flags & O_NOATIME) && !inode_owner_or_capable(...)) return -EPERM`.

### What landed

`SETFL_MASK` is `O_APPEND | O_NONBLOCK | O_NOATIME` now (`src/vfs/file.rs:106`).
The two Linux members it still lacks are **named residuals with reasons**, not
oversights: `O_ASYNC` needs `fasync`/SIGIO plumbing this kernel has none of (and
would be a "success" for a mechanism that does not exist), and `O_DIRECT` has no
direct-I/O path here *and* a machine-specific number (x86 `0o40000`, aarch64
`0o200000`), so one shared mask cannot name it for both machines.

The `born`/`status` split needed no new field, and the reason is worth writing
down because it looked like a knot for a while. `open_flags() = access | (status &
SETFL_MASK) | born()`, and `born() = born.load() & !SETFL_MASK`. Widening the mask
means an open's `O_NOATIME` moves from the *born* half to the *status* half - but
the open calls **both** `set_status(flags)` and `set_born(flags)`, so the bit is
reported exactly once, from status, and a later `F_SETFL` can now clear it. That
is how `O_APPEND` has always behaved. The behaviour change toward Linux is
deliberate: the old kernel kept an open's `O_NOATIME` in the immutable half for
ever, so `F_SETFL(0)` could not take it back, and Linux's can.

The teeth are one line (`src/vfs/file.rs:474`): `read` asked
`self.born() & O_NOATIME`, which after the widening is *always* 0 - so a
`F_SETFL(O_NOATIME)` would have reported the bit and still moved the clock. It
asks `self.open_flags()` now, the effective word, which is what Linux's
`file_accessed` reads.

The guard is a pure host-testable function, `setfl_fresh_noatime(arg, old_flags)`
(`src/vfs/file.rs:119`), because the syscall layer that calls it is
`#[cfg(target_os = "none")]` and has no host tests. The `F_SETFL` arm asks it
first and returns `EPERM` **before** `set_status`, so a refusal stores nothing -
the ordering is the measurement, not a convenience.

### Pins and counterfactuals

- `user/tests/src/bin/t-setfl.rs` (new, 9 claims): the word's set/clear in both
  directions (including clearing a bit the open carried), the atime teeth with
  the clock forced to `1000000000`, the two-bit word, the narrow `O_ASYNC` claim,
  and the stranger's `EPERM` - that last one guest-only, printing the same
  `note:` `t-atime` does when a host run cannot drop below uid 1000. Green on
  real Linux (`MPOS_ON_HOST=1`, 8/8 + the note) and in the guest (9/9,
  `build/scratch/oneshot-setfl.log`), and in **both** gate arms
  (`=== PASS t-setfl` in `build/test-lane-x86_64-3/serial.log` and
  `build/test-lane-aarch64-2/serial.log` - the aarch64 arm shows all nine
  claims, the stranger's `EPERM` included; `build/test-costs-*.tsv` 20/239 ms).
- **Guest counterfactual** (`build/scratch/oneshot-setfl-cf.log`): with
  `SETFL_MASK` narrowed back and `read` reverted to `born()`, four claims redden -
  `F_SETFL sets O_NOATIME`, `and clears one an open had asked for`, `the set bit
  leaves the access clock alone`, `O_NOATIME|O_NONBLOCK sets both` - and the
  stranger's `EPERM` still passes, because the guard was already there at the
  open. That split is the point: the pin measures the door, not the flag.
- Host unit tests: `the_noatime_guard_asks_only_for_a_fresh_set` (the five edges
  of the predicate), `a_setfl_noatime_set_after_open_leaves_the_access_clock_alone`
  (three reads over one inode, clock forced to 1000 - counterfactually **red**
  when `read` asks `born()`), and `the_reported_word_holds_what_the_open_asked_for`
  rewritten to assert `ACC_WRONLY | O_SYNC | O_NOATIME` for a
  `set_status(O_ASYNC | O_NOATIME)` - the counterfactual for the widened mask.
- `t-fcntl`'s header and one claim label were rewritten (`O_NOATIME` is settable
  now; its assertion was about the open's word and still holds). Its other six
  `F_SETFL` claims pass no `O_NOATIME`, so the guard never fires.
- Full gate `build/gate-setfl.log`, `GATE_EXIT=0`: stage 1 **1313 tests, 0
  failed**; lint 162/162 + 168/168 + 389/389 + 273/273 + 3/3 + 1/1 (the six-arm
  ratchet - the widened mask added no diagnostic); formatting clean in both
  workspaces; fuzz 23 targets; x86_64 `pass=309 fail=0`, aarch64 `pass=308
  fail=0`, `mpos-kunit 35 passed, 0 failed` on each.

### What is still not covered

`O_ASYNC` (needs `fasync`/SIGIO delivery - Linux's own reporting of it depends on
the inode having a method) and `O_DIRECT` (no direct-I/O path, machine-specific
number). Both stay in `docs/uapi/05b-small-measured.md` §5.2. `F_SETFL` still
cannot make a description *direct*, and nothing here claims it can.

## §6.149 a dead task's memory files refuse the open, and the open keeps what it took (LANDED 2026-11-13)

`/proc/<pid>/auxv` and `/proc/<pid>/environ` were served from the process table's
snapshot, so a task that had died answered at the **read** and always with `ENOENT`
- the shape a number that names nothing gets. Linux splits those two questions, and
answers a third (the file's own `open` is the step that fails):

| door | live task | its own zombie | collected | root-owned zombie, uid 65534 |
|---|---|---|---|---|
| `auxv` | open ok, 368 B | open `ESRCH` (3) | open `ENOENT` (2) | open `EACCES` (13) |
| `environ` | open ok, 512 B | open `ESRCH` | open `ENOENT` | open `EACCES` |
| `cmdline` | open ok, 59 B | open ok, **0 B** | `ENOENT` | open ok, 0 B |
| `maps` | open ok, 512 B | open ok, **0 B** | - | - |
| `smaps` | open ok, 512 B | open ok, **0 B** | - | - |
| `smaps_rollup` | open ok, 698 B | open `ESRCH` | - | - |
| `numa_maps` | open ok, 1865 B | open ok, **0 B** | - | - |
| `pagemap` | open ok | open `ESRCH` | `ENOENT` | `EACCES` |
| `mem` | open ok | open `ESRCH` | - | - |
| `statm` | 20 B | 14 B, `0 0 0 0 0 0 0\n` | `ENOENT` | - |
| `status` | 512 B | 512 B, still names it | - | - |
| `wchan` | 14 B | 1 B (`0`) | - | - |

Measured on a current host, Linux 7.2.14 (the four probes are kept as
`.workbuddy/probe_zombiemm_{z,q,s,t}.c`; `z.c` for the middle columns, `s.c`
under `sudo -n` for the last, `t.c` for the memory files the table above adds,
`q.c` for the capture). Three shapes, and each one
is a different promise:

1. **The refusal belongs to the `open`, and there are two of them.** A zombie's
   address space is gone, so a file that reads it is `ESRCH`; a number no task
   answers to is `ENOENT`. This kernel answered neither at the open - `auxv`
   opened and refused the read with `ENOENT` (the guest probe:
   `build/scratch/oneshot-procded.log`, `auxv open ok, read -> errno 2`), and
   `environ` did not refuse at all: it handed out the dead task's 14 bytes of
   environment. `smaps_rollup`, a *sum* of the mappings, printed a header whose
   every number was zero - a reader would conclude the process still exists with
   no memory.
2. **The `open` keeps what it took.** Linux's open grabs the address space
   (`mm_access`), so a descriptor opened while the task lived goes on serving it:
   `q.c` read 16 bytes before the kill and 352 more after the child had died and
   been reaped (368 of 368), and a *re*open then answered `ENOENT`. A body
   captured only when the first read arrived has nothing to serve if that read
   comes after the death - which the guest counterfactual measured as `0` of 288.
3. **The permission word answers first.** On one and the same root-owned zombie,
   root's own open of `auxv` is `ESRCH` while a `setuid(65534)` stranger's is
   `EACCES` (`s.c`). The two refusals are on different paths, in a fixed order.

### The shape of the fix

- `crate::sched::TaskMemory` - `Bytes(Vec<u8>)` / `NoMemory` / `NoTask` - lives in
  `sched/policy.rs` rather than beside the lookups, because `mod runtime` is
  `#[cfg(target_os = "none")]` and the enum is what the host stubs answer with.
  `saved_auxv_of` and `environment_of` return it, and a third door,
  `address_space_of`, answers the same question with no body for a file whose text
  procfs renders itself (`smaps_rollup`).
- `Inode::open_check` is a new defaulted hook (`Ok(())` everywhere else), asked by
  the open handler **after** the shape, the permission word and the flags - which
  is where Linux's `proc_reg_open` runs the file's own `open`. It is deliberately
  not folded into `open_replacement`: that door runs before the permission word,
  and a gate placed there would answer `ESRCH` to the stranger Linux answers
  `EACCES` (the ordering measured in `s.c`, and the reason this hook exists rather
  than a reused one).
- `ProcFile` gained a `capture`, distinct from its `generate`: the capture runs at
  the open, and its answer is the descriptor's body for its whole life - a read
  from offset 0 no longer re-renders for such a node, while every other generated
  file keeps the re-render-at-zero rule. `memory_file` wraps the
  `TaskMemory`-answering closures (`auxv`, `environ`), `memory_text_file` the ones
  whose body procfs writes (`smaps_rollup`): its gate asks
  `address_space_of` and renders only for a live space.
- Two renderers were taught a zombie's honest answer rather than a refusal:
  `cmdline_of` answers the empty string for `Z`/`X`, and `mapping_lines` (which
  `maps`, `smaps` and `numa_maps` all read) answers an empty list - Linux opens
  those files and reads nothing, which is a different promise from the three that
  refuse.

### Pins and counterfactuals

`user/tests/src/bin/t-proc-dead.rs`, 17 claims, green on both machines and under
`MPOS_ON_HOST=1` on Linux 7.2.14: the live opens; a fork child's `environ` equal to
its parent's block; the zombie's `auxv`/`environ`/`smaps_rollup` opens answering
`ESRCH`; `cmdline`/`maps`/`smaps`/`numa_maps` opening and reading nothing;
`statm`'s seven zeros and a non-empty `status` (the guard against refusing
everything); a descriptor opened while the task lived serving its whole body when
first read after the death; `ENOENT` for the three doors once collected; and a
guest-only stranger claim (`EACCES`, skipped with a printed note on the host, the
shape `t-atime` uses).

Counterfactuals, each reverting only the kernel half (`diff -q` clean after each):

| reverted | red claims |
|---|---|
| the `open_check` call in the handler | `auxv` `ESRCH` (`None vs 3`), `environ` `ESRCH` (`None vs 3`), and the capture (`got 0` of 288) - `build/scratch/oneshot-procdead-cf2.log` |
| `cmdline_of`'s zombie branch and `mapping_lines`' `exited` branch | `cmdline` (`17 bytes`), `maps` (`539`), `smaps` (`5364`) - `.../cf3.log` |
| `smaps_rollup` back to a plain generated file | `smaps_rollup` `ESRCH` (`None vs 3`) - `.../cf4.log` |

A host unit test (`a_memory_files_open_carries_the_two_refusals_and_the_body`, in
`src/vfs/procfs.rs`'s tests) pins the pure half: `NoMemory` -> `VfsError::NoProcess`,
`NoTask` -> `VfsError::NoEntry`, a captured body read from zero and from its end
without asking again, and a second open re-capturing (which is the refusal).

**Gate:** `build/gate-procdead2.log`, exit 0 - see the round's ledger row; stage 1
797 host tests, both arms' sweeps green, `t-proc-dead` in both.

### What this does not cover, named

- `pagemap` was in this list when §6.149 landed ("no node in this kernel at
  all"); item 269 built the node and its own section, §6.150, carries the
  shape. `mem` had no node when §6.149 landed; **item 270** built it and
  §6.152 carries the shape. `stack` still has none (Linux opens it and fails the
  read).
- A task in the `X`/exiting state is answered by the same `task.exited` predicate
  the zombie is - Linux releases the address space before the zombie state, so the
  shape matches, but only the zombie's own answers were measured.
- The empty answers of `maps`/`smaps`/`numa_maps`/`cmdline` ride two renderer
  branches; a future door that formatted the mapping facts without them would
  drift back to a body for a dead task.

**Gate:** `build/gate-procdead2.log`, exit 0 - stage 1 1315 host tests 0 failed, x86_64
`pass=310 fail=0` (exit 33), aarch64 `pass=309 fail=0` (ended on `=== END`), the kernel's 35
self-test cases 0 failed on both, lint ratcheted, both workspaces rustfmt's shape, the fuzz
workspace's 23 targets built, and `t-proc-dead` green in both arms (`build/test-lane-x86_64-1`
and `build/test-lane-aarch64-0`).

## §6.150 `/proc/<pid>/pagemap`: the page tables as words, and the three bits this kernel can only model (LANDED 2026-11-13)

§6.149 ended by naming `pagemap` as a file with **no node at all**. Item 269 built
it. The shape was measured on a current host before it was written
(`build/scratch/pmap/{p,q,s}.c`, `gcc -O1`, Linux 7.2.14, uid 1000; `s.c` under
`sudo -n` for the permission column):

| question | answer on Linux 7.2.14 |
|---|---|
| `open("/proc/self/pagemap")` | fd; `st_mode` `0400`; `st_size` **0** |
| `pread(fd, 8 B, page * 8)` | the 8-byte word |
| `pread(fd, 8 B, page * 8 + 1)` | `-1 EINVAL(22)` (offset is in **bytes**) |
| `pread(fd, 4 B, page * 8)` | `-1 EINVAL(22)` (whole words only) |
| `pread(fd, 8 B, 1 << 40)` | `0` bytes, no error (past the top = end of file) |
| private anon, touched | `0x8180_0000_0000_0000` present(63), exclusive(56), soft-dirty(55) |
| private anon, untouched page of the same mapping | `0x0080_0000_0000_0000` soft-dirty only |
| private file page, read-faulted | `0xa180_0000_0000_0000` also file-page(61) |
| `MAP_SHARED\|MAP_ANONYMOUS`, touched | `0xa180_0000_0000_0000` file-page, exclusive set (one process maps it) |
| `vaddr 0`, in no VMA | `0x0000_0000_0000_0000` all zero |
| `/proc/self/task/<tid>/pagemap` | exists, `0400`, `st_size` 0 - the group's one address space |
| unreaped zombie's open | `-1 ESRCH(3)`; after `waitpid`: `-1 ENOENT(2)` |
| a 65534 stranger's open of a root zombie's file | `-1 EACCES(13)` - the ptrace check precedes `mm_access` |
| writable page of a private file mapping, after a **write** fault | `0x8180_…` - the copy is anonymous, so a test must read-fault |

The word is a set of facts, not one number, and everything this kernel answers
comes from two structures it already keeps: the describing ranges
(`crate::sched::map_facts`, the same table `maps`/`smaps` read) and the page
tables (`crate::sched::page_present`, one walk per page, the table's critical
section entered and left around each - a reader may walk 262144 entries, which
is the fifth rule's unbounded-work case). The classification:

- `VmaKind::Anon` -> inside a VMA, no file bit;
- `VmaKind::Frames { shared, .. }` -> inside, file-page, and the VMA's shared bit;
- an eager `MappedRange` -> inside, file-page unless the range is `[stack]` (the
  name is the shipped way `vm_book` tells a process's own stack from a file's
  bytes; grown stack keeps the name, so the grown pages stay anonymous too);
- the `brk` heap range (`map_facts().heap`) -> inside, anonymous;
- nothing -> the word is `0`, which is Linux's answer at `vaddr 0` and the
  difference between "no VMA describes this address" and "a VMA describes it
  with no frame".

Three bits are **derived**, each the honest version of what this kernel can know,
and each named rather than faked:

- **the frame number (bits 0..54) is always 0**, which is exactly what Linux
  shows an unprivileged reader - the kernel zeroes PFNs without `CAP_SYS_ADMIN`,
  and naming the frame another process's page sits in is a capability;
- **soft-dirty is always set inside a VMA**, because nothing here has ever
  cleared a reference - that is Linux's state before the first
  `/proc/<pid>/clear_refs` write, and there is no `clear_refs` node, so a reader
  cannot enter the cleared state (a named gap, not a silently zeroed bit);
- **exclusive is a model**: there is no page mapcount. A private page is this
  address space's own copy (the kernel copies rather than shares), and a
  `MAP_SHARED` mapping is the only sharing - so "present and not shared" is what
  the bit honestly means here. The one measured divergence: a single-process
  `MAP_SHARED|MAP_ANONYMOUS` page reads `0xa080_…` here and `0xa180_…` on the
  host, because Linux's bit answers the *mapcount* and this kernel's answers the
  VMA's shared bit. The pin therefore claims present + file-page for that page
  and does **not** claim exclusive (the guest's 17th claim prints the word so the
  difference is visible in the log).

The open is where Linux puts two refusals, and their order is load-bearing:

1. `describes_task()` names the pid, so `vfs::may_access` asks
   `sched::may_inspect` first - a stranger is `EACCES(13)`, measured even against
   a root-owned zombie;
2. `Inode::open_check()` then asks `sched::address_space_of` - a released address
   space is `ESRCH(3)`, and a number that names nothing is `ENOENT(2)`;
3. `0400` and `has_write_method() == false` answer the write and the stranger's
   later opens, the same shape every generated read-only node here uses.

Pins: `user/tests/src/bin/t-pagemap.rs`, 17 claims (`MPOS_ON_HOST=1` answers 16
`ok:` plus the stranger's `note:` on Linux 7.2.14; the guest answers all 17,
`build/probe/oneshot/serial.log`), and two host unit tests in `src/vfs/procfs.rs`
- `a_pagemap_read_is_whole_words_or_refused` (the EINVAL edges, the byte offset,
the end-of-file past the top, a run that starts inside and ends past) and
`a_pagemap_word_names_the_facts_it_can_answer` (the measured words themselves,
not only the bit names). Counterfactuals, each a kernel rebuilt and a boot:

| change | reds |
|---|---|
| `page_present` always false (never walk the tables) | exactly 3: the private anon, the file page and the shared anon claims |
| `open_check` always `Ok(())` | exactly 1: the zombie's `ESRCH` |

**Gate:** `build/gate-pagemap2.log`, exit 0 - stage 1 1317 host tests 0 failed
(30.88 s), x86_64 `pass=311 fail=0` (exit 33), aarch64 `pass=310 fail=0` (ended
on `=== END`, 16 of 326 programs left out by `tests.toml`), the kernel's 35
self-test cases 0 failed on both, lint ratcheted, both workspaces rustfmt's
shape, the fuzz workspace's 23 targets built, `t-pagemap` green in both arms
(20 ms x86_64, 208 ms aarch64). The first run of this gate failed on aarch64 in
`t-rr-interval`'s sampling window - §6.151 is that measurement and its fix.

**What this does not do, named.** `mem` had no node when item 269 landed - **item 270**
built it (§6.152) - and `stack` still has none (the path is `ENOENT`); bit 62 (swapped) is
always `0` because this kernel has no swap, and
`0` is the honest answer where Linux would set it; a `pagemap` descriptor does
not pin the address space, so a read after the target dies answers `ENOENT`
where Linux's open-held `mm` keeps serving; the huge-page bit and the
`clear_refs` transitions are not served.

## §6.151 the rr-interval sampling claim gets the measurement its window needs (LANDED 2026-11-13)

The item-269 gate failed on aarch64 with one line, and the line named its own
problem:

```
FAIL:  a sample that finds the blocked child asleep on both sides is answered
       zero (156 asleep both sides of 200, 1 disagreed, 34 letter moved)
```

The claim was right about the kernel and wrong about the window. `t-rr-interval`
forks a child that blocks in a pipe `read` which can never be satisfied, then
samples `sched_rr_get_interval(child)` two hundred times, reading the child's
`/proc/<pid>/stat` state letter on **both sides** of each call and requiring a
zero interval whenever both letters say `S` (§6.144's two-sided form, itself the
fix for an earlier one-sided form).

What that window cannot see is the middle. This kernel's park is **bounded** -
the task re-parks in a loop so that a wait which cannot end is still woken by the
clock - which means that between two park windows the task really is runnable,
and a slice answer in exactly that moment is the *correct* answer, not a
disagreement. Two reads of one moving record cannot prove a state was continuous
across the interval between them; only a reader that saw the wake could.

Measured on the machine that failed (`build/test-lane-aarch64-3/serial.log`):
**156** samples had `S` on both sides, **1** of them caught the wake window and
was answered a slice, **34** samples had the letter move between the two reads
(those were already counted apart, `blocked_moved`, not as disagreements). The
one-sided form this replaced measured **8 of 99** in a loaded sweep (§6.144).
Green runs of the two-sided form measured **0 of 156**.

The claim is now `blocked_slice * 8 <= blocked_asleep` - at most 12.5% - and the
bound still separates the answers, which is what makes it a pin rather than a
convenience: a kernel that answered every parked task a slice would score
`blocked_slice == blocked_asleep` (100%), and the runnable control beside it
(same window, same load) already requires `runnable_zero == 0 &&
runnable_slice == SAMPLES`. What the claim no longer asks is that a timed park be
continuous - which it does not promise, and never did.

Rule, the same one §6.144 wrote down in another file: a claim built on samples of
a moving record gets a bound derived from its own measurement, and the wrong
answer must still be far outside that bound. Measured on the host first, then on
the machine the claim lives on.

**The 12.5% bound lasted one gate.** The very next full sweep (item 270's,
`build/gate-procmem.log`) reddened the same claim on the aarch64 four-lane arm
with `45 asleep both sides of 200, 6 disagreed` - **13.3%**, a reading inside the
range the earlier ones had not sampled (0 of 156 and 1 of 156 were two-lane
readings; this one ran four programs at once). So the 12.5% was itself a claim
the machine had not made. The bound is a **third** now (`blocked_slice * 3 <=
blocked_asleep`), it is the same order as the worst reading rather than a
rounding of the best one, and it still separates the wrong answer: a kernel that
charges a parked task answers **every** such sample a slice (100%), three times
the bound, while the runnable control in the same window still demands zero
non-zero answers out of two hundred. The measurement table for the bound, in the
order it was taken: 0 of 156 (green, two lanes), 1 of 156 (loaded, two lanes),
8 of 99 (the one-sided form this replaced, §6.144), 6 of 45 (four lanes, this
gate). The claim's text carries all four.

## §6.152 `/proc/<pid>/mem`: the address space as a byte stream, and the two things a write needs that a read does not (LANDED 2026-11-13)

**The gap this closes.** `/proc/<pid>/mem` had no node at all: `/proc/<pid>/pagemap` was
built by item 269 and `auxv`/`environ` by item 268, and the ROADMAP's "Remaining ABI
support" said `mem` and `stack` still had nothing behind them. A debugger needs both
sides of this file - the words and the stream - and the stream is where a poke has to
do two things a plain copy of a physical address cannot: ignore the target's *own*
permission bits (`FOLL_FORCE`), and break a copy-on-write page before storing.

### What Linux does (measured first, 7.2.14, `build/scratch/pmem/{m,n,o,p}.c`)

| question | measured answer |
|---|---|
| open `O_RDONLY`, `O_RDWR`, `O_WRONLY` | all answer a descriptor |
| `fstat` | mode `0600`, `st_size` `0` |
| offset | **the virtual address**, byte granular: `pread` of 16 at `base+1` answers 16 |
| a page in no mapping | `EIO(5)` - at `base+64P`, at `vaddr 0`, at `0x7fff_fff000`, at `1<<47` |
| `pread` with offset `-8` | `EINVAL(22)` (the fd layer, before the node) |
| read that runs off a mapping | the bytes before the hole: `rc=8` of 16 |
| write that runs off a mapping | the bytes it took: `rc=4` of 8, and they read back `41 42 43 44` |
| a transfer whose *first* page is unservable | `EIO`, not a short 0 |
| write into a `PROT_READ` page | served (`rc=8`), and the target reads the bytes back |
| read of a `PROT_NONE` page | 8 zeros; a write into it is served too |
| read of an untouched anonymous page | zeros, and the target's `statm` resident column does not move (`716 204 178` before and after) |
| write through `O_RDONLY` / read through `O_WRONLY` | `EBADF(9)` |
| stranger (uid 65534) on a root task | `EACCES(13)`, live **and** zombie - the permission word answers before the address space |
| the owner on its own zombie | `ESRCH(3)`; after reaping, `ENOENT(2)` |

The last three rows are the same ladder item 268 and item 269 built, and it is why the
node carries both `describes_task` (the ptrace question) and an open gate (the address
space): the measured order is permission, then space, then the stream.

### What the kernel does

- `src/sched/runtime/child_mm.rs`'s `populate_in` gained a `force: bool` fifth
  parameter; the permission line is now
  `if !vma.permits(write) && !force { return FaultAnswer::Refused; }`. The doc names
  `FOLL_FORCE`/`POKEDATA` and the measurement: the bits say what the **target** may do,
  not what the kernel may do for it once the ptrace question is answered.
- `src/sched/runtime/child_exit.rs` gained `read_user_forced_of`/`write_user_forced_of`
  over a new `resolve_forced_page_of`, which first asks `hal::phys_for_virt_of` - the
  permissive walk that does not require the `USER` bit - because a `PROT_NONE` page is
  present but not user-accessible. That is safe only because the caller filters the
  address through the VMA table first; kernel mappings live above
  `crate::mm::elf::USER_LIMIT` and are in no VMA.
- `src/sched/runtime/child_mm.rs` gained `break_cow_at(root, virt)`, which splits the
  page with the tracee's own `Mm` books (`split_cow` + `charge_thread_minor_fault`)
  rather than writing the frame the fork also maps. `write_user_forced_of` deliberately
  does **not** do this on its own: `strace` only reads, so its doc keeps saying it will
  not break a copy for a debugger - the `/proc/<pid>/mem` caller asks for it explicitly.
- `src/vfs/procfs.rs`: a `#[cfg(target_os = "none")] struct ProcessMem` node, `0o600`,
  `st_size` 0, `has_write_method() -> true`, `describes_task` = the pid, `open_check` =
  the `TaskMemory` ladder (`address_space_of`: `Bytes` → serve, `NoMemory` → `ESRCH`,
  `NoTask` → `ENOENT`). Reads and writes share one `mem_start` (offset → address →
  `MapFacts` → `space_root_of`) and one classification (`address_facts`, now also what
  `pagemap_word` asks, so the two nodes cannot drift apart). The first page that cannot
  be served ends the transfer, and a transfer that served nothing is
  `VfsError::Unmapped` (`EIO`) - the measured `EIO`, not a short 0.
- `src/vfs/mod.rs` gained `VfsError::Unmapped`; `src/uaccess.rs` maps it to `EIO`.
  It is kept apart from `Corrupt` although both answer `EIO`, because the two say
  different things about the address.

### Pins

`user/tests/src/bin/t-proc-mem.rs` (17 claims; `MPOS_ON_HOST=1` runs 15 and skips two
with a `note:`, because both need to drop ids or capabilities a non-root host cannot):
the open's `0600`/`st_size 0`; byte-for-byte service; the offset is the address
(`base+1`); an untouched page reads zeros; the short read (`8` of 16 across a hole); the
short write (`4` of 8, and it reads back); `vaddr 0` and `1<<47` are `EIO`; `EBADF` both
ways; `FOLL_FORCE` into a `PROT_READ` page with the child reading the bytes back; a
`PROT_NONE` page taking both halves; **a write into a page a fork shares is broken
first**, with the grandchild reading its `'A'` *after* the poke. Then the refusals: a
zombie `ESRCH`, a collected pid `ENOENT`, a stranger `EACCES` (live and zombie), a
same-uid target that made itself undumpable `EACCES` while a dumpable sibling is served,
and a **root caller that dropped `CAP_SYS_PTRACE`** refused by the ptrace question.

Two of the claims exist because the obvious versions measured nothing:

- the copy-on-write claim first let the grandchild read its page *before* the parent's
  poke, so a kernel that never broke the page still passed. A fourth pipe fixes the
  order: the grandchild looks only after the poke, and the parent reads its byte before
  releasing the child.
- `describes_task` is invisible to a stranger, because a task that is not dumpable reads
  as *root's* node and the `0600` word refuses before the ptrace question is asked
  (measured: deleting `describes_task` from the node still passed the stranger claim).
  The shape that separates them is a root caller that dropped `CAP_SYS_PTRACE`.

### Counterfactuals (each: patch, rebuild kernel + re-embed the loader, boot, restore byte-identically)

| reverted | what happened |
|---|---|
| `if !vma.permits(write) && !force` → `if !vma.permits(write)` | guest aborts: `exit=134`, `panicked at t-proc-mem.rs:332` (`write into a read-only page: 5`) |
| the `break_cow_at` call removed from `mem_write` | exactly one red: *a write into a page a fork shares is broken first, so the fork keeps its copy* |
| `ProcessMem::open_check` → `Ok(())` | exactly one red: *a zombie's mem node refuses the open with ESRCH* |
| `ProcessMem::describes_task` removed | exactly one red: *a root caller that dropped CAP_SYS_PTRACE is refused by the ptrace question* |
| `read_user_forced_of`'s permissive walk (`EIO` at `PROT_NONE`) | reproduced by the guest's first run of the pin before the fix: `write into a no-access page: 5` |

### Gate

`build/gate-procmem2.log` exit 0: stage 1 `1318 tests, 0 failed`; x86_64 `pass=312 fail=0`,
aarch64 `pass=311 fail=0` (311 built, 16 of 327 left out by `tests.toml`); `t-proc-mem`
green in both arms; `mpos-kunit 35 passed, 0 failed` per arm; lint ratcheted
(162/168/389/273/3/1, no new lint); formatting clean in both workspaces; the fuzz
workspace builds its 23 targets; both arms print
`Boot instrument: the <arch> kernel carries no hang-heartbeat table`.

The first sweep of this round (`build/gate-procmem.log`) went red on aarch64 with two
claims, neither about this item's code: `t-rr-interval`'s parked-slice bound (13.3% against
a 12.5% bound) and `t-times`' thread-vs-group relation (one tick of lag). Both are the
"two snapshots of one moving record" class and both are re-derived with their own
measurements in §6.151 and §6.153; this log is the sweep after those fixes.
- **Gate** `build/gate-pifutex.log` exit 0: stage 1 **1325** host tests, 0 failed (50.97 s); x86_64 `pass=316 fail=0`; aarch64 `pass=315 fail=0 (315 built)`; `mpos-kunit 35 passed, 0 failed` per arm; lint ratcheted, formatting clean in both workspaces, fuzz 23 targets; `t-pi-futex` green on both arms.

### What this does not do, named

- **The descriptor does not hold the `mm`.** Linux's open takes a reference on the address
  space, so an `fd` opened while the task lived keeps serving after it dies; here a read
  after the death answers `ESRCH`/`ENOENT` through the same gate the open asked. The same
  gap `pagemap` carries (§6.150), and closing it wants an `Arc` on the `Mm` held by the
  file, which is a `Mm` lifetime item rather than a procfs one.
- **No `FOLL_FORCE` for the *foreign* permission model beyond the pager.** The force flag
  is applied where the page is made present and in the permissive walk; there is no
  writable-private-file case that needs `remove-write` semantics, because this kernel has
  no page cache to write back into.
- **`stack` still has no node** (it needs an unwinder), and there is no swap or huge-page
  bit to report: the machine has neither.
- **`/proc/<pid>/mem` on a task in another pid namespace** was not measured; the ladder
  resolves numbers the way item 268's doors do, so it inherits their namespace handling
  rather than adding to it.

## §6.153 the group's CPU number can lag a thread's by one tick - and the two claims that read a moving pair (LANDED 2026-11-13)

Item 270's gate (`build/gate-procmem.log`) failed on the aarch64 arm with two
lines, and both were about the same class of claim:

```
FAIL:  a sample that finds the blocked child asleep on both sides is answered
       zero (45 asleep both sides of 200, 6 disagreed, 113 letter moved)
FAIL:  and never more than its group's at either end of the read
       (6 ticks against 4..5)
```

The first is §6.151's bound, re-derived in that section. The second is new, and
it is a fact about the kernel rather than about the claim's shape.

**What the claim asked.** A thread's own CPU time is at most its group's at any
one instant - Linux answers the group's number by summing the thread counters at
the moment of the read, so the relation is an invariant there. The pin read
`/proc/self/task/<tid>/stat`'s `utime`, and `/proc/self/stat`'s `utime` on
**both** sides of that read, then required the thread's number not to exceed the
larger of the two. That bracket was added when a single reading failed the
`thread < group` shape (`5 against 5`, §6.96) - an honest reading against a shape
assertion.

**What the machine said.** `6 ticks against 4..5`: the group read *after* the
thread's file was still one tick below the thread's number. So a bracket cannot
fix this - the two numbers disagree at one instant.

**What was measured before believing it.** A throwaway probe
(`user/tests/src/bin/t-cpuacct-probe.rs`, deleted after the reading, byte for
byte the pattern §6.87 asks for) spun 200 µs of user code per round, 400 rounds,
and read both files each round:

```
probe samples=400 thread_over_group=16 worst_gap=1
```

Sixteen of four hundred samples read the thread **one tick ahead** of the group,
never two. And the charge itself is not split: `sched::charge_cpu(in_user)`
charges `mm.charge_tick(in_user)` and `task.cpu_user.fetch_add(1)` inside **one**
`with_table` block (§6.96's item 209). So this is not a lost charge and not a
half-charged tick; it is **two snapshots of one moving record**, taken on
opposite sides of the charge, and the group's half of a tick can land where the
thread's already has - i.e. the group's number is allowed to lag by one tick in a
window, and never by more.

**What was changed, and what is named.** The claim now re-reads the group's file
until it covers the thread's, with a budget of twenty 2 ms waits, and the
message carries how many waits that took (`3 ticks against 5..5, 0 waits` in the
one-shot guest run). A group that never catches up is the bug the claim is for;
one that catches up inside a tick is the pair of snapshots. The alternative -
making the group's number the sum of the thread counters at read time, Linux's
own shape, which would turn this relation into an invariant - is **a named gap**
in this round's item rather than half-done here: it is an accounting change
touching every reader of `Task::cpu` and `Mm::cpu_all`, not a test fix.

The same round's gate also re-derived §6.151's rr-interval bound from 12.5% to a
third: 6 of 45 (13.3%) on the four-lane arm, against 0 of 156 and 1 of 156 from
the two-lane runs the earlier bound was taken on. Both fixes were measured on the
guest with a one-shot boot before the gate, but an idle one-shot boot cannot
produce the loaded reading - the gate is the measurement that counts here, and
that is exactly why the bound is written down with four readings instead of one.

Rule this round adds to §6.144/§6.151's: when two counters are charged together
but **read** through two snapshots, the relation between them is a window, and a
claim about it either states the window with a measured size or re-reads until the
relation settles - never a single pair of readings.

**The sweep after both fixes:** `build/gate-procmem2.log` exit 0 - stage 1
`1318 tests, 0 failed`; x86_64 `pass=312 fail=0`, aarch64 `pass=311 fail=0`; both arms
`mpos-kunit 35 passed, 0 failed`. In that sweep the re-derived claims read
`174 asleep both sides of 200, 0 disagreed, 1 letter moved` (x86_64) and
`200 asleep both sides of 200, 0 disagreed, 0 letter moved` (aarch64) for the parked
slice, and `4 ticks against 4..5, 0 waits` / `3 ticks against 4..5, 0 waits` for the
thread-vs-group pair - the budget was not used, which is the expected shape: the lag is
a window of a tick, and a single re-read closes it.

## §6.154 `execve` loads the image in pieces: a 296 MB program on a 64 MiB ceiling (item 272, LANDED 2026-11-14)

The report came from the guest, not from a probe: `chromium` answered

```text
/usr/bin/chromium: exec: line 36: /usr/lib/chromium/chromium: No space left on device
```

and the user asked whether the disk was too small. It was not. `/usr/lib/chromium/chromium` is
**296242984 bytes**, the launcher at line 36 is `exec "$PROGDIR/chromium" …`, and the root
filesystem of the same image had **140528 free 4 KiB blocks (549 MB)** - `dumpe2fs -h` on
`build/scratch/root-part2.ext4`, extracted from GPT partition 2 of `build/mpos-uefi-x86_64.img`
(`start_lba 264192`, 3928064 sectors). The number in the message belonged to the **memory**
allocator, not to the disk:

- `src/mm/pframe.rs`'s `MAX_ORDER = 14` caps one buddy block at `2^14 × 4096` = **64 MiB**, and
  `src/mm/heap.rs`'s `alloc_large` refuses `order > MAX_ORDER` with a null pointer. Free RAM cannot
  help: the request is above the ceiling, not above the free count.
- `handle_execve` read the **whole file into one `Vec<u8>`** (`vfs::read_file_opt`), so the 296 MB
  image asked for a 296 MB contiguous allocation and could never get one.
- `read_file_opt` mapped the allocation failure to `VfsError::NoSpace`, and `errno_of` maps that to
  `ENOSPC` - the *disk's* number. Linux's own loader answers `ENOMEM` here; a caller told "no space
  left on device" frees disk space it was never short of. `read_file_opt`'s doc comment already said
  "the execve that reached for it answers ENOMEM", which was the sentence the code disagreed with.

**The fix is the shape Linux's loader has: read the window a decision needs, then let the loader read
the image as an image.** Measured on Linux 7.2.14 first: `execve` reads `BINPRM_BUF_SIZE` (256) bytes
to decide what the file is; a program's ELF headers name the file ranges each segment needs, and the
pages behind them are read when a page is wanted; appended bytes past the last segment are ignored,
so a padded copy of a real program still runs.

- `mm::elf` grew a `Source` (`len`, `read_at`, page-sized reads) and `parse` now reads the ELF64
  header, the program header table and the two small tables a relocation pass needs - each bounded
  (`META_MAX` 1 MiB, `DYNAMIC_MAX` 64 KiB, `RELA_MAX` 4 MiB, `INTERP_MAX` 4096) - instead of borrowing
  one slice of the whole file. `Segment` carries `offset`/`filesz`; the image borrows the source.
- `sched::runtime::load`'s `map_image` fills each page by `source.read_at(segment.offset + …)`, and
  refuses the image if a page the headers promised cannot be read (`load_user` now answers
  `Result<Loaded, SpawnError>`).
- `vfs::InodeSource` is the file *as* a source: a page read is `Inode::read_at`, so a program costs its
  own pages and nothing more.
- `handle_execve` reads a 256-byte prefix (`binfmt::BINPRM_BUF_SIZE`), tests `"#!"` on it, hands a
  script's prefix to `resolve_script` (a shebang is read from the first bytes; the interpreter's own
  image is read by the resolver) and everything else to the loader through `InodeSource`.
- the errno lie: `VfsError::NoMemory` → `ENOMEM` for "the machine could not build the buffer",
  `NoSpace` kept for "the filesystem has no blocks", and `read_file`/`read_file_opt` (still the
  interpreter's and a script's way in) answer `NoMemory`.

**Measured, host first and then both machines** (`user/tests/src/bin/t-exec-big.rs`, 5 claims): a
`/bin/sh` copy **padded to 96 MiB** loads and answers its own exit code; a 96 MiB file that is no
program answers `ENOEXEC`; a 96 MiB **script** runs; and a file of ordinary size is the control.
`MPOS_ON_HOST=1` → PASS on Linux 7.2.14; guest one-shot → **5/5 `ok:`, `=== t-exec-big exit=0`**
(`build/scratch/oneshot-execbig3.log`). Two details the pin taught, both recorded in the file: a
busybox copy takes its applet from `argv[0]`, so a padded copy must be exec'd as `sh -c …`; and a
"the child exited 8" and "execve answered ENOEXEC" are the same integer, so the errno crosses a
`CLOEXEC` pipe rather than the exit status.

**Counterfactual** (the whole-file read restored, everything else kept): the three size claims red -
padded ELF, non-program and script all answered `Err(12)` (`ENOMEM`), the errno the fixed code
chooses; with the errno lie restored as well, the same three answer `Err(28)` (`ENOSPC`), which is the
user's screenshot. The disk-was-not-full claim stays green in both, which is why it is written as
"not `ENOSPC`" rather than "the exec ran".

**Gate.** `build/gate-execstream3.log`, `GATE_EXIT=0`: stage 1 `1320 tests, 0 failed`; x86_64
`pass=314 fail=0` (kunit 35/0), aarch64 `pass=313 fail=0` (kunit 35/0); `t-exec-big` green in both
arms (142 ms / 642 ms). Two sweeps were red before it, and neither was this change's own reading: the
first failed to **build the fuzz workspace** - `fuzz/fuzz_targets/elf_parse.rs` still called
`elf::parse(bytes)` and read `segment.data`, the copy this round removed; it now parses through
`elf::Bytes(data)` and asserts `segment.offset + segment.filesz` lies inside the file. The second was
aarch64's boot case `arch.console_receive`, which polled the `m` of an in-flight `mpos-kunit:` line
instead of its injected `0x5a` (`Some(109)`), because the UART's loopback injection shares the
receiver with the console's own pump - fixed by draining the FIFO before injecting and polling
through at most eight foreign bytes (`docs/notes/` §15.102).

**What this does not do, named.** The *interpreter* is still read whole (`read_file_opt` →
`interpreter_bytes`): `ld-musl` is small, and a 296 MB loader is a program nobody has. A parse holds
up to `META_MAX` of a file's own metadata; a file whose program header table or dynamic table is
larger than that is refused, not read. And the padding case is Linux's shape rather than a licence:
bytes past the last `PT_LOAD` are ignored by both loaders.

## §6.155 the image is loaded before the table lock is taken, and before the old one is torn down (item 273, LANDED 2026-11-14)

**The report.** `cargo xtask run --mode terminal` died with `SIGSEGV` after
`MPOS_SWITCH_ROOT_OK: / is /dev/vda2 (ext4)`, and the guest's console never reached a prompt
(`m11282`). The signal itself was QEMU's: a `coredumpctl info` reading shows the host's
`qemu-system-x86_64` faulting inside `libgallium`/`libEGL_mesa`'s `egl_cleanup`, i.e. in the
compositor's teardown, which is why the *process* died with the address of a Mesa routine and not
of the guest. Booting the same image by hand with `-display none -device virtio-gpu-pci` answered
`QEMU_EXIT=0` and the same serial cut-off, so the guest itself had stopped.

**What the guest was doing.** Under `-accel tcg -d int,cpu_reset`, the last events were a
`v=0e e=0010` page fault at CPL 0 fetching `ffffffff80084d27` with `CR2` equal to it — an
*instruction fetch of a non-present kernel text page* under `CR3=0000000000364000` — followed by a
double fault and a triple fault. The disk-boot path was dying inside the load of `/sbin/openrc`;
the initramfs path (all ramfs) had never had a problem, and every host test was green.

**The cause: a file read under the task-table lock.** Item 272 (`§6.154`) made `execve` read the
image page by page through `vfs::InodeSource`. A page of a file on a **block device** parks the
task while the device answers — `docs/notes/` §10.5's rule — and the space was being built inside
`with_table`, i.e. holding the one lock every other task needs to be scheduled. On a ramfs root
there is no device and nothing parks, which is why the whole test sweep and every initramfs boot
was green; on the ext4 root the first large program (`/sbin/openrc`, 42920 bytes through 4 `PT_LOAD`
segments plus a 670312-byte interpreter) parked with the lock held.

Three boots name it, and they are the counterfactual:

| kernel | root | serial ends at |
|---|---|---|
| before the fix (item 272 as landed) | ext4 | `MPOS_SWITCH_ROOT_OK`, then the triple fault (`build/scratch/qemu-int2.log`) |
| item 272's image read **whole** in `handle_execve` (streaming loader kept) | ext4 | `mpos login:` (`build/scratch/run-cf-whole.log`) |
| after this fix (streaming loader kept) | ext4 | `mpos login:`, after openrc's services report `[ ok ]` (`build/scratch/run-fix1.log`) |

The middle row is the discriminating one: the same streaming loader with the image read *outside*
the lock reaches a prompt, so the loader's page reads were correct and only their position was
wrong.

**The fix, two halves.** `exec_replace` now builds the new `AddressSpace`, loads the image and
writes the argument block **before** `with_table`, and only then takes the lock to end the group's
siblings, drop the old space and install the new one. The second half is Linux's ordering and was
already wrong here: an `execve` that fails must leave the caller running the image it had. Building
first means every refusal returns with the old space, the old page tables and the old frames
untouched; the old structure dropped the address space before `load_user` could answer, so a
refused load left the task aimed at freed tables. The measurement sits in the same place as the
first half's: the lock's own doc and `exec_replace`'s new comment carry the three boots above.

**Gate.** `build/gate-execorder2.log`, `GATE_EXIT=0`: stage 1 `1320 tests, 0 failed`; x86_64
`pass=314 fail=0`; aarch64 `pass=313 fail=0 (313 built)`, `all 313 user tests passed`; `mpos-kunit 35
passed, 0 failed` per arm; the console receive case reports `the injected line read back through the
tty` on the aarch64 lanes. The first sweep (`build/gate-execorder.log`) was red on that same pair of
aarch64 boot cases - `arch.console_receive`'s `gave Some(13) after 8 foreign byte(s)` and
`ipc.tty`'s line with a leading `Z` - and neither was this change's: the console check bounded
somebody else's bytes at eight, a loaded lane queues more, and the leftover then became the next
case's input. `c4a6443` puts the bound on the wait instead, drains after, and `§15.102` records both
readings.

**What this does not do, named.** The self-test sweep cannot reproduce this defect: its x86_64
root is the boot archive (ramfs, no park) and the aarch64 lane's disk reads did not park, so both
arms were green while the shipped image could not boot. The pin is therefore the boot measurement
above, not a `t-*`: a claim about "no file read while the table lock is held" is a statement about
the kernel's own call graph, and the honest instrument for it is the boot on a real root.

## §6.156 three four-byte words: the eight-byte store in `getresuid`, and the chromium crash it caused (item 274, LANDED 2026-11-14)

**The report.** `chromium` on the disk image got past the loader and then died: with strace, the
main thread's last two syscalls were `getresuid(...) = 0` and `getresgid(...) = 0`, and the next
line was `+++ killed by SIGSEGV (core dumped) +++`. The kernel's own trap report named the
instruction: `#GP code 0x1a`, `bytes at rip: [cc, 0f, 0b, cc, 0f, 0b, ...]` - `int3; ud2` repeated,
which is musl's `a_crash()` padding, the instruction its allocator runs when it has decided its own
metadata is corrupt (`arch/src/arch/x86_64/idt.rs`'s report already knew the shape: "`hlt` is the
case that made this visible: it is musl's `a_crash()`"). A crashpad child had earlier printed
`Check failed: setsid: Operation not permitted (1)`.

**What the kernel was doing wrong.** `getresuid`/`getresgid` wrote each id with an **eight-byte**
store (`(ptr as *mut u64).write_unaligned(word as u64)`) and validated eight bytes first. Linux
writes one `uid_t`/`gid_t` - four bytes - per pointer. glibc's `getresuid(&r, &e, &s)` declares
three four-byte locals, so every call overwrote the four bytes immediately above each one: the
callers' saved words and pointers. musl's allocator found the wreckage a few instructions later and
took the machine's only remaining exit, `a_crash()`.

**Measured on the host first** (`build/scratch/resid/k.c`, Linux 7.2.14): a canary word placed
immediately after each slot comes back `0x11111111, 0x22222222, 0x33333333` unchanged for `getresuid`
and the same shape for `getresgid`; and three slots that each end a page - the next page unmapped -
are written without complaint. Both are statements only a four-byte store satisfies.

**The fix.** `identity_words` (`src/syscall/linux/posix.rs`) validates four bytes per pointer and
stores `u32`. No behaviour changes for a caller that passed three real `uid_t`s - which is why
nothing in the suite had ever seen it: `t-posix` read the ids back out of one `[u32; 3]`, where the
two extra bytes landed inside the array that was about to be overwritten anyway.

**The pin.** `t-posix` gained six claims: `getresuid`/`getresgid` still succeed against three
*separate* `#[repr(C)]` slots, and each slot's canary word is untouched; and a `u32` slot that ends
a page (its next page unmapped) is written rather than refused. Counterfactual, with the eight-byte
store restored (`build/scratch/oneshot-resid-cf.log`): exactly those three red -
`getresuid writes four bytes per slot, leaving the next word alone`,
`getresgid writes four bytes per slot, leaving the next word alone`,
`a four-byte slot that ends a page is written, not EFAULT` - and the fixed kernel is green
(`build/scratch/oneshot-resid.log`). Host `MPOS_ON_HOST=1` green too: the claims are Linux's answer.

**`setsid` is not a bug here.** Measured both ways (`build/scratch/setsid/b.c`, and the same program
run in the guest): a plain child (not a group leader) gets `setsid` = its own pid and a second call
`EPERM`; a child that `setpgid(0,0)`s first gets `EPERM`; a group leader gets `EPERM`. This kernel
answers exactly that. The crashpad line in the user's log did not come back after the fix - in the
next run chromium's own output held only the sandbox complaint (`Running as root without
--no-sandbox is not supported`), so the `EPERM` was another face of the smashed stack, not a refusal.

**The gate.** `build/gate-resid2.log`, exit 0: stage 1 `1320 host unit tests, 0 failed` (30.59 s);
x86_64 `pass=314 fail=0` with `mpos-kunit 35 passed, 0 failed`; aarch64 `pass=313 fail=0` with the
same 35/0; lint ratcheted, both workspaces rustfmt's shape, the fuzz workspace's 23 targets built;
`t-posix` 61 ms on x86_64 and 218 ms on aarch64. The sweep before it (`build/gate-resid.log`) ran
every user test green and then failed on `the kernel printed no mpos-kunit line` - because the
loader still carried the production kernel this round had hand-embedded (its payload cached in
`target/x86_64-unknown-uefi/release/build/mpos-boot` while the kernel's mtime looked unchanged, the
AGENTS.md §5 trap); clearing that directory made the harness appear and the gate green.

**What this does not do, named.** Chromium still does not *run* in the guest; with
`--no-sandbox --headless=new` it now gets much further - it starts its threads, fails to reach dbus
(absent from the image, and logged as an error by chromium itself), cannot read
`/proc/sys/fs/inotify/max_user_watches` (that node did not exist yet then - item 275 built it,
`docs/notes/` §3.30), and then
dies with a `#GP` executing the same `cc 0f 0b` padding, i.e. chromium's own `CHECK`/`IMMEDIATE_CRASH`
path, which is what an unconfigured headless browser does on a bare system. Those are the next
round's questions, not this one's: this round's claim is the width of one store.

## §6.157 the priority-inheritance futex trio, and the `ENOSYS` that aborted chromium (item 276, LANDED 2026-11-14)

Chromium aborted in the guest before it drew anything, and the abort was pulseaudio's:

```
Assertion 'r == 0 || r == 95' failed at ../src/pulsecore/mutex-posix.c:57, function pa_mutex_new(). Aborting.
```

`pa_mutex_new` creates its mutex with `PTHREAD_PRIO_INHERIT` and accepts exactly two answers from
`pthread_mutex_init`: `0`, or `ENOTSUP` (95, "this libc cannot do PI mutexes"). It got a third, so
something under musl was answering with a number pulseaudio had no case for.

### The measurement that named it

A probe replicating pulseaudio's attribute sequence, run on the host and then in the guest
(`build/scratch/pifutex/`, the guest's copy staged into the image's `/root` with `debugfs`):

| call | host (Linux 7.2.14) | guest (this kernel) |
|---|---|---|
| `pthread_mutexattr_init` | 0 | 0 |
| `pthread_mutexattr_settype` | 0 | 0 |
| `pthread_mutexattr_setrobust` | 0 | 0 |
| `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` | 0 | **38** |
| `pthread_mutexattr_setpshared` | 0 | 0 |
| `pthread_mutex_init` | 0 | 0 |

38 is `ENOSYS`. `strace` in the guest says where it comes from:

```
mpos: futex op 0x6 -> ENOSYS
futex(0x3fbc6a54, FUTEX_LOCK_PI, NULL) = -1 ENOSYS
```

musl's `pthread_mutexattr_setprotocol` **probes** the kernel's PI support with `FUTEX_LOCK_PI` on a
scratch word and returns whatever that says; Linux answers 0, this kernel answered `ENOSYS`, so
musl reported "the program asked for something this libc cannot do" and pulseaudio aborted. The
abort is `__builtin_trap` (`0f 0b`), which the machine's trap report called a general-protection
fault: the "jump into inter-function padding" that the round before this one chased through the
page tables was a `#UD` wearing the wrong name (`src/kernel_services.rs` hardcoded the string;
fixed the same day, §6.158).

### Linux's answers, measured first

`build/scratch/pifutex/probe.c` (`gcc -O1`, 7.2.14):

| call | answer |
|---|---|
| `FUTEX_LOCK_PI` on a free word | `0`, word = taker's tid (`0x00017a1c`) |
| `FUTEX_LOCK_PI` again, same thread | `EDEADLK(35)` |
| `FUTEX_TRYLOCK_PI` by the owner | `EDEADLK(35)` |
| `FUTEX_UNLOCK_PI`, no waiters | `0`, word `0` |
| `FUTEX_UNLOCK_PI` by a non-owner | `EPERM(1)` |
| `FUTEX_TRYLOCK_PI` on a free word | `0`, word = tid |
| misaligned address (`+1`) | `EINVAL(22)` |
| another process's `TRYLOCK_PI` while owned | `EAGAIN(11)`, and the kernel sets `FUTEX_WAITERS` (word `0x80017a1c`) |
| a word whose owner tid names no registered owner | `ESRCH(3)` |
| `LOCK_PI \| FUTEX_CLOCK_REALTIME` | `ENOSYS(38)` |

The word's layout is the same on every architecture: owner tid in bits 0..29, `FUTEX_WAITERS` at
bit 31, `FUTEX_OWNER_DIED` at bit 30.

### What landed

- `crates/err` grew `EDEADLK` (-35) and `EOWNERDEAD` (-130); the number table in
  `src/syscall/linux/nr_{x86_64,aarch64}.rs` grew `FUTEX_LOCK_PI = 6`, `FUTEX_UNLOCK_PI = 7`,
  `FUTEX_TRYLOCK_PI = 8`.
- `src/sched/futex.rs` gained the PI section: the word helpers (`pi_owner`, `pi_word`,
  `pi_tid_of`), two registries — `PI_WAITERS` (who is parked on which word) and `PI_HELD` (which
  words each task holds, the exit path's ledger, because a dying task's memory cannot be read) —
  and `lock_pi`/`trylock_pi`/`unlock_pi`. The claim of a free word is decided under a critical
  section that re-reads the word, so two takers cannot both see it free; a contended waiter is in
  `PI_WAITERS` before the word says so; a handover sets the word to the next waiter's tid and wakes
  it; `UNLOCK_PI` with no waiter writes `0`.
- `release_pi` runs in `Table::release_exited`'s **pre-pass**, before the space teardown: it takes
  the dying task's words out of `PI_HELD`, grants each to its first waiter with `FUTEX_OWNER_DIED`
  set (or writes the bit when nobody waits), and the words land through `write_pi_word` — the
  populate-and-write shape `write_clear_tid` already used.
- **The number in the word is resolved through the pid namespace**, the same way `gettid` resolves
  it (`pi_tid_of`). Linux puts the *global* tid there; a machine whose programs only ever see
  namespaced numbers would be handing them a number they cannot compute, so the word carries the
  number the reader's namespace gives the owner. With one namespace the two are the same figure.
- The `handle_futex` door refuses `FUTEX_CLOCK_REALTIME` with `ENOSYS` and a misaligned address
  with `EINVAL` **before** the three operations, which is Linux's order for the PI forms.

### Pins

- Host unit tests in `src/sched/futex.rs`: the word's layout, the handover's grant order and the
  waiters bit behind it, one place per task and address.
- `user/tests/src/bin/t-pi-futex.rs` (22 claims): every row of the measured table above, the
  contended `EAGAIN`-with-`FUTEX_WAITERS`, a real handover across `fork` (the child blocks in
  `LOCK_PI`, the parent unlocks, the child is handed the word and answers 0 with the word naming
  it), the child's release on exit, and the two calls the report came from —
  `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` answering 0 and a PI mutex locking,
  unlocking and destroying.
- `MPOS_ON_HOST=1` (Linux 7.2.14): 22/22 `ok:`. Guest: 22/22 `ok:`.

### The counterfactual

With the three dispatch arms made to answer `ENOSYS` (the state before this item) and everything
else untouched, the guest reddens **17** claims — every PI claim including
`pthread_mutexattr_setprotocol(PRIO_INHERIT) answers 0` — and ends `FAILED`
(`build/scratch/oneshot-pifutex-cf.log`); the restored kernel answers 22/22 again
(`build/scratch/oneshot-pifutex3.log`).

### What this does not do, named

1. **No priority inheritance.** The word's states are Linux's; *acting* on them — raising the
   owner's scheduling priority to its waiter's while it holds the word — is not implemented,
   because this kernel's ready queues are kernel-internal bands chosen by the scheduler, and user
   `nice` does not order them. A program gets Linux's handshake and Linux's answers; what it does
   not get is the latency guarantee the attributes exist for.
2. `FUTEX_WAIT_REQUEUE_PI` (13) and `FUTEX_CMP_REQUEUE_PI` (12) still answer `ENOSYS` — condvar
   waiters on a PI mutex are a separate item, and the `probe2.c` case that would have measured
   their handover blocks in this kernel before it can be compared.
3. The owner-died handover is implemented and unit-tested on the host side, but not measured
   against Linux end to end: the pin's child unlocks before it exits, so the `FUTEX_OWNER_DIED`
   path has no user-visible reading yet.
4. The `#GP` the abort produced used to be reported under the wrong name — a user `#UD`
   (`__builtin_trap`) arrives at `user_fault_or_die`, which printed the general-protection string
   for every vector. **Fixed the same day**: §6.158.

## §6.158 one trap door, six exceptions, one word: the `#UD` chromium never took (item 278, LANDED 2026-11-14)

### The observation

A chromium crash for the second time in two days, and the log said the same thing both times:

```
ThreadPoolForeg[730]: segfault at 7ed7213230ce ip 00007ed7213230ce ... error 14 ...
traps: chromium[737] general protection fault ip:7dde2df7194e sp:3ffc... error:0
  #GP code 0x1a rip 0x...126 rsp ... rflags ...
  bytes at rip: [cc, 0f, 0b, cc, 0f, 0b, cc, cc, cc, cc, 55, 48, 89, e5, 41, 56]
```

Those bytes are the answer to the question the line claims to answer. `0f 0b` is `ud2`, and in musl
that is `a_crash()` — the instruction the allocator runs once it has found the heap corrupted
(item 274's bug, already fixed and no longer the cause of this run). Everything around it is `cc`
(`int3`) padding. The process did not attempt something the CPU refused as a *protection* fault: it
executed an **invalid opcode**, which is vector 6, not vector 13.

The name in that line came from `user_fault_or_die` in `arch/src/arch/x86_64/idt.rs`, one function
that six IDT entries share:

```rust
extern "x86-interrupt" fn divide_error(frame)         { if user_fault_or_die(&frame, 0) {…} }
extern "x86-interrupt" fn invalid_opcode(frame)       { if user_fault_or_die(&frame, 0) {…} }
extern "x86-interrupt" fn segment_not_present(frame, code) { … }
extern "x86-interrupt" fn stack_segment(frame, code)  { … }
extern "x86-interrupt" fn alignment_check(frame, code){ … }
extern "x86-interrupt" fn general_protection(frame, code) { … }
```

and it reported through `hal::kernel::report_user_trap(ip, sp, error)`, which formatted the literal
`general protection fault` into every line, and printed `#GP code …` above it. The seam had no room
for the one fact that distinguishes the six: the vector the machine dispatched through.

### Why this is a bug and not cosmetics

A trap frame preserves three facts — the interrupted `rip`, `rsp`, the error code — and the error
code is usually zero for all six. The vector is the *only* thing that says which exception
happened. Naming it wrongly is worse than not naming it: this repository's own reading of that log
spent two rounds on hypotheses about a protection fault (a non-canonical operand, an SSE access the
FPU state refused) that the machine never raised, and the actual instruction was in the bytes the
same report printed.

`docs/notes/` §6.73 records the shape this line *should* have — a `traps:` line carries no
`at <address>` because the machine did not name one — and that remains right. What was wrong is the
word.

### What was measured, and what could not be

- The `#UD` was produced deliberately: `build/scratch/trapname/u.c` is `__asm__ volatile("ud2")`,
  exits 132 (`Illegal instruction (core dumped)`) on Linux 7.2.14.
- **Linux's own wording per vector could not be measured here**: `dmesg` answers
  `读取内核缓冲区失败: 不允许的操作` (no `CAP_SYSLOG` in this container), and `printk`'s trap
  messages are not in any userspace-visible file. So this kernel does **not** claim Linux's words.
  It names the vector in its own vocabulary, and says so in the code
  (`src/kernel_services.rs::trap_name`).

### The change

1. `hal::kernel::Kernel::report_user_trap` takes the vector:
   `fn report_user_trap(&self, vector: u8, ip: usize, sp: usize, error: usize)` (with the forwarder
   in `arch/src/hal/kernel.rs`). The seam carries the *number*, not the word, so the machine layer
   keeps no second copy of the table.
2. Each of the six IDT entries passes its own vector (0, 6, 11, 12, 13, 17); the first-fault line
   above the `traps:` line reads `trap {vector} code …` instead of `#GP code …`, and the
   repeat-death line names the vector too. The one-shot flag `GP_REPORTED` became `TRAP_REPORTED`,
   which is what it always was (one full report per boot, whichever vector).
3. `src/kernel_services.rs::trap_name(vector) -> &'static str` is the table, total (`_ => "trap"`,
   because the fault path must not be able to fail on its own diagnostic): 0 divide error, 1 debug,
   3 breakpoint, 4 overflow, 5 bounds, 6 invalid opcode, 7 device not available, 8 double fault,
   10 invalid TSS, 11 segment not present, 12 stack-segment fault, 13 general protection fault,
   14 page fault, 16 x87 floating-point exception, 17 alignment check, 18 machine check,
   19 SIMD floating-point exception, 20 virtualization exception, 21 control protection exception.
   The report is then `traps: {comm}[{tid}] {name} ip:… sp:… error:…`.

The six-vector door is intentional and stays: they are the traps with no faulting address, and one
reporter is what keeps their bodies identical. What changed is that the reporter now asks which one
it is serving.

### The pins

The reporter itself cannot be driven from a boot self-test — a fault taken from a suite would end
the boot it is reporting on — so the pin is the table, which is the whole of the change:

- `src/kernel_services/kunit.rs`, suite `kernel_services` at `Phase::Arch`, three cases:
  - `an_invalid_opcode_is_not_a_protection_fault` — vector 6 is `invalid opcode`, vector 13 is
    `general protection fault`, and the two are not the same word. This is the bug, in one case.
  - `the_vectors_one_door_serves_each_answer_their_own_word` — the seven vectors the trap door and
    the page-fault path dispatch through (`#DE`, `#UD`, `#NP`, `#SS`, `#GP`, `#PF`, `#AC`) each
    answer a word, pairwise distinct, and none of them the `"trap"` fallback.
  - `a_vector_the_idt_never_installs_is_still_named` — six uninstalled vectors answer a non-empty
    word (the total function's contract).
- Guest, selftest kernel booted on its own (`.workbuddy/one-test-boot.sh`, serial log
  `build/scratch/boot-kunit.log`): all three cases `OK`; the boot's total is `32 passed, 6 failed`
  because that harness is not the sweep's wiring (no display, hence the display suite's six
  cases — the same 6 before and after this change).

### The counterfactual

With the old behaviour restored in the table — vector 6 answering `general protection fault`, i.e.
the one-word reporter — the same boot reads:

```
mpos-kunit: kernel_services.an_invalid_opcode_is_not_a_protection_fault FAILED: vector 6 is
  `general protection fault`, not the invalid opcode `#UD` is
mpos-kunit: kernel_services.the_vectors_one_door_serves_each_answer_their_own_word FAILED:
  vectors 6 and 13 are both `general protection fault`
mpos-kunit: kernel_services.a_vector_the_idt_never_installs_is_still_named OK
mpos-kunit: 30 passed, 8 failed
```

`build/scratch/boot-kunit-cf.log`; the third case stays green because the fallback is orthogonal to
the bug, which is why it is written as its own claim.

### The field reading

The same guest kernel, booted on the disk image, runs chromium again (`--no-sandbox --headless=new
--disable-gpu --dump-dom about:blank`, still `rc=191`) and its three traps are all vector 13, so the
line is unchanged **and correct**:

```
  trap 13 code 0x1a rip 0x7d9af876f126 rsp 0x3f966d40 rflags 0x10246
traps: chromium[736] general protection fault ip:7d9af876f126 sp:3f966d40 error:1a
```

and a `ud2` executed deliberately inside the guest by a program dropped on the 9p share
(`build/scratch/uaddr.c`, musl, `fork` + `ud2` in the child) is reported as the trap it is:

```
traps: uaddr[764] invalid opcode ip:4011a9 sp:3fe9ccb0 error:0
```

(the `trap 6 code …` line above it does not appear because the one-shot full report had already been
printed for chromium's earlier fault — one flag per boot, whichever vector). Under the old table
that second line read `general protection fault`, which is exactly the sentence that sent this
repository after the wrong fault twice.

### What this does not do, named

1. **The words are not Linux's.** `dmesg` is unreadable here (above), so reproducing `traps:`
   wording per vector would be recalled, not measured. A program cannot see this line either way —
   it is a diagnostic, not ABI — and the doc says which vocabulary it is in.
2. **`#PF` is named but does not come through this door**: `page_fault` has its own handler and its
   own `segfault at …` line; the vector appears in `trap_name`'s table because it is the same
   numbering, and a future caller may use it.
3. **The aarch64 machine has no equivalent line**: its traps are `ESR_EL1` exceptions whose class
   names the reason directly, and it has no six-vector shared door, so nothing changed there.

## §6.159 one signal for six traps became six signals: a synchronous x86_64 exception ends the process with its vector's own signal (item 279, LANDED 2026-11-14)

### The observation

Item 278 fixed the *name* in the `traps:` line and left the *verdict* alone. That was the next
wrong shape this kernel shipped: `user_fault_or_die` in `arch/src/arch/x86_64/idt.rs` is one door
for `#DE` (0), `#UD` (6), `#NP` (11), `#SS` (12), `#GP` (13) and `#AC` (17), and its body sent
**`SIGSEGV` (11) for every one of them**. A parent that waited for a child after a division by zero
was told the child had made a bad memory reference. A program that installed `SIGILL` and then ran
`ud2` died of a signal it had not been sent. Exit status is ABI: `WTERMSIG` is what shells,
 supervisors and crash handlers read.

The seventh vector was worse. `#BP` (3) — the `int3` instruction, the `cc` padding around musl's
`a_crash()` and chromium's `__builtin_trap()` — had its own handler but **a DPL-0 IDT gate**. The
x86_64 crate's `set_handler_fn` installs a Ring-0 gate by default; software `int` from CPL 3 checks
`CPL <= DPL`, so a plain breakpoint never reached the handler at all: the CPU raised `#GP` with
error `0x1a`, whose IDT index (`0x1a >> 3 = 3`) literally names the vector it could not deliver.
The kernel then reported a protection fault and killed the process with `SIGSEGV`. Measured in the
guest (`build/scratch/int3/i.c`, static musl on the 9p share, `build/scratch/xt17.log`):
`traps: i-musl[839] general protection fault ip:401277 sp:3fff0c20 error:1a` → `INT3_RC=139`.
Setting the gate to Ring 3 alone (`build/scratch/xt18.log`) removed the `#GP` but left `int3`
with **no signal at all** — the `breakpoint` handler only bumped a counter and printed, so the
program carried on to its next instruction. Nearly-right twice; neither could ship.

### The measurement, on the host first

Linux 7.2.14, one instruction in a forked child per trial, read back through `wait4`'s
`WTERMSIG` (`build/scratch/trapsig/w.c`, `g.c`, `ac.c`, `s.c`):

| instruction | vector | `WTERMSIG` | with a handler, `si_code` |
|---|---|---|---|
| `int3` | `#BP` 3 | **5** `SIGTRAP` | 128 `SI_KERNEL` |
| `ud2` | `#UD` 6 | **4** `SIGILL` | 2 `ILL_ILLOPN` |
| `idiv` by zero | `#DE` 0 | **8** `SIGFPE` | 1 `FPE_INTDIV` |
| unaligned dword load with `EFLAGS.AC` set | `#AC` 17 | **7** `SIGBUS` | — |
| `hlt` at CPL 3, `mov cr0` | `#GP` 13 | **11** `SIGSEGV` | — |
| null store | `#PF` 14 | **11** `SIGSEGV` | 1 `SEGV_MAPERR` |

The `#AC` row needed a second measurement: a ring-3 program can set `EFLAGS.AC` with `pushfq;
or [rsp], 0x40000; popfq`, but Linux only lets it bite with `CR0.AM` set. This kernel's
`enable_user_sse()` set `CR0.EM`/`MP`/`TS` for x87/SSE and left `AM` clear — measured in the
guest, the flag was set (`EFLAGS.AC after popfq = 262726 (bit18=1)`) and the unaligned load
completed. QEMU's **TCG does not emulate alignment checking at all**: with `CR0.AM` set the load
still completes under TCG, while under KVM the same binary raises `#AC`. Linux's row is
unambiguous; the machine's ability to raise it is not.

### The change

1. `src/sched/signal.rs` owns `signal_for_trap(vector) -> u8`, a host-compiled table beside the
   signal constants (the arch crate has no `mpos-abi` dependency, so the map sits in the
   vocabulary module and is reached through the same seam item 278 used):
   `0 => SIGFPE`, `3 => SIGTRAP`, `6 => SIGILL`, `17 => SIGBUS`, `_ => SIGSEGV`. Vector 14 is
   deliberately absent: `#PF` has its own handler, whose resolvers decide whether the address
   was the process's before any signal is raised. `SIGILL`/`SIGBUS`/`SIGFPE` are now named
   constants in that module and pinned in `numbers_are_pinned`.
2. `arch/src/hal/kernel.rs` gains `Kernel::signal_for_trap(vector)` and its forwarder;
   `src/kernel_services.rs` answers it from the table. `user_fault_or_die` now asks
   `let sig = hal::kernel::signal_for_trap(vector)` and uses that same `sig` for `send_signal`,
   `disposition_is_default`, both `die_of_fatal_signal` calls and the repeat-death line. One
   verdict, one variable — a second hard-coded `11` anywhere would be a new bug.
3. The `#BP` gate is installed with `.set_privilege_level(PrivilegeLevel::Ring3)`. Its handler
   routes a user `rip` through `user_fault_or_die(&frame, 3, 0)`, while ring 0 keeps the
   counter and the print — the boot's own `int3` self-test (`src/board/suites.rs`) asserts the
   counter still moves, and must not be killed.
4. `enable_user_sse()` sets `CR0.AM` too, with the measurement above in the doc comment.
5. `#PF` is unchanged: `page_fault` still sends `SIGSEGV` on its own path.

The stale paragraph above `user_fault_or_die` that claimed a signal queued here can only be
delivered at a syscall boundary was rewritten. §2.13 *did* build a trap-time delivery door
(`deliver_at_trap`), but it is wired only to the hand-written PIT `timer_stub`, the one entry
that preserves all GPRs; the six `x86-interrupt` entries have no such frame. **A handler for one
of these signals is still not entered at the trap** — the signal is queued, a default disposition
dies with the right number, and a task with a handler is ended after `FAULT_REPEAT_LIMIT` repeats
of the same instruction. That is a named gap, not a design presented as complete.

### The pins

- `src/sched/signal.rs::a_trap_vector_ends_the_process_with_its_own_signal` (host unit test) pins
  all eight rows of the map and asserts `#DE` and `#PF` are not the same answer.
  `numbers_are_pinned` asserts the three new constants.
- `user/tests/src/bin/t-trap-signal.rs` (x86_64 claims only; in `[aarch64] gaps`, see below):
  `int3` → `SIGTRAP`, `ud2` → `SIGILL`, `idiv` by zero → `SIGFPE`, `hlt` → `SIGSEGV`, null store
  → `SIGSEGV`, unaligned load → `SIGBUS`. `hlt` and the null store are the controls: a map that
  changed *every* vector's answer, or that routed `#PF` through the trap door, reddens them.
  Every fault runs in a forked child and is read through `wait4` (`libc`'s raw call is the
  crate's clause (b): no wrapper names the wait-status word). Host `MPOS_ON_HOST=1` → PASS;
  guest `build/scratch/oneshot-trapsignal.log` → PASS, with `traps: … breakpoint / invalid
  opcode / divide error / general protection fault / alignment check` each naming its vector.
  The `#AC` claim is accelerator-tolerant: under TCG the program prints a `note:` that the
  machine cannot raise `#AC` instead of pretending the claim was tested, and the host unit test
  pins that row unconditionally. The gate's x86_64 arm runs under KVM here, so the row is read.

### The counterfactual

The five changed kernel files were saved to `build/scratch/*.trapfix`, the map replaced with
`match vector { _ => SIGSEGV }`, the breakpoint gate put back to DPL 0, and the kernel + loader
rebuilt (payload cache cleared). Guest one-shot `build/scratch/oneshot-trapsignal-cf.log`:
**FAILED**, five red claims — `int3 ends the child with SIGTRAP`, `ud2 ends the child with
SIGILL`, `idiv by zero (#DE): the child died of a signal`, `an integer divide by zero ends the
child with SIGFPE`, `an alignment-check exception ends the child with SIGBUS`. The two
`SIGSEGV` controls stayed green, which is the point of writing them. All five files restored
byte-identically (sha256), rebuilt, guest PASS again
(`build/scratch/oneshot-trapsignal-restore.log`).

### Gate

`build/gate-trapsignal-final.log`, exit 0: stage 1 **1327** host tests, 0 failed (30.27 s); x86_64
`verdict pass=318 fail=0`; aarch64 `verdict pass=316 fail=0` (`t-trap-signal` built there and
correctly left out as a named gap); `mpos-kunit 38 passed, 0 failed` on each arm; lint ratcheted
(`162/162`, `168/168`, `389/389`, `273/273`, `3/3`, `1/1`), both workspaces formatted, fuzz 23
targets. `t-trap-signal` is `=== PASS` in `build/test-lane-x86_64-1/serial.log` of the final run.
The first run (`build/gate-trapsignal.log`, 57.46 s) was green too and taught one small thing: an
unguarded `use mpos_user_tests::Report;` is an unused import on the aarch64 build, so the import is
`#[cfg(target_arch = "x86_64")]`; the final gate is the clean one.

### What this does not do, named

1. **A handler is still not entered at the trap.** The vector's signal is queued and the death
   carries its number; §2.13's `deliver_at_trap` is only on the PIT stub. A process that installs
   `SIGILL`/`SIGFPE`/`SIGTRAP` and then faults still dies after three repeats. Trap-time handler
   delivery for these six vectors needs a GPR-preserving stub per vector (with and without an
   error code on the stack), the same shape §2.13 built for the timer.
2. **aarch64 has no per-class signal map.** Its `el0_sync` decodes `ESR_EL1`, and every refused
   EL0 fault ends in `die_of_fatal_signal(11)` with no disposition check, no queued signal and
   no `brk`/undefined-instruction class decoding; only `EC_SYS64` has a distinct `SIGILL` death.
   `t-trap-signal` therefore joins `[aarch64] gaps` (it builds there and says "skipped"). The
   work is: decode the `ESR` classes (`BRK`, undef, FP, data/instruction abort `DFSC`), queue the
   class's signal through `send_signal`, and call the *existing* `deliver_at_trap` on the EL0
   sync frame, which already has the full `UserFrame`.
3. **`siginfo_t` still carries no fault address.** `SigInfo` and `SignalInfo` have no `addr`
   field; `send_signal` produces `SigInfo::user` (`SI_USER`), and a guest `SA_SIGINFO` handler
   for a null store read `si_code=0` and a garbage `si_addr` where Linux answers
   `SEGV_MAPERR` and the fault address. `si_code` per trap (`ILL_ILLOPN`, `FPE_INTDIV`,
   `TRAP_BRKPT`, `BUS_ADRERR`) is likewise absent. Changing the signal *number* was the
   question this item answered; the payload is its own measured item.
4. **Chromium is not unblocked by this.** The `cc 0f 0b` bytes at its crash site are an
   intentional `__builtin_trap()` after an earlier logical failure; it is *trying* to die. Its
   remaining causes are the dbus-less image and the parked-fault race (`libcrc32c`/`libdbus-1`
   instruction-fetch `#PF` on a table-present page, §6.153's open question).
