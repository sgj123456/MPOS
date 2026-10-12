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

*Both remaining resources are now enforced:*
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
| `CLONE_NEWUSER` | uid/gid mapping, and the capability set *within* the namespace | **refused** until there is a mapping to store |
| `CLONE_NEWIPC` | SysV IPC objects and POSIX message queues | **refused**: this kernel has neither, so it isolates nothing |
| `CLONE_NEWCGROUP` | the cgroup tree a process sees | **refused**: there is no cgroup tree here to see |
| `CLONE_NEWTIME` | `CLOCK_MONOTONIC`/`BOOTTIME` offsets per namespace | **refused**: one tick clock, and no per-namespace offset to store |

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
descriptor opened on `/proc/<pid>/ns/<kind>`; no namespace here can be named
that way, so the call answers `EINVAL` for an open descriptor and `EBADF` for a
closed number - all it can learn. The design follows from the table: each
namespace object becomes an inode whose open descriptor *is* the namespace (so
two descriptors on one `/proc/<pid>/ns/uts` name one object, with one `st_ino`),
`setns` checks the descriptor's type against `nstype` (a mismatch is `EINVAL`,
Linux's answer), and `NS_GET_USERNS`/`NS_GET_PARENT` are out of scope until
there is a user namespace for them to point at.

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
creation is the whole mechanism (what stays parked with a shared-group handle is CLONE_FS-style
*separation*, where siblings must split). `clone(CLONE_NEWUTS)` came with it, same shape, Linux's
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
  kind that is still refused whole — `CLONE_NEWTIME` in `t-posix`, `CLONE_NEWPID` *once the capability
  is back* in `t-caps` — and each pins the served answer beside it. `t-ns-uts` had the same shape one
  level down: its mixed word was `NEWUTS | NEWNS`, which became a legal pair, so the refusal moved to
  `NEWUTS | NEWNET` — what that file's own header had described all along ("the served namespace with
  an unserved one").
* **`/proc/mounts` and `/proc/self/mountinfo` still rendered the machine's table.** The step moved the
  table onto the task and routed the *resolution* free functions through `process_fs_context`, but the
  two renderers kept `mount::global()` — so every task read one list and `t-ns-mnt`'s "a child's umount
  peels its own copy" looked at the machine's mounts from inside a copy, where the peel cannot be seen.
  They call the caller-routed `mount::mount_records()` now. A sibling of the same oversight is **named
  rather than fixed**: `linkat`'s descriptor-side `EXDEV` check still asks
  `mount::global().fs_serving_inode` (`src/syscall/linux/fs/file.rs`), so a task with its own mounts can
  be refused a link Linux would have made — an exotic path that wants its own pin before its own fix,
  and it is the remaining half of "every free function routes through the process context".
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
already useful and the wire is a separate question. USER, IPC, CGROUP and TIME
stay refused, each with the reason in the table above, and each is worth
revisiting only when the thing it would isolate exists (§8.2's seccomp and §7.4's
compositor work are both more urgent).

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
* **The six thread-group flags still answer `0` for a caller that has siblings.**
  Linux refuses with `EINVAL` there, because the descriptor table or the handler
  table really is shared. This kernel answers `0` for every caller, which is
  Linux's answer only for a single-threaded one - and since threads exist now
  (`t-clone`), the old comment's premise ("a process is its own only thread") is
  no longer universally true. Splitting a resource off from one's siblings is
  what is not modelled.
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
that does have siblings Linux answers `EINVAL` where this kernel still answers
`0`. That is a named gap in the "Step 1, landed" list rather than a claim.

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
two records, Linux's one) — which are exactly the claims the strays broke.

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
And the kinds this kernel has not (pid, net, ipc, user, cgroup, time) have **no
entry** in `/proc/<pid>/ns` - which is the surface a real kernel built without a
namespace has (a machine with `CONFIG_TIME_NS` off answers `ENOENT` for
`/proc/self/ns/time`), so a reader is told two names and nothing it cannot check,
and no descriptor for an absent namespace can exist to be handed to `setns`.

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

### R.6 (moved from ROADMAP's status board) — `/proc/<pid>/maps`: the three sources, and why two of them stay beside the VMA table

The file a crash reporter reads to find a program's base address: every mapping in Linux's six-field format — `start-end perms offset dev inode name`, addresses as lowercase hex, the offset zero-padded, `dev` as `%02x:%02x`, and the name in the fixed column Linux uses (73, measured across address and inode widths on 7.2.7) when there is one — which is what makes the last field addressable whatever the width before it. The perms word's fourth character is the `MAP_SHARED` bit, so it is carried on the mapping rather than guessed from what backs it: a `memfd` mapped `MAP_PRIVATE` aliases the same frames and is still `p`. Three sources feed one file, because the address space is recorded in three places: the VMA table (`mmap`, `mprotect`, `brk`), a list of ranges the *loader* filled, and the program break, which is what gives a range the `[heap]` name — applied by address, since this kernel's heap is an ordinary anonymous mapping. `[stack]` is recorded when the stack is laid out and stretched down as the stack grows, so a program reading its own maps from deep in a call is not told about a stack that stops above it.

**What the work needed, and it is the interesting part:** `execve` maps an image's pages *eagerly* — it fills every page as it loads — so an image leaves **no VMAs at all**, and the same is true of a `MAP_PRIVATE` mapping of an ordinary file. A maps file built from the VMA table alone would therefore omit the one line a crash reporter actually wants: the program's own code. The loader's ranges and the private file mappings are recorded *beside* the table (`mm::vma::MappedRange`) rather than put in it, and the reason is the fault path rather than tidiness: recording them as `VmaKind::Anon` VMAs would make an address that should take a fault — one whose frame a `munmap` or a `MADV_DONTNEED` gave back — silently refill with zeros, which for a program's own code means blank bytes where the file's bytes belong. The cost of that choice is named with it: those ranges are the *load's* record and not the current state, so a `munmap` or `mprotect` over one of them — a dynamic program's `RELRO` pass does the second on every start — is not reflected in the report where a VMA would have been edited. `t-maps` pins the lines, not the mutation, so closing this needs a `forget_ranges`/`reprotect` on the recorded list before it can be pinned.

**Two divergences, pinned as divergences:** an image's and a private file mapping's line reports `dev` `00:00` and inode `0`, because there is no object left to name — and `00:00` is what this kernel's own `stat` reports for every filesystem, so the two files agree rather than disagreeing. And a *shared anonymous* mapping is named `/memfd: (deleted)`, which is Linux's own name for a nameless `memfd` mapping (measured: `memfd_create("", 0)` + `MAP_SHARED` reports exactly that), because that is what the object here is; Linux reaches the same shape through `/dev/zero (deleted)`, which is an artefact of its own implementation. `memfd_create` now *keeps* the name it used to only validate, as `/memfd:<name> (deleted)` — the spelling both of its readers expect, measured on Linux — so a `wl_shm` pool's mapping says which buffer it is instead of leaving the column blank. Not written: `/proc/<pid>/smaps` (per-mapping `Rss`/`VmFlags` and the rest), and `/proc/<pid>/numa_maps`.
