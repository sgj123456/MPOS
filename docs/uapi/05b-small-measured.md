# UAPI §5.2

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

### 5.2 Small, measured, and not yet landed

- **A FIFO is served now, parked `open` and all.** **Landed 2026-10-16** (`docs/uapi/` §5.1 item 26).
  The entry this replaces said `mknodat(S_IFIFO)` was refused
  (`EOPNOTSUPP`) and designed the node a real one needed; the design is built. The name is a
  `S_IFIFO` node (size `0`, `nlink` 1) and its `open` runs Linux's **rendezvous**: a reader and a
  writer meet at the name, a reader with no writer and a writer with no reader both park (a parked
  reader counts as a reader, so the arriving writer releases it), `O_RDWR` is its own peer and does
  not park, `O_RDONLY|O_NONBLOCK` returns at once, `O_WRONLY|O_NONBLOCK` with no reader is
  **`ENXIO`**, the buffer is **discarded** when both counts reach zero, and an opened description
  reports `S_IFIFO` (and `ESPIPE`s a seek). The whole measured contract (`.workbuddy/probe_fifo.c`,
  Linux 7.2.9) is §5.1 item 26's, and `t-fifo` (42 claims) is its pin.

  **The one residual this entry used to carry is closed (2026-10-17, `docs/uapi/` §5.1 item 28).** It
  said a **fatal** signal delivered while a task is parked in a FIFO's `open` ends the task inside the
  wait, so the count that open took is never given back and the FIFO remembers a peer that never
  arrived. It is not the count that has to be owned by something the exit pass drops, as this entry
  first read the problem: the *wait* is what had to change, and Linux's own order is the answer —
  the park ends for a fatal signal like it does for a catchable one, the `open` runs the give-back it
  already had, and the death happens at the syscall gate, exactly where `fifo_open`'s `err_rd:` runs
  on `ERESTARTSYS`. Measured on a current host first (`.workbuddy/probe_fifo_park.c`, Linux 7.2.9): a
  parked reader killed inside the park leaves `open(O_WRONLY|O_NONBLOCK)` at **`ENXIO`**, a parked
  writer's death leaves a reader reading **`0`**, and `t-fifo`'s killed-opener section pins both on
  both machines — so the rendezvous claims no longer need to release their openers to stay honest,
  though they still do, because a released opener is the claim they are about.


- **The device-node capability is asked now, in Linux's place in the order.** **Landed 2026-10-17**
  (`docs/uapi/` §5.1 item 36). The entry this replaces said both `mknod` doors built a device node for
  **any** caller where Linux answers `EPERM` without `CAP_MKNOD`, and that the gate was not one line
  because a `setuid` here did not re-derive the capability sets. That prerequisite landed on its own in
  the meantime (`set_current_identity` runs `Credentials::on_identity_change`, the measured
  `cap_emulate_setxuid` rule `t-caps` pins), so the gate is asked now — of the caller's **effective
  set**, and of exactly one kind of node. Measured whole on a current host first
  (`.workbuddy/probe_mknod_cap.c`, Linux 7.2.9, an unprivileged caller, one scenario per answer):
  `S_IFCHR` and `S_IFBLK` are **`EPERM`**; `S_IFREG`, `S_IFIFO` and `S_IFSOCK` are **served** to a
  caller holding not one capability; the character node whose `dev` is **`0`** is served too — that is
  how overlayfs spells a *whiteout*, and the exception is the character type's alone, since a *block*
  node with the same `0` is `EPERM`; and the question comes **last**, after the type, the name, the
  name's existence (`EEXIST` wins) and the directory's writability (a device node in a directory the
  caller may not write is `EACCES`, not `EPERM`). `abi::mknod_needs_capability` holds the rule and
  `t-mknod`'s capability section pins all of it from both sides — a root caller that lowers its
  effective id for the duration, and an ordinary user, which is what a host run is.

  **Which namespace the capability is held in — landed 2026-10-17** (`docs/uapi/` §5.1 item 38).
  Linux's `vfs_mknod` asks `capable(CAP_MKNOD)`, and `capable` is the **initial** user namespace —
  so a caller that has entered a user namespace, is uid 0 in it and reads the whole set back *still*
  cannot make a device node on a filesystem the parent namespace owns (measured:
  `.workbuddy/probe_mknod_userns.sh`, Linux 7.2.9 — `touch` in the same directory answers `0` while
  `mknod c 1 3` answers `EPERM`, and the `dev 0` whiteout is served there as it is outside). The
  entry this replaces said this kernel's checks ask the caller's own namespace, so that caller was
  served here, and named the gap as the capability model's. It is closed for the four call sites
  whose Linux rule is `capable()` — `mknod`'s `CAP_MKNOD`, `fileattr_set`'s `CAP_LINUX_IMMUTABLE`,
  `reboot`'s `CAP_SYS_BOOT` and `syslog`'s `CAP_SYSLOG` — through `Credentials::capable`
  (`crates/cred`), with the three *other* shapes named below rather than approximated.

- **A device node made by `mknod` carries no device number.** `st_rdev` is the field `mknod(2)`'s
  `dev` argument fills - measured on the host, `/dev/null` reports `rdev 1:3`, the number its own
  creation passed, and an unprivileged caller cannot make a device node to watch it happen
  (`EPERM`, measured) - and this kernel ignores the argument: `handle_mknodat` reads only the mode
  and hands `parent.create(name, kind)` its kind, so a node made here reports `rdev 0:0`. It is
  named rather than fixed because writing the number is not the whole of it: this kernel has no
  table from a device number to a driver (its `/dev/null` *is* the null device, a driver-published
  object; a node made here is a placeholder the `devtmpfs` mount later covers), so a true
  `st_rdev` would make `stat` right and the device still absent - and it would put a fabricated
  node in front of the rdev-keyed ioctl families (`'R'`'s entropy words, `FIONREAD`), which
  dispatch on that word and would then answer for a device that is not there. Closing it means a
  number-to-driver registry, which is the same prerequisite `devfs`'s birth modes want. Owner:
  `src/syscall/linux/fs/file.rs`'s two `mknod` doors. `t-mknod` reads the type of what it made and
  prints the `st_rdev` that came back.

- **A descriptor's second name now denotes the file behind it — and what that leaves.** **Landed
  2026-10-16** (`docs/uapi/` §5.1 item 24). The entry this replaces said a pipe "cannot be named by
  its own descriptor's procfs link here", and read the answer as `ENOENT` from the code; the
  measurement (`.workbuddy/probe_fdlink.c`, Linux 7.2.9, through `syscall(2)`) says the call
  answered **`EINVAL`**, and that the door was acting on the link rather than on the file the name
  denotes. It was never only about a pipe: *every* following door acted on the link, so `stat` of
  `/proc/self/fd/N` returned a success carrying the *link's* metadata where Linux returns the
  file's, a name inside a path was `ENOTDIR`, and `open` cloned the descriptor where Linux opens
  the file fresh. The walk denotes now, so the whole family is Linux's — and four things stay
  named, each measured by the same probe or by the pin's own first run:

  (a) **A *foreign* process's descriptor is not nameable here, and its channel is spliced as
  though it were a path.** Linux names one: measured, a child's `pipe:[…]` entry under
  `/proc/<child>/fd/N` answers `stat` as the pipe (`S_IFIFO`, the pipe's own inode) and `truncate`
  refuses `EINVAL` like any other pipe. This kernel's link lends a descriptor only to the caller's
  own thread group (`ProcLink::as_fd`'s gate), and a link that carries no payload is *spliced by
  its target text* — which is right for the named file such an entry reports (it is reached by the
  path it names) and wrong for a channel, whose report is `pipe:[<ino>]` and not a path at all, so
  the same call is `ENOENT`. Owner: `src/vfs/procfs.rs`'s `ProcLink::as_fd` gate + the walk's
  splice fallback; closing it means deciding what a descriptor *denotes* for a caller that may not
  clone it, which is a permission question this kernel answers by thread group where Linux answers
  it by ptrace access (`proc(5)`; not measured here — every descriptor the probe reached was the
  caller's own child, and the probe ran as root).

  (b) **`st_dev` through a name that denotes another filesystem is the *name's*.** Measured on the
  host, `stat("/proc/self/fd/<file>")` reports the file's device (`st_dev` 52, the file's own
  filesystem) beside the file's inode; here the path is what the device is looked up by, so the
  answer is the name's filesystem's (`t-fd-name`'s guest run printed `2 by path, 3 by the name`,
  whose `3` is the procfs `st_dev`). This is the limitation §5.1 item 5 already owns — an inode
  does not say which filesystem it belongs to, which is why `File` records the device at `open` —
  and the shape is older than this name: the stat doors look the device up by the caller's *path*
  string, so any followed symlink that crosses a mount reports its own directory's filesystem (read
  from the code rather than measured here — it wants a second mount to measure against). Owner: the
  stat doors' `dev_of_path`.

  (c) **`..` after a descriptor's second name steps back along the walk's chain, not from the
  file.** Linux jumps the walk onto the file, so `/proc/self/fd/<a directory>/..` is that file's
  own parent — measured, the same inode as `/tmp/..`. Here the walk's *stack* answers `..`, and the
  stack's last directory is `/proc/self/fd`, so the parent is the name's. Reaching Linux's would
  mean asking where the denoted node lives (`MountTable::parent_of`'s bounded search) on every
  `..` the walk meets, which is the cost the walk's chain exists to avoid. Owner:
  `src/vfs/mount.rs`'s `resolve_walk`.

  (d) **The link's own metadata is Linux's now. Landed 2026-10-17** (`docs/uapi/` §5.1 item 24).
  The entry this replaces said `ProcLink` reported the trait's `mode()` of `0` and its own
  `size: 0`, and the *observable* was the kind's default in both fields — `lstat` answered
  `S_IFLNK | 0777` and size `0`, because `reported_mode` reads a filesystem that keeps no modes as
  its kind's default, so the `0` was never what a caller saw (the counterfactual run reads back
  `got Some(1ff)`). Measured whole on a current host first (`.workbuddy/probe_proclink_mode.c`,
  Linux 7.2.9, `lstat` through `syscall(2)`): the link's mode is the *direction* its descriptor was
  opened in, as two bits — `0500` read-only, `0300` write-only, `0700` both, and `0` for a
  descriptor with neither (`O_PATH`) — its `st_size` is **64** whatever the target's text is
  (`readlink` on the same name answers the *target*: 9 bytes for `/dev/null`), and every other magic
  link (`exe`, `cwd`, `root`, `ns/*`, `/proc/self`) is `0777` with size `0`. The directory those
  links live in has metadata of its own too: `/proc/<pid>/fd` is `S_IFDIR | 0500` rather than the
  kind's `0755`, with a size that is **the number of descriptors it lists** (3 answer 3, 23 answer
  23, and a hole is a number rather than a span — fds 0 and 200 with one more open answer 3).
  `ProcLink::mode`/`metadata` report the link's half, `ProcDir` gained the two fields that let one
  directory answer the other, and the direction is read off the live description
  (`sched::descriptor_access`, the one record of what the open asked for), so the answer follows a
  descriptor reopened or closed between two stats. **One thing stays named** (the entry's other
  residual — the rest of both trees — landed as its own item below): an `O_PATH` descriptor, a flag
  this kernel drops and serves as the open it was asked for, would report the direction the open
  actually got where Linux reports neither bit. Owner: `src/vfs/procfs.rs`'s `ProcLink`/`ProcDir`,
  `src/sched/runtime/procfs.rs`'s `descriptor_access`. Pin: `t-fd-name`.

- **Every other procfs/sysfs node reports its own word. Landed 2026-10-17** (`docs/uapi/` §5.1
  item 37) — the entry (d) above used to name this as its remaining half. Measured whole on a
  current host first (`.workbuddy/probe_proc_node_modes.c`, Linux 7.2.9, `lstat` through
  `syscall(2)`, over every path this kernel's two trees build): a proc directory is `0555`
  (`/proc` itself, `/proc/sys/...`, every `/proc/<pid>`, `/proc/<pid>/task`,
  `/proc/bus/pci/<bus>`); a generated file nothing can write is `0444` (`/proc/version`,
  `/proc/meminfo`, `/proc/<pid>/status`, `/proc/net/tcp`); a file that *can* be written is `0644`
  (the sysctls, `/proc/<pid>/oom_score_adj` and `uid_map`, and the live configuration space
  `/proc/bus/pci/<bus>/<dev>.<func>`); `/proc/<pid>/environ` is `0400` where its neighbour
  `cmdline` is `0444`; `/proc/<pid>/ns` is `0511`; and `/proc/cmdline`'s own `st_size` is the length
  of the line it answers. In `/sys`, the tree's root is `0555` while every directory under it is the
  kind's `0755`, a read-only attribute is `0444` (`dev`, `size`, a function's `vendor`) and the
  attribute that can be written — a device's `uevent`, the door `udevadm trigger` writes — is
  `0644`. **The word follows each node's *shape***, which is Linux's own rule for both trees
  (`proc_create` with no mode means `S_IRUGO`; `DEVICE_ATTR_RO` against `DEVICE_ATTR_RW`), and three
  consequences are named rather than hidden. First, the nodes Linux makes writable and this kernel
  did not — `/proc/sys/kernel/pid_max`, `threads-max`, `pty/max`, `/proc/<pid>/comm` — report the
  read-only word, because the file refuses the write and the word says so. Second, the word is a
  *report* and not a gate: procfs keeps no modes to enforce, so an `0400` node is readable by any
  caller exactly as its `0444` neighbours are — the mode-less filesystem's own limit, unchanged by
  this pass. Third, three names differ in *kind* before the mode can: Linux answers `0777` links for
  `/proc/self`, `/proc/thread-self` and `/proc/mounts`, where this kernel answered a directory (a
  mirror of the process, and a mount table), so their word followed from the shape - and that
  consequence **landed on 2026-10-18** (`docs/uapi/` §5.1 item 46): the shapes are Linux's now, the
  `MirrorDir` node those two names were built from is deleted, and the same program claims the kind,
  the target text, the registered `st_size` and the `DT_LNK` a listing hands out. Owner:
  `src/vfs/procfs.rs`'s node types
  (`ProcFile`/`ProcCtl`/`ProcDir`/`ProcConfig`) and `src/vfs/sysfs.rs`'s
  (`SysDir`/`SysFile`/`SysConfig`/`SysPciEvent`/`SysEventAttr`). Pin: `t-proc-modes`.

- **`getcwd` derives the cwd's name from the directory. Landed 2026-10-17** (`docs/uapi/` §5.1
  item 31). This entry used to read that `getcwd` reported the path the process `chdir`'d by where
  Linux canonicalises it, because this kernel's cwd was the path *string* `chdir` stored; the fix
  makes `adopt_cwd` - the one writer of the cwd - store the name `MountTable::path_of` finds for
  the directory instead of the argument, with the caller's spelling kept as the fallback for a node
  the tree cannot name. Measured whole on a current host first (`.workbuddy/probe_getcwd.c`, Linux
  7.2.9): a symlink at the end and in the middle of the argument answers the target, a descriptor's
  `/proc/self/fd/N` name and a `fchdir` through a symlinked descriptor answer the canonical path,
  and a `..` taken after a link that crosses to another parent climbs from the *target's* parent.
  `t-fd-name` claims the descriptor-name, symlink and cross-parent shapes now. **Two consequences
  stay named**: the search can answer a *different* name for a directory the tree reaches several
  ways, or `None` for a cwd deeper than its visit budget; and a cwd **removed underneath the
  caller** is `ENOENT` on Linux, where this kernel - keeping no parent edge - answers the stale
  name. Owner: `src/syscall/linux/fs/file.rs`'s `chdir`/`fchdir`/`adopt_cwd`.

- **The reported `F_GETFL` word holds the mode and the two flags, where Linux's holds more.**
  **Landed 2026-10-16** (`docs/uapi/` §5.1 item 21) for the half a caller reads to learn a
  descriptor's direction: the access mode the description was made under is in the word - a path
  `open`, a `pipe2` end and an `inotify` queue all answer Linux's mode - and `F_SETFL` no longer
  clears it. **The other half landed 2026-10-18** (`docs/uapi/` §5.1 item 49): what the open was
  *asked* for comes back too, because Linux reports `filp->f_flags` and `F_SETFL` only rewrites the
  `SETFL_MASK` member of it. Measured on a current host through `syscall(2)`
  (`.workbuddy/probe_getfl.c`, Linux 7.2.11): `open(O_RDONLY)` reports `0x8000` and
  `open(O_WRONLY)` `0x8001`, where `0x8000` is `O_LARGEFILE` - the kernel *adds* that bit to every
  path open, machine-dependently (`0o100000` on x86_64, `0o20000` on arm64, where this repository's
  own uapi table deliberately defines neither) - and beside it every flag the caller named:
  `open(dir, O_RDONLY|O_DIRECTORY)` `0x18000`, `open(O_RDONLY|O_NOFOLLOW)` `0x28000` (`O_NOFOLLOW`
  *is* reported, so the bit is not open-only on Linux, which `src/syscall/linux/abi.rs`'s comment
  about it used to say it was), `open(O_PATH)` `0x200000`, `O_SYNC`/`O_DSYNC` `0x109001`,
  `O_ASYNC` `0xa000`, `O_DIRECT` `0xc002`, `O_NOATIME` `0x48000` - while `O_CREAT`/`O_TRUNC` and
  `O_CLOEXEC` are **not** reported, the first pair being spent by the open and the last being a
  *descriptor* flag (`F_GETFD`'s word). Two members of Linux's word stay deliberately missing here:
  `O_LARGEFILE`, which this kernel neither defines nor invents (`t-open-flags` pins that it is
  ignored on both machines), and the three bits `SETFL_MASK` is narrower by - `O_ASYNC`,
  `O_DIRECT` and `O_NOATIME` are **reported** when an open asked for them but cannot be *set*
  through `F_SETFL`, where Linux's mask allows all three. A caller that compares the whole word
  against what it opened with now agrees on everything it can ask for, which is what `rustix::fs`'s
  `Dir` and any reopened `/proc/self/fd` entry read it for. Owner: `src/vfs/file.rs`'s
  `File::born`/`set_born` and `open_flags`, set at the one open path. `docs/notes/` §15.16,
  §15.67.
- **`__WCLONE` narrows the wait to the kind of child `clone`'s low byte named, and
  the byte survives on the task. Landed 2026-10-18** (`docs/uapi/` §5.1 item 50).
  The bit is not a hint: Linux reads `clone`'s low byte at the *other* end and
  splits children into two kinds — born to report `SIGCHLD` ("non-clone") and born
  to report nothing or something else ("clone") — and a wait may only be answered
  about the kind its option word admits. `__WCLONE` is the clone kind alone,
  `__WALL` both, and neither is the non-clone kind alone. This kernel accepted the
  bit and reported every child, so the two doors disagreed with Linux about which
  children the caller has — measured whole on a current host first
  (`.workbuddy/probe_wclone.c` and `probe_wclone_block.c`, Linux 7.2.9):

  * **a "clone" child is not a child a plain wait has**: `clone(0)` child exited,
    `wait4(-1, WNOHANG)` is `ECHILD` (**not** `WNOHANG`'s 0) — and so is
    `wait4(clone_pid, WNOHANG)` and `waitid(P_PID, clone_pid, WEXITED)`: naming a
    child does not widen the option word. `__WCLONE` collects it, status word and
    all;
  * **the mixed set is the shape that tells a filter from a widening**: with a
    forked child and a live `clone(0)` child, `wait4(-1, 0)` returns the *forked*
    one, and the next `wait4(-1, WNOHANG)` is `ECHILD` with the clone child still
    alive — and stays `ECHILD` after it dies. `__WALL` is what collects it;
  * **a non-`SIGCHLD` signal makes a clone child too**: `clone(SIGTERM)` is
    invisible to a plain wait and visible to `__WCLONE` (`__WALL` collects it);
  * **the blocking shapes answer at once**: with only a live clone child,
    `wait4(-1, 0)` and `waitid(P_ALL, WEXITED)` are `ECHILD` immediately rather
    than parking — the half a scan can get wrong while every non-blocking answer
    is right;
  * **`__WALL` wins over `__WCLONE`** when both bits are set (a `SIGCHLD` child is
    reported by `__WALL|__WCLONE`, where `__WCLONE` alone is `ECHILD`);
  * **the delivery half landed 2026-10-18, and the whole rule was measured rather
    than assumed** (`docs/uapi/` §5.1 item 59): Linux uses the same byte to notify
    the parent, so a `clone(SIGTERM)` child's death puts a `SIGTERM` —
    **not** a `SIGCHLD` — with `si_code` `CLD_EXITED` (1), the child's pid and real
    uid and the exit code in the parent's queue. Measured one scenario per
    disposition on a current host first (`.workbuddy/probe_exitsig.c`, Linux
    7.2.11): a `clone(0)` child delivers **nothing** whatever the mask; the byte
    is the signal, not a flag; and the predicate has two named halves — `SIG_IGN`
    **drops** a non-`SIGCHLD` byte even when blocked, the one place "a blocked
    signal is never ignored" does not hold (F1 against K1), and the set of exit
    signals Linux ignores by default is exactly `sig_kernel_ignore`
    (`{SIGCHLD, SIGURG, SIGWINCH}`), so a `clone(SIGURG)` death is dropped at
    `SIG_DFL` unblocked while a `clone(SIGTERM)` death at `SIG_DFL` unblocked
    **kills** the parent. `SIGCONT` is *not* in that set, which a second probe
    settles the only way it can be seen — its default is *resume*, so a
    `clone(SIGCONT)` child's death resumes a **stopped** parent
    (`.workbuddy/probe_exitsig_cont.c`), and the record is delivered. Served by
    `sched::notify_parent_of_death`, pinned by `t-clone-exit`.
    `__WNOTHREAD` also stays accepted-and-ignored (there is no second scan for it
    to exclude a sibling group from), and `/proc/<pid>/status`'s `exit_signal`
    line is still the constant it has always been, which is a rendering nothing
    pins.

  Owner: `src/sched/runtime/task.rs`'s `ChildKind` and `Task::exit_signal`,
  threaded through the one eligibility rule (`trace.rs::waitable`) and the record
  (`Retired::exit_signal`), and `src/syscall/linux/proc.rs`'s
  `wait_child_kind` — one decoder for both doors. Pins: `t-wait4` (11 claims,
  51 in the file) and `t-waitid` (5, 68 in the file), every one of them green on
  a current host before it was green here (61 of the 68 are reachable there; the
  rest move a child's ids, which needs a privilege). `docs/notes/` §15.68. The
  delivery half is `src/sched/runtime/child_signal.rs`'s `notify_parent_of_death`
  (the rule and its measurement), called from `child_exit.rs` with
  `signal::SigInfo::child`, `sched::signal::ignored_by_default` beside
  `default_action`, and the pin `t-clone-exit` (29 claims); `docs/notes/` §15.78.

- **`signalfd(2)` and `SIGCHLD`: the three divergences are served, and what the
  record still leaves.** **Landed 2026-10-17** (`docs/uapi/` §5.1 item 29), after
  the whole contract was re-measured on a current host with
  `.workbuddy/probe_signalfd_chld.c` (Linux 7.2.9) — a scenario per disposition,
  each in its own forked child, because the first draft of that probe measured
  its own leftovers. The entry this replaces named three divergences; all three
  are gone, and the measurement is what replaced them:

  * **one death is one record.** A child's death *is* the `SIGCHLD` record —
    Linux's pending queue is the one table — and the second table this kernel
    kept for it (`sched::read_sigchld`, read by the descriptor as a synthetic
    child-exit record) is deleted. Measured, blocked `SIG_DFL`: exactly one
    record, `code=1[CLD_EXITED] pid uid status=7`, then `EAGAIN`;
  * **that record is Linux's**, with the exit code in `si_status`:
    `CLD_EXITED`+7 for an exit, `CLD_KILLED`+9 for a child that takes `SIGKILL`,
    the child's pid and its real uid — through the descriptor, through a
    handler's `siginfo_t` and through `sigtimedwait` alike (all three measured,
    and all three now render the status the same way);
  * **a death the parent never asked to be signalled for leaves nothing**.
    Measured, the four dispositions: unblocked `SIG_DFL` → **nothing** (the
    zombie is still `wait(2)`'s), blocked `SIG_DFL` → the one record,
    `SIG_IGN` → nothing *and* `waitpid(WNOHANG)` is `ECHILD`, and a handler →
    the record. This kernel reported the exit whatever the mask said, which is
    what the no-std version had relied on.

  `t-signalfd` is the pin — 46 claims at this landing, **62** after the CPU pair
  below (60 with two notes on a host run, where the real uid cannot be moved:
  `t-waitid`'s shape for that field), no per-machine branch left, green on a
  current host first — and it claims both halves of the disposition rule
  (the unblocked death is `EAGAIN`; the sigkill record is `CLD_KILLED` with the
  signal in the status) plus the frame's rendering of the same record.

  **One residual stays named rather than hidden.** A child whose `SIGCHLD` is
  **ignored** is reaped by the kernel on Linux (`waitpid(WNOHANG)` measured as
  `ECHILD`); this kernel keeps the zombie, so a parent that ignores the signal and
  never waits holds a slot where Linux holds none. Closing it means skipping the
  retired record for such a parent *and* making the reap prompt, which is a second
  behaviour rather than a line of this one. Owner: `src/sched/runtime/`'s exit
  path. `docs/notes/` §15.11 (batch 21, where the divergences were named) and
  §15.44 (the landing). **The other two residuals this entry named have since
  landed:** the record's `si_utime`/`si_stime` (Linux's are the child's CPU in
  ticks — `9` for a 100 ms child, `0`/`0` for one that only exited; now carried,
  §5.1 item **33**) and `CLD_DUMPED`/`WCOREDUMP` (§5.1 item **32** — the entry
  below carries it, and it is the one whose measurement also corrected the
  *reason* this tree used to give, so it is worth reading beside the paragraph
  above).

- **A death by a core-dumping signal is `CLD_DUMPED` with `WCOREDUMP` — when the
  child was dumpable.** **Landed 2026-10-17** (`docs/uapi/` §5.1 item 32). The
  third residual of the `SIGCHLD` entry above, closed. Two probes, because the
  rule has two halves. `.workbuddy/probe_coredump_signals.c` (Linux 7.2.9,
  `RLIMIT_CORE` 0) walks every fatal signal and reads back both the `wait4`
  status word and `waitid`'s `si_code`: exactly **`SIGQUIT`, `SIGILL`, `SIGTRAP`,
  `SIGABRT`, `SIGBUS`, `SIGFPE`, `SIGSEGV`, `SIGXCPU`, `SIGXFSZ`, `SIGSYS`** dump
  (`sig_kernel_coredump`'s set) and every other fatal signal is `CLD_KILLED` —
  and the bit ignores `RLIMIT_CORE`, which is what made the "mpos writes no core
  files" reason this kernel's comment gave the *wrong* reason: a `SIGSEGV` child
  reports the bit with the limit at 0 and read back as 0. The second probe,
  `.workbuddy/probe_wcore_dumpable.c`, then split *signal* from *dump attempted*:
  under `PR_SET_DUMPABLE 0` — which makes `do_coredump()` bail at its first check,
  where this host's **pipe** `core_pattern` defeats `RLIMIT_CORE` — the *same*
  `SIGSEGV` death is `CLD_KILLED`/bit-clear, while `dumpable` 1 sets it, and the
  flag is the *child's* (the kernel's own `SUID_DUMP_ROOT` value, 2, is not
  settable through this door — Linux answers `EINVAL` — so 1-versus-0 is the pair
  the door can show). Served by computing the classification once at the
  death (the signal's `abi::signal_dumps_core` **and** the child's `dumpable`) and
  threading it through `abi::exit_chld`/`abi::wait_status` on both records, so
  `wait4` and `waitid` agree about one death. Pins: `t-wait4` (a dumpable
  `SIGSEGV` sets the bit, `SIGTERM` clears it, and a `PR_SET_DUMPABLE 0`
  `SIGSEGV` clears it) and `t-signalfd` (`CLD_DUMPED` through the descriptor,
  `CLD_KILLED` when the child is non-dumpable). `docs/notes/` §15.49.

- **A UDP send's full buffer answered `EMSGSIZE`, so a burst stopped at four — and the blocking
  half of Linux's rule is served now, with its bound named.** A socket's send buffer is finite, so a
  burst outruns it; what the caller hears then is Linux's question, and Linux's answer has two halves
  — a *blocking* sender waits for room, a non-blocking one is `EAGAIN` — with `EMSGSIZE` kept for the
  datagram wider than the buffer that would have to hold it. This kernel had one mapping for both of
  `smoltcp`'s meanings of `BufferFull` ("wider than the buffer" and "no room at this instant") and
  answered **`EMSGSIZE`** to each, so a perfectly ordinary 1400-byte datagram was refused as though it
  were oversized. Measured by `t-netstats`, which sends 256 of them back to back: **four were
  accepted** — the depth of the virtio TX ring — and every send after that was refused, so
  `tx_packets` moved by four and the burst's rate said nothing. `src/net/socket.rs`'s `udp_send` now
  asks which case it is (`data.len() > sock.payload_send_capacity()` keeps `EMSGSIZE`) and answers
  **`EAGAIN`** to the momentary full. **The wait is served too**, and it is §5.1 item 27: the state a
  full buffer puts a caller in is one Linux's blocking sender *parks* out of, and the park's wakeup
  source turned out to be the stack's own poll — the wake list every poll wakes, which is exactly the
  event that drains a socket's send buffer — so what it needed was a condition true only when the
  buffer can really take the datagram. Both earlier shapes failed on that condition rather than on the
  wakeup: a park with no deadline stopped the sweep dead at `t-netstats`'s first parked send, and a
  tick-bounded park spun at **97% CPU** because the wait's own readiness answer (`can_send()`, the
  metadata ring) was true while `enqueue` still refused on the payload ring — a park whose condition
  holds returns at once, so the caller retried, was refused and parked again without ever sleeping.
  The condition now asks **both** of `enqueue`'s questions: the packet count and the payload window
  (`udp_room_for`), whose one blind spot — the ring's wraparound — clears as the buffer drains and
  costs one more turn of a loop bounded by the four packet slots. **Measured whole on a current host
  first** (`.workbuddy/probe-udpblock*.c`, Linux 7.2.9): with `SO_SNDBUF` at its floor and an on-link
  neighbour that never answers ARP holding the charge, three 1400-byte datagrams go in, the fourth
  *blocking* send sleeps **3072-3098 ms** until the neighbour gives up and then returns the datagram,
  and the non-blocking send at the same point is `EAGAIN` at once; a signal inside the park answers
  `EINTR` without `SA_RESTART` and is swallowed — the send restarted and completed — with it.
  **Three things stay named rather than served**, and two of them are the *size* question rather than
  the room one: **Linux's `EMSGSIZE` line is the maximum UDP datagram** (65507 accepted, 65508
  refused, measured), not the send buffer, so a datagram *wider* than `SO_SNDBUF` is still taken
  (5600 bytes into a 4608-byte buffer) — where this kernel refuses it, because its payload ring is
  4 KiB and the datagram can never be enqueued; that is the buffer-size item the ROADMAP carries
  (`SO_SNDBUF`/`SO_RCVBUF` record-only), and it moves the line the day the ring does. **The park is
  bounded** at `DRAIN_WAIT_MS` (three seconds, the window Linux's own neighbour resolution takes):
  past it the answer is `EAGAIN`, because smoltcp *keeps* a datagram its egress could not dispatch —
  an unknown neighbour's silent second, a datagram the MTU cannot carry, a link taken down — where
  Linux's device drops the frame and releases the charge, so an unbounded park here would be a hang
  rather than a wait; the bound's worst case is the behaviour this pass replaced. And the
  **wraparound** above. Until then `t-netstats`'s burst retries on `EAGAIN` (a bound, not a tolerance)
  and prints the retry count as a measurement, and its exactness pair — one `sendto` moves
  `tx_packets` by exactly one and `tx_bytes` by exactly the frame's size — is the pin. The park
  itself is `t-udpblock`. `docs/notes/` §10.4.

- **The buffer numbers are Linux's, and a datagram socket's send queue is the number it
  names. Landed 2026-10-18** (`docs/uapi/` §5.1 item 58). What stood here was
  "`SO_SNDBUF`/`SO_RCVBUF` are record-only": this kernel stored the caller's figure
  unchanged, read it back unchanged, and let the ring behind it answer for capacity.
  Measured whole on a current host first (`.workbuddy/probe_sockbuf.c`, Linux 7.2.11,
  `net.core.wmem_max` and `rmem_max` both 4194304, their `*_default` both 212992):
  `setsockopt` stores **twice** the ask (2304 reads 4608, 10000 reads 20000, 100000 reads
  200000); the doubling is clamped from below at each option's own floor - `SO_SNDBUF`
  4608, which is why asking 100, 1 or 0 all read 4608, and `SO_RCVBUF` 2304, whose
  doubling is visible again at 2048 -> 4096; the ceiling is the sysctl maximum applied to
  the *ask*, which is why ten million reads back 8388608; and a **negative** ask is not an
  error at all - read as unsigned it is capped at the maximum, so `setsockopt(-1)` answers
  0 and the option reads 8388608. A fresh socket reports its family's own default: 212992
  for both datagram options and 16384/131072 for a stream (`tcp_wmem`/`tcp_rmem`'s first
  entries). The queue behind the number was measured too, with the dead-neighbour fill
  `docs/notes/` §10.4 uses: 1400-byte datagrams from an unread buffer stop after **2** at
  the floor, 4 at 9216, 9 at 20000 and 57 at 131072 - about 2.2 KiB charged per datagram,
  and a 5600-byte datagram is *accepted* into a socket whose buffer reads 4608, so the
  send buffer is not the datagram line.

  Both halves are served now. The number is Linux's (`net::sock_buffer_size`, its floors
  and its 4 MiB ceiling), and for a datagram socket it is what a sender is **refused by**:
  `udp_send` asks the buffer before the enqueue and `udp_room_for` asks the same question,
  so a socket at the floor admits three 1400-byte datagrams and one at the default admits
  the ring's eleven. Linux's grace rides with it - an empty queue admits one datagram
  whatever its size. The ring rose from 4 KiB to 16 KiB for this to be possible at all (a
  ring smaller than the smallest buffer a caller can ask for could never be the one that
  binds) and from 4 packet slots to 16, so the pool can hold the eleven.

  **What stays named.** `SO_RCVBUF` is the number a caller reads and not a window this
  kernel resizes: the receive ring is fixed at 16 KiB, so a socket that asks for less does
  not shrink it (Linux drops at delivery once the queue passes the buffer). The
  datagram-too-big line is still the ring's rather than Linux's 65507 - this stack has no
  IP fragmentation, so a datagram wider than a frame cannot leave, and `EMSGSIZE` is the
  honest answer where a queue entry would die at dispatch. And the count a floor buffer
  admits is 3 here against Linux's 2, because this kernel charges the payload where Linux
  charges each skb's truesize.

  Pins: `t-sockbuf`, a new program with **12 claims** - the transformations, runnable on a
  host as well as in the guest because the floors and the ceiling are the same numbers on
  both - and `t-udpblock`'s `the fill stops where the send buffer's number says`, which is
  the governing half. `docs/notes/` §15.77 carries the gates and the counterfactual.

- **A terminal's foreground group is *session state*, and a reader is not its owner.
  Landed 2026-10-18** (`docs/uapi/` §5.1 item 56). The entry that stood here had two
  halves; the second - the number `TIOCGPTN` mints past the six virtual consoles - is
  this kernel's own and stays pinned as its own (`t-pty`: the payload under the sweep,
  the host's number printed under `MPOS_ON_HOST`). The first was the divergence, and it
  is closed: Linux keeps a terminal's foreground group strictly as the *session's*
  answer, set when a session leader makes the tty its controlling terminal
  (`__proc_set_tty`) or by `TIOCSPGRP` from that session, and reading never sets it.
  This kernel answered with the reader - `Tty::read` recorded the reading task's group
  as the terminal's foreground ("the same answer for a program that never asks") - so a
  program that read a tty it never claimed became its owner: it received the typed
  `SIGINT`/`SIGQUIT`/`SIGTSTP` and the resize `SIGWINCH`, and `tcgetpgrp` named it.
  Measured whole on a current host first (`.workbuddy/probe_tty_fg.c` and
  `probe_tty_fg2.c`, Linux 7.2.11, every scenario in a session of its own):

  * **a pair nobody holds has no group**: `TIOCGPGRP` on the *slave* is `ENOTTY`
    (errno 25) and on its *master* answers `0`; `TIOCSPGRP` is `ENOTTY` on either end,
    and `TIOCSWINSZ` succeeds with nobody to tell. The master's answer is not an
    oversight - Linux asks the controlling-terminal question of the slave end only
    (`tty == real_tty && current->signal->tty != real_tty`), because a supervisor that
    holds nothing but the master has no other way to learn whether it can do job
    control at all;
  * **being a session leader is not holding it**: the same answers after `setsid`
    until `TIOCSCTTY` lands, and then the group is the leader's own - on both ends,
    since a pair is one terminal;
  * **the refusals are ordered and distinguishable**: from the owning session
    `TIOCSPGRP` answers `EFAULT` for a bad pointer, `EINVAL` for a negative number,
    `ESRCH` for one that names no process (zero included), `EPERM` for a group in
    another session - and the group it would not take is where it was;
  * **reading is not claiming**: a process in *another session* reads the slave and the
    answer does not move. That is the row the old kernel failed.

  Two more measured shapes are named rather than built: a background group that tries
  to change the terminal is refused `ENOTTY` when its group is orphaned (with `SIGTTOU`
  caught) and served when `SIGTTOU` is ignored - this kernel has no terminal-generated
  `SIGTTOU` to send, so it serves the call; and a *background read* of the terminal's
  own session is `SIGTTIN`'s business on Linux (measured: the reader is stopped, its
  read never completes), which is a signal this kernel does not raise either. Neither
  is reachable from the fixture, which puts its reader in another session precisely so
  both sides of the boundary are the shapes measured. `TIOCNOTTY` is served now
  (§5.1 item 57): the leader's call gives the *session's* terminal back - its foreground
  group hears a `SIGCONT` and a `SIGHUP`, the terminal keeps no foreground, `/dev/tty`
  stops opening for every task that named it, and the pair is not hung up - while any
  other member's call clears only its own record and the session keeps the line.

  Owner: `src/vfs/tty.rs`'s `read`/`foreground` (the claim is gone), the `0x540F`/`0x5410`
  and `0x5422` arms of `src/syscall/linux/fs/ioctl.rs` (the session questions, the order
  of the refusals, and the hand-back), and `src/sched/runtime/child_signal.rs`'s
  `session_of`, `session_of_tty` and `release_controlling_tty`. Pin: `t-pty`'s three
  sections - the two foreground ones, **17 claims** (54 in the file, was 37), and the
  hand-back's **18** (72 now) - all 67 printed claims green on a current host first.
  `docs/notes/` §15.75 and §15.76 carry the gates, the counterfactuals and the fixture
  lessons.

- **`FIONREAD` answers Linux's per-type byte count on a regular file, a pipe, a socket and a
  terminal. Landed 2026-10-17** (`docs/uapi/` §5.1 item 30). The entry this replaces said this
  kernel answered `ENOTTY` for everything that is not the entropy pair; Linux's answer is one rule
  *per file type*, all measured on a current host first (`.workbuddy/probe_fionread.c`,
  `probe_fionread2.c`, `probe_ttyinq.c`, `probe_netinq.c`, `probe_unixinq.c`, Linux 7.2.9): a
  regular file is `i_size - f_pos` and is **not clamped** (a 10-byte file at offset 20 answers
  `-10`, because Linux's `put_user(i_size_read(inode) - file->f_pos, int __user *)` truncates the
  wide subtraction to a signed 32-bit word; a `/proc` file, a regular file by `S_ISREG` with size
  0, answers `-f_pos`); a pipe is its queued bytes on *both* ends; a **socket** answers its own
  receive queue - a connected **stream** end (a `socketpair` stream half, an AF_UNIX stream, TCP)
  the whole readable stream (3 + 4 written answers 7, not the first write), a **datagram** end its
  *first* pending message (5 then 9 queued answers 5, an empty queue 0), a fresh unconnected stream
  0, and a **listener** `EINVAL` (TCP and AF_UNIX alike); a **terminal** answers its input queue - a
  completed canonical line (canonical `"hi"` answers 0, `"hi\n"` 3, `"ab\ncd\n"` 6) or every raw
  byte (raw `"hello"` 5, 3 after a two-byte read) - while a pty **master** reports what the slave
  has printed (3). The two entropy devices answer `EINVAL`; a directory, an `eventfd`, `/dev/null`,
  `/dev/zero` and a **netlink** socket answer `ENOTTY`. Each node that holds the queue answers
  through the new `Inode::fionread` trait method (`src/vfs/mod.rs`), so the syscall layer keeps only
  the two answers it alone can give - the regular file's `size - pos` and the kind refusal.
  **The correction this entry carries**: the old text said a tty "answers 0", which is only the
  canonical-mode case where nothing complete is queued (the earlier probe had left raw bytes in the
  line buffer); the real rule is the readable input, measured cleanly per case above. **One
  residual stays named**: a **raw** socket's Linux answer is unmeasured here and the socket is
  unreachable to a caller, so it is refused `ENOTTY` rather than guessed. Owner:
  `src/syscall/linux/fs/ioctl.rs`'s `0x541B` arm plus the `Inode::fionread` overrides in
  `src/vfs/pipe.rs`, `src/vfs/tty.rs`, `src/vfs/pty.rs`, `src/net/socket.rs`, `src/net/unix.rs` and
  `src/net/mod.rs`; pinned by `t-ioctl` (regular file, pipe, `socketpair` stream + datagram),
  `t-pty`, `t-tcp-listen` and `t-udp-loopback`.
- **`RNDRESEEDCRNG` provenance.** Now that the reseed is real, `RNDGETENTCNT`'s 4096 stays
  a constant across it; a caller that reseeds and re-reads sees the same figure Linux shows,
  so no change is needed — recorded so the next reader does not "fix" it into a lie.
- **`/proc/<pid>/smaps` is served, and the three numbers this kernel cannot derive are a
  rule rather than a guess. Landed 2026-10-18** (`docs/uapi/` §5.1 item 54). The entry this
  replaces said `maps` was served in Linux's exact format while the two siblings were
  unwritten. `smaps` is the one `smem`, `ps_mem` and every compositor asking how much of a
  client's `wl_shm` buffer is resident read, and it is now served in Linux's own shape:
  each record *is* the `maps` line for its mapping byte for byte (measured over a process
  covering every shape: 34 of 34 records agreed), then Linux's twenty-four field lines in
  Linux's order, each value ending in column 24 whatever its width, with `THPeligible` and
  `ProtectionKey` carrying no unit and `VmFlags`' words each followed by a space.

  What is *not* Linux's is three of the accounting numbers, because this kernel has none of
  what Linux computes them from — no mapcount (`Pss`), no dirty bit and no page cache (the
  four shared/private buckets), no accessed bit (`Referenced`) — and each divergence is
  measured on both sides rather than asserted. The file answers with one rule: **every
  resident page is this mapping's own, and it is dirty**, so `Pss`, `Pss_Dirty` and
  `Private_Dirty` are `Rss`, `Shared_*`/`Private_Clean` are zero, `Referenced` is `Rss`
  (a page here becomes resident only by being faulted in), and `Anonymous` is the resident
  set exactly when no object owns the pages — which is Linux's own answer for the two
  shared shapes measured (`memfd`, shared anon: `Private_Dirty`, `Anonymous: 0`). Everything
  else is zero because the feature behind it does not exist: no KSM, no huge page, no pmd-mapped
  page, no hugetlb, no swap, no pkey, and `Locked` because `mlock` here takes no lock.
  `VmFlags` carries the same *grant* the `maps` permission column does — `rd`/`wr`/`ex`,
  `sh`/`ms`, and `mr`/`mw`/`me` unconditionally — with Linux's `ac` and `sd` deliberately
  absent, since printing either would claim a state nothing here keeps. Pin: `t-smaps`, 34
  claims, green on this host first, beside four renderer unit tests; `docs/notes/` §15.73
  carries the gates, the counterfactual and the two fixture lessons.

- **`/proc/<pid>/numa_maps` and `/proc/<pid>/smaps_rollup` are served — the two
  other spellings of `smaps`' accounting. Landed 2026-10-19** (`docs/uapi/` §5.1 item
  62). The entry that stood here had both files' formats written down and neither
  file written: "`numa_maps` is `<start> default [file=…] [anon=N] [dirty=N]
  [mapped=N] [mapmax=N] [active=N] N0=N kernelpagesize_kB=4` with spaces inside a
  name escaped as `\040`", and "`smaps_rollup` is one record for the whole address
  space under a `[rollup]` header". Both are now served, and both were measured
  again rather than taken from that note — because two of the measurements are
  mistakes a probe makes before it knows better:

  * **`/tmp` is tmpfs on this host**, so a "file" mapping made there is a shmem
    mapping whose folios are dirty from the moment the probe wrote them: a
    *read-only* mapping reported `dirty=4`. The files moved to ext4 and the answer
    changed shape.
  * **adjacent anonymous mappings merge**, so two shapes became one VMA and one of
    them looked absent. Every shape is fenced by `PROT_NONE` pages now, and the
    `maps` line beside every answer proves it.

  What `numa_maps` says, one line per mapping in `maps`' order (the gate VMA
  excepted: `[vsyscall]` has a `maps` line and no `numa_maps` one, because it is not
  a VMA of the address space at all): the **start alone, no `-end`**, then the
  policy — `default` for every mapping, since `set_mempolicy`/`mbind` are not served
  — then the name, which is `file=<path>` when a path is known, `heap` for the heap
  and `stack` for the stack, and **nothing at all** for a name that is not a path
  (`[vdso]`, `[vvar_vclock]`: measured, they print no name either). A path's space is
  `\040` and its tab `\011`; a backslash is left alone. Then, **only when the range
  holds a page**: `anon=` for a range whose pages are the mapping's own, `dirty=` for
  any the kernel counted dirty, `mapped=` where Linux's own `anon`+`file` counts do
  not add up to its total, `mapmax=` when some page's mapcount exceeds one,
  `active=`, one `N<node>=` per node, and `kernelpagesize_kB=4`. A range with no
  resident page prints the policy and no number at all — not `N0=0`.

  Two of those are the reason this kernel's line is shorter than Linux's, and both
  are named: **`mapped=` and `mapmax=` are absent**, because the first is a question
  about Linux's own two counters and the second is a mapcount — the same fact
  `smaps`' `Pss` already names. So is the **`stack`** word: this kernel names the
  heap and not the stack. And a *read-only* object-backed range answers `dirty=N`
  where Linux answers `mapped=N`, because nothing here records whether a page was
  written — item 54's rule, "every resident page is this mapping's own, and it is
  dirty", applied once more. `active=0` is the honest number: the count is the pages
  on Linux's active LRU list and this kernel keeps no LRU, and Linux prints the field
  for every range whose count differs from its total, which is every range holding a
  page.

  **One divergence is the other way round and is worth a sentence of its own:
  reading an anonymous page gathers nothing on Linux.** A read fault is satisfied
  with the shared zero page, which is not a page it counts, so a two-page read of a
  private anonymous range came back with no counts at all — where this kernel
  materialises a real frame and reports `anon=1 dirty=1`. It is named rather than
  claimed (`t-numamaps`' header carries it), because a claim has to be true on both.

  `smaps_rollup` is one record for the whole address space: a header that is the
  **bounds** of the space rather than one mapping's — `<first start>-<last end>
  ---p 00000000 00:00 0`, with `[rollup]` in the same column 73 a `maps` line names
  its mapping — and then twenty-two field lines, which is `smaps`' twenty-five with
  three taken out and three put in: out go `Size`, `KernelPageSize`, `MMUPageSize`
  and `THPeligible`, `ProtectionKey`, `VmFlags` (all one mapping's); in come
  `Pss_Anon`, `Pss_File` and `Pss_Shmem`. Its numbers are the sums of `smaps`' —
  `Rss` measured exactly (`smaps` 2096, rollup 2096) where `Pss` is not (278 against
  281, because each record's is rounded before it is added).

  Owner: `src/vfs/procfs.rs`'s `format_numa_maps`, `push_escaped_path`,
  `format_smaps_rollup` (the ABI, compiled for the host) and `resident_lines` (the one
  place the residency is counted, so the three files cannot disagree), and the two
  nodes beside `smaps` in `/proc/<pid>`. Pins: **`t-numamaps`**, a new program, 20
  claims, and `t-smaps`' ten more (33 → 43) — every one run against a current host
  first, and byte-identical across the two machines.
- **`devfs` birth modes.** Devices are born `0666 root:root`; `devtmpfs` births `0600`
  and lets udev widen it. Needs per-class birth modes and group-name resolution, not a new
  mechanism (`docs/notes/` §3.4).
- **The console tty is published under the machine's own serial name.** `/dev/ttyS0` on
  x86_64, `/dev/ttyAMA0` on aarch64 - one node (the console tty, where the kernel log goes
  and which the kernel's standard descriptors are) under the name the psABI gives that
  machine's first serial port, which is also the name `console=` spells. `mount -t proc` and
  `mount -t sysfs` are served on **both** machines (a real init mounts and reads them); the
  parts that are x86_64's - PCI config-space read/write, the evdev registry `/sys/class/input`
  enumerates - sit behind the `mpos_pci_config`/`mpos_input_registry` capability flags, so a
  machine without one builds the tree from the neutral model (empty on aarch64) instead of
  refusing the filesystem.
- **`open(2)`'s flag *numbers* are the machine's, and four differ.** arm64's
  `asm/fcntl.h` overrides `O_DIRECTORY` (`0x4000` vs x86_64's `0x10000`), `O_NOFOLLOW`
  (`0x8000` vs `0x20000`), `O_DIRECT` (`0x10000` vs `0x4000`) and `O_LARGEFILE` (`0x20000`
  vs `0`, i.e. absent) before including `asm-generic/fcntl.h`. `O_TMPFILE` is
  `O_DIRECTORY`'s partner, so its composite moves with it (`0x404000` on arm64, `0x410000`
  on x86_64) while its own bit, `__O_TMPFILE` (`0x400000`), does not. The overlap is
  load-bearing rather than tidy: arm64's `O_LARGEFILE` is x86_64's `O_NOFOLLOW` bit, and
  musl's `open()` adds `O_LARGEFILE` to every open - so a kernel carrying one table answered
  `ELOOP` to every `open()` of a symbolic link on aarch64, which is what stopped `openrc`
  starting its syslog service (the loader opening `libcap.so.2 -> libcap.so.2.78`). Pinned by
  `user/tests/src/bin/t-open-flags.rs` on both machines. `O_LARGEFILE` is deliberately
  undefined in the kernel: it is ignored, as Linux ignores it, and what matters is that
  ignoring it does not mean mistaking it for another flag.
- **The `DRM_VIRTGPU_*` family (`0x41..=0x4b`) is served, 2D and 3D both.** All eleven
  requests are dispatched, and each answers per card shape, as Linux's does
  (the instrument was `user/staging/virtgpu-probe/`, now deleted - its table is
  in `docs/notes/` §7.4, as are the GL card's own answers -
  `virgl=1 context_init=1 capsets=2 cached=2 mask=0x6`), with
  its one named divergence left in the `'d'` row above (of the three it carried, two
  are closed). The **mailbox** (`render::job` - an
  ioctl describes a device command and parks, because the control queue belongs to the
  render task) and a **device resource** behind a GEM handle (`Dumb.resource` in
  `drivers/drm/`) carry both halves, and the 3D commands ride the same machinery:
  `CTX_CREATE`/`DESTROY`/`ATTACH_RESOURCE`, `RESOURCE_CREATE_3D`,
  `TRANSFER_{TO,FROM}_HOST_3D`, `SUBMIT_3D`, `GET_CAPSET_INFO`/`GET_CAPSET`. There is no
  command-stream parser to fuzz because there is nothing to parse - Linux forwards the
  stream with `vmemdup_user` and never reads it (§4 rule 2's target is the
  variable-length `GET_CAPSET` reply instead, and it is
  `fuzz/fuzz_targets/virtgpu_capset.rs`). **What the family still does not buy is a
  renderer, and the reason is not in this row**: Mesa never loads the virtio driver,
  because the DRM device's parent chain carries no bus identity, so `drmGetDevices2`
  finds no card and every EGL platform falls back to `llvmpipe` - measured, with the
  sysfs attributes and the two interpositions that ruled other explanations out, in
  `docs/notes/` §7.4. **And one fix the 2D half already owes**: `CREATE_DUMB` should create a
  device resource as Linux's does, which would remove two of the three divergences above.
  It is not done because it would make every dumb buffer eagerly backed and add a device
  round trip to the path the console, weston and `/dev/fb0` all allocate through.
- **The EL0 system-instruction traps Linux answers and this kernel refuses.** Two rows of
  Linux's `do_el0_sys` table are served - the `CTR_EL0` read and the
  `DC CVAU`/`CVAC`/`CIVAC`/`IC IVAU` maintenance family, pinned by `t-el0-sysreg` - and what
  is left of that table falls through to `SIGILL`, which is Linux's own answer for a trap its
  table has no row for *but is not Linux's answer for four of these*. Linux serves the counter
  reads (`CNTVCT_EL0`/`CNTVCTSS_EL0`, `CNTFRQ_EL0`), the `CPUID` reads (`do_emulate_mrs`),
  `WFI` (whose handler simply skips it), and the point-of-persistence pair of maintenance
  operations (`DC CVAP`/`DC CVADP`). The pair is refused here for a reason the others do not
  share and it is worth naming: they need `FEAT_DPB`, and running an EL1 spelling of an
  operation the CPU does not implement is an *undefined instruction*, which this machine
  answers by stopping - so a refusal is the safe answer as well as the honest one. None of the
  four has been asked for on either machine yet (llvmpipe's flush asks for the pair that is
  served), so what is missing is need rather than design, and each refusal is a signal a caller
  can see rather than silence. Owner: `arch/src/arch/aarch64/vectors.rs`'s `el0_sys`, whose own
  doc names each. `docs/notes/` §15.36.

- **The two wait doors' writers sit where Linux's sit: the wait, then the rusage, then
  the record — and a refusal writes the record too. Landed 2026-10-18** (`docs/uapi/`
  §5.1 item 51). Item 34 gave both doors the caller's `struct rusage`; what it left was
  *where* the writes sit relative to the wait and to each other, and that is visible only
  to a caller that hands over a pointer the kernel cannot write to. Linux's `do_waitid`
  collects (or refuses) inside `kernel_waitid`, copies the `struct rusage` when — and
  only when — it has an event to report, and writes the six-word `siginfo_t` **last**, on
  every path, with that copy's own `EFAULT` outranking whatever the wait said. This door
  cleared the whole record up front, which made a bad `rusage` leave *zeros* where Linux
  leaves the caller's bytes, and made a bad record refuse *before* the wait — so the child
  Linux collects was still there. Measured whole on a current host first
  (`.workbuddy/probe_waitid_record.c`, Linux 7.2.9, scenarios P–Z and AA–AH):

  * **the wait comes first, and its child is gone by the time anything is refused**: a bad
    `rusage` (P), a bad record (Q), both (R) and a *straddling* rusage (AH) all leave the
    child collected — the second wait is `ECHILD`;
  * **the rusage is copied only when something was reported**: `EINVAL` (AB) and the
    `WNOHANG` "nothing yet" (AC) leave it untouched, where a reported death fills it
    *before* the record is touched — which is why a bad `rusage` leaves a writable record
    exactly as the caller wrote it (P: all `0xAA`);
  * **the record is written on every path**: a report writes the event, and `EINVAL`
    (S, AE for `ECHILD`), an interrupted wait and the `WNOHANG` zero write **six zeros** —
    and nothing else, so `__pad0` at 12 and every byte from 28 on stay the caller's own
    (S: `si_signo`/`si_errno`/`si_code`/`si_pid`/`si_uid`/`si_status` zero, the tail still
    `0xAA`);
  * **the record's refusal outranks the wait's**: `EINVAL` (T, U), `ECHILD` (V), a bad
    idtype (AD) and the `WNOHANG` zero (AF) all answer **`EFAULT`** when the record cannot
    be written — Linux writes the record after the wait and returns the write's error;
  * **a null record is legal**, and collects (W) — the caller that wants only the reap;
  * **the check is per word, not for the record**: a record whose *tail* is unmapped but
    whose six words all land is written and answers 0 (X), and one whose third word
    straddles the boundary is `EFAULT` with the two words before it already written
    (AG: `si_signo` 17, `si_errno` 0, `si_code` untouched). A whole-record check answers
    both the other way round;
  * **the rusage is a range copy, and it shows**: one that straddles into the unmapped
    page leaves its mapped head written and answers `EFAULT` (AH). This kernel refused the
    whole range before writing a byte, which was the same mistake one door over and is why
    the writer changed with the record's.

  Implemented as a split, not a reordering: `waitid_collect` does the word, the selector
  and the collection and returns a `WaitIdAnswer` (an errno, plus the event when there is
  one), touching no caller pointer; `handle_waitid` then runs the two writers in Linux's
  order and lets the last one's `EFAULT` win. `abi::write_waitid_info` /
  `write_waitid_empty` write the six words through one `put_siginfo_word`, which checks
  the word's own four bytes — that is what makes X and AG come out right — and
  `write_rusage` builds the 144 bytes and hands them to `sched::write_user`, the page-by-
  page copy Linux's `copy_to_user` is. Host unit tests: one per writer shape (the six
  words, the untouched `__pad0` and tail, the no-event zeros).

  Pin: `t-waitid`, **34 new claims** (102 in the file), every one of the reachable ones
  green on a current host first (`MPOS_ON_HOST=1`, 95 of them; the other seven move a
  child's ids, which needs a privilege, and none of the new ones does). Gate: x86_64
  183/0, aarch64 182/0, kunit 32/0, both machines' `ok:` lines byte-identical.
  Counterfactual (both writers put back): stage 1 unchanged at 565/0 — the unit tests pin the
  writers' *shape*, and that is not what moved — and the sweep `182/1` with `FAIL t-waitid` and
  **eleven** red claims by name. The precedence claims stay green there on purpose: refusing
  *before* the wait still answers `EFAULT`, so only "what happened to the child" tells the two
  shapes apart, and those are the claims that do.

  The `*status` half of that question is its own entry below (`docs/uapi/` §5.1 item 53) and was
  measured rather than assumed; the `si_uid` line `/proc/<pid>/status` prints is still the constant
  it always was, which is item 50's leftover rather than this one's.

- **A `wait4` status word is one `put_user`, and this door already answered it that way. Landed
  2026-10-18** (`docs/uapi/` §5.1 item 53). Item 51 named it as the neighbour it had not measured:
  `*status` was checked with a whole-word `buffer_ok` before a single write, and the open question
  was whether Linux's `put_user` there is **atomic** or a range copy that leaves its head behind the
  way `struct rusage` does (AG). Measured on a current host first
  (`.workbuddy/probe_wait4_status.c`, Linux 7.2.9, fourteen scenarios A–N), and the answer is
  atomic — so this door's shape was right and nothing in the kernel changed. With the whole page
  filled `0xAA` before every call:

  * **a word that straddles the page end is `EFAULT` with nothing written** — B, C and D (two bytes
    in and two out, one in and three out, three in and one out) all leave both reachable bytes
    `0xaa`, where the record's own range copy in the same call leaves its head behind
    (`probe_waitid_record.c` AH). The word goes whole or not at all;
  * **the reach is "mapped" *and* "writable"**: wholly inside `PROT_NONE` (E) and inside a page the
    caller made read-only (N) are both `EFAULT`, and the read-only page still holds its seeded byte
    afterwards, so the check is the permission and not just the mapping;
  * **a null pointer is "not interested"** (F): the child is reaped and the answer is its pid;
  * **`*status` is written before the record**: a refused word leaves a good `rusage` untouched (G),
    which is the mirror of item 51's own pair, where a refused record leaves the word written (H);
  * **the two answers that are not reports never reach the word**: the `WNOHANG` zero (J) is still
    `0` and `ECHILD` (K) is still `ECHILD`, both with a pointer the kernel could not write to;
  * **a refusal that does reach a report has already taken the event**: a refused death leaves the
    second wait `ECHILD` (M), and a refused *stop* is gone on the next look (L) — the loss is
    Linux's own behaviour here, not an accident of this kernel.

  Pin: `t-wait4`, **33 new claims** (84 in the file, was 51), all green on a current host first
  (`MPOS_ON_HOST=1`, and none of them needs a privilege) — every one of the fourteen scenarios the
  probe walked is a claim now, the control (A), the other two splits (C, D) and the both-refused
  shape (I) included. Gate: x86_64 `pass=183 fail=0 (183 built)`, aarch64 `pass=182 fail=0`,
  `mpos-kunit 32 passed, 0 failed` on both, exit 0; both machines' `ok:` lines byte-identical (84
  and 84, diffed clean), and equal to this host's own run but for item 34's one named divergence
  line about `ru_maxrss` and the fault/switch counters. Counterfactual: writing the word byte by
  byte with each byte passing its own `buffer_ok` — the range-copy shape, i.e. the shape the record
  has — is `pass=182 fail=1 (183 built)` with `FAIL t-wait4 exit=1` and **four** claims red by name:
  the three reachable-byte claims (B, C, D) and the both-refused one (I). Each of their own `EFAULT`
  claims stays green either way, and that asymmetry is the point: an error alone cannot distinguish
  "refused atomically" from "refused after writing what it could reach".

- **A raised `RLIMIT_SIGPENDING` buys no room past the ring.** **Landed 2026-10-17** (`docs/uapi/`
  §5.1 item 35): the queue's bound is the resource now, in Linux's shape — the target user's total
  against the target's `RLIMIT_SIGPENDING`, with `sigqueue` answering `EAGAIN` and `kill` dropping
  the copy. The one corner the storage leaves: the records live in a fixed per-task ring of
  `RT_QUEUE_LEN` (16), so the resource's **default** is 16 rather than Linux's `max_threads / 2`,
  and a caller that *raises* the limit (root may raise the hard one) still meets the ring — it gets
  `EAGAIN` at 16 anyway, which is honest about what this machine can hold but is not Linux's count.
  It is a corner rather than a hole because nothing is lost silently and the refusal is a signal a
  caller can see; growing the ring is bounded memory per task (the worst-case rule, `AGENTS.md` §4),
  which is why the number is where it is. Owner: `src/sched/signal.rs`'s `RT_QUEUE_LEN`, named in
  its own doc. Pinned incidentally by `abi.rs`'s row-11 test, which reads the default against the
  ring.

- **A mount point answers the mount, and a name a mount sits on may not be taken away. Landed
  2026-10-18** (`docs/uapi/` §5.1 item 45). Named 2026-10-17 by §5.1 item 37's own gate run, which
  is where the first half was seen: the no-follow resolver split the last name off its parent,
  resolved the parent, and looked the final component up *in it* — so the mount crossing the walking
  resolver performs on every child it looks up never happened for the last component, and
  `lstat("/proc")` answered the ramfs directory the initramfs created (`0755`, where the same name
  through the *following* door answered the mounted root's `0555`). This entry's own sentence about
  why it was not a two-line fix is what the landing turned on: one helper answered both questions,
  and the stat doors want the crossing while the make-or-remove doors want the covered **name**
  (Linux's `rmdir` of a mount point is `EBUSY`, not the removal of the mounted root). Measured whole
  on a current host first, in nine sections (`.workbuddy/probe_mountpoint_doors.c`, Linux 7.2.9,
  root) with a tmpfs over a `0755` directory and a file bind so one mode word and one `st_dev` say
  which inode answered; the tables, the orderings and the tokens are in §5.1 item 45 and in
  `docs/notes/` §15.63. **What landed is the split the sentence implied**: `resolve_walk` gained a
  trailing-link mode (Linux's `LOOKUP_FOLLOW` against `LOOKUP_NOFOLLOW` for the last name only —
  the flag says nothing about mounts, which are crossed under either, which is exactly why the two
  questions are two arguments), the no-follow resolver *became* that walk so the crossing happens
  with the walk's own bind state, and the old parent-and-lookup body survives as
  `resolve_entry`/`resolve_entry_at` for the three doors whose question is a name (`mkdirat`'s,
  `mknod`'s and `mknodat`'s `EEXIST` probes). The refusal is `is_mount_point` — a fact about the
  node, Linux's own `d_mountpoint` — asked at the end of `may_remove` (after the permission half,
  where the measurement puts `EACCES` before `EBUSY`) and on either end of `renameat2`, whose type
  pair (`EISDIR`/`ENOTDIR`) comes first for the same reason. Pin: `t-mountpoint` (37 claims, both
  machines, green on this host first) beside `t-proc-modes`' 20 and three host unit tests. **Two
  things the probe found stay named**: a `rename` between a bind's own view and the outer tree is
  `EXDEV` on Linux (`old_path.mnt != new_path.mnt`) and is still served here, and the *file* mount
  point — the only case where `unlink`'s half of the refusal is reachable — cannot exist in this
  kernel, since every mount it can make lands on a directory.

- **The three capability scopes this kernel's one word could not express. Named 2026-10-17, all
  three landed by 2026-10-18** (`docs/uapi/`
  §5.1 items 38, 39, 40 and 42; `crates/cred`'s `Credentials::user_ns_initial`). The landed change
  gives the model the two questions Linux's own call sites ask — the caller's *own* namespace
  (`has_effective`, what `unshare`'s `CAP_SYS_ADMIN` and `chroot`'s `CAP_SYS_CHROOT` ask) and the
  **initial** one (`capable`, what `mknod`'s, `fileattr_set`'s, `syslog`'s and `reboot`'s ask,
  measured in `.workbuddy/probe_capable_scope.sh`: a user-namespace root is served the first two and
  refused the last four). Linux has three more shapes, and each site below asks one of them:

  - **`setns` asks the namespace being *joined*** — `ns_capable(ns->user_ns, CAP_SYS_ADMIN)` — and
    the check comes *before* the "already in it" no-op. Measured (the probe's `setns` mode): a
    credential scoped to a child user namespace, holding the whole set in it, is `EPERM` when it
    joins a mount namespace owned by the initial one. This kernel asked the caller's own, so such a
    caller was allowed where Linux refused. **Landed 2026-10-18** (`docs/uapi/` §5.1 item 42):
    `Namespace::owner` holds whose each namespace is — written where one is *made*
    (`vfs::ns::mint_owned`), with `USER` its own owner, which is Linux's `ns_common::user_ns` and
    the reason joining a *sibling* from inside one is `EPERM` while joining one made *inside* it is
    served — and `userns::Caller::capable_in` is the four-clause walk that answers it, whose third
    clause (the caller *made* the next one down) is what makes a namespace a grant rather than a
    view: a caller with an empty set can still join what it made. The whole table is a host unit
    test in `crates/userns`, and `t-ns-setns` pins the two halves on the machine — `EPERM` for the
    no-op the machine's own namespace is served, and `0` for the same no-op once the namespace is
    the caller's own (a *real* re-join is not expressible here yet: a descriptor in this kernel
    follows the process, which is the entry at the bottom of this file).
  - **`AF_PACKET` asks the *network* namespace's owner** — `ns_capable(sock_net(sk)->user_ns,
    CAP_NET_RAW)` — which is why the measurement splits the two ways it does: `EPERM` inside a bare
    user namespace (the net namespace is still the machine's, whose owner is the initial one) and
    **served** inside a user namespace made *with* `CLONE_NEWNET` (its owner is that user
    namespace). This kernel asked the caller's own, so the bare case was served where Linux
    refuses. **Landed 2026-10-18** (§5.1 item 42, the same `owner` table): the door asks
    `ns_capable_of(owner_of(current_net_ns))`, and `t-ns-net` pins both halves in one child — the
    whole set held inside a bare user namespace still hears `EPERM`, and the same caller opens the
    same socket in a namespace it made.
  - **`CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` ask the *file's* filesystem's user namespace**
    - `capable_wrt_inode_uidgid` - and they are the reason a check is not "is the
    caller root". **Landed 2026-10-17** (`docs/uapi/` §5.1 item 39). Measured whole on a current
    host first (`.workbuddy/probe_capable_scope.sh dac`, root with each capability dropped from the
    effective set in turn, against mode `0000` and `0444` fixtures): `CAP_DAC_OVERRIDE` grants a
    read *and* a write on a non-directory **whatever its bits are** - the clause that required some
    triad to carry the bit is gone from Linux, so a `chmod 000` file is readable and writable by a
    caller holding it, where this kernel refused both - and grants execute only where some execute
    bit exists (`S_IXUGO`), which is what makes `access(X_OK)` on a `0644` file `EACCES` even for
    root and the compound `R_OK|W_OK|X_OK` refused where `R_OK|W_OK` is granted;
    `CAP_DAC_READ_SEARCH` grants a read on its own and everything on a directory but a write; and a
    caller whose credentials are *not* in the file's namespace - uid 0 mapped to the outside root,
    the probe's fourth block - is refused by both. **The rest of the family landed 2026-10-18**
    (`docs/uapi/` §5.1 item 40) - the *owner-shaped* calls asked "is the caller root or the owner"
    where Linux asks `inode_owner_or_capable` = `fsuid == owner || capable_wrt_inode_uidgid(
    CAP_FOWNER)`, and `chmod`'s set-group-id rule asked `uid 0` where Linux asks
    `in_group_or_capable(.., CAP_FSETID)`. Both are measured the same way and were their own pass,
    which is what this entry said; it is done now, together with `CAP_CHOWN` (which `chown`'s two
    arms ask) and the discovery that the timestamps call is *two* questions rather than one
    (`ATTR_TOUCH` accepts write permission, `ATTR_TIMES_SET` does not). The two object-shaped
    scopes above — the namespace being joined and the network namespace a packet socket is opened
    in — landed together on 2026-10-18 (§5.1 item 42), because they are one question: whose the
    *object* is. **What stays named** is the same capability's third door, below.

- **A write to a set-uid file leaves the bit, and `CAP_FSETID` is what would have taken it.**
  **Landed 2026-10-18** (`docs/uapi/` §5.1 item 41). Linux's
  `file_remove_privs` runs the same two bits off a **write** and a **truncate** as it does off a
  `chown`, which is the whole reason a set-uid program cannot be handed to somebody by editing it
  in place - and it is `CAP_FSETID` that spares them, so this is the same capability's third door.
  Measured on a current host (`.workbuddy/probe_suid_write.c`, Linux 7.2.9, a root caller writing
  one byte to fixtures carrying the bits): with the whole set held a write to `04755` leaves
  `04755` and one to `02755` leaves `02755`, and with `CAP_FSETID` dropped alone both come back
  storing **`0755`** - the set-uid bit **and** the set-group-id bit, including the set-group-id of
  a file in a group the caller is *not* in, which is where this door differs from `chmod`'s (there,
  being in the group spares the bit; here only the capability does). A `truncate` to zero does the
  same thing, because it is the same helper. The one shape the rule leaves alone is the
  set-group-id with no group execute - `02644` stays `02644` - the same locking-mark clause a
  `chown` honours and a `chmod` does not. This kernel's write path touched neither bit, so a caller
  that dropped the capability kept an authority Linux would have taken away. It asks now, through
  `crate::vfs::remove_privs` (and the `without_set_id` two clauses it shares with item 40's `chown`
  door), at the four doors Linux reaches: `File::write`, `ftruncate`, `truncate` and an `open`'s
  `O_TRUNC`. **One shape stays named**: a write through a mapping, which Linux does not strip
  either. The other one - `fallocate`'s resize, which reached `Inode::truncate` without passing any
  of those doors - is measured and closed one bullet down (§5.1 item 52), and it turned out to be
  wider than a resize: the flag question and the clocks were missing on every successful call, the
  ones that grow nothing included.

- **`fallocate` is a write even when it writes no byte: it costs the file its set-id bits and
  stamps its clocks. Landed 2026-10-18** (`docs/uapi/` §5.1 item 52). Item 41 gave the write path
  Linux's `file_remove_privs` at four doors and named this one as unmeasured. Measuring it widened
  the question: Linux's `vfs_fallocate` ends **every** successful path in `file_modified`, and that
  is three facts at once - the set-id bits go unless `CAP_FSETID` spares them, `mtime`/`ctime`
  move, `IN_MODIFY` is reported - while the immutable flag refuses the call before anything is
  allocated. This door reached `Inode::truncate` directly, so it passed none of them, and the two
  paths that grow nothing (`KEEP_SIZE`, a range already inside the file) passed nothing at all
  because they never reached the truncate. Measured whole on a current host first
  (`.workbuddy/probe_fallocate_mode.c`, Linux 7.2.9, on ext4 under `/home` and on tmpfs under
  `/tmp`, the two agreeing):

  * **the bits go on every successful call, not only on a resize**: a `4755` file stores `0755` for
    a grow (A), a `KEEP_SIZE` that leaves the length at 4 (B), a range starting past the end (C),
    a `KEEP_SIZE` past the end (D) and a range already inside the file (E), and a `2755` set-gid
    file stores `0755` too (F). The rule is the call's, not the size change's;
  * **the clock moves on the same set and on no other**: with `mtime` pinned to `1000000000` by
    `utimensat`, all six successful calls come back reporting the current second, and a refused one
    (J: `EPERM` on an immutable file) leaves both the pinned second and the 4-byte size exactly
    where they were - so the stamp belongs to the success arm, not to the call;
  * **the capability is what spares the bits, and only them**: as root, which holds `CAP_FSETID`,
    the same grow leaves `4755` and *still* moves `mtime` (H). Two rules, two questions;
  * **the immutable flag refuses this door, and the append-only flag does not**: a descriptor
    opened before `chattr +i` answers `EPERM` with the size unchanged (J), while an append-only
    file serves both the growing range and one inside the file (I, K). That is
    `crate::vfs::may_modify` and **not** `may_alter`, which would have refused the `a` file Linux
    serves - the measurement is the whole reason the narrower predicate is the right one.

  Implemented as a flag question where Linux asks it (after the descriptor's write mode, before the
  kind) and one helper at both success arms: `fallocate_modified` runs `remove_privs`, then
  `set_times(atime, now)` - the `ctime` rides with it, as `set_times` promises - then the
  `IN_MODIFY` event, which is `file_modified`'s order.

  Pin: **13 claims** - `t-fallocate` 9 (the pinned clock, the allocation that cannot change the
  length, the moved `mtime`, the length that did not move, the re-pin, the refused `EINVAL` and its
  untouched clock), `t-caps` 2 (the set-id bits with `CAP_FSETID` dropped, beside the whole set's
  control) and `t-fsflags` 2 (the `EPERM` and the size it left alone). Host: every one of the nine
  `t-fallocate` claims green on a current host under `MPOS_ON_HOST=1`; the `t-caps` and
  `t-fsflags` ones are the gated kind (a dropped capability, an immutable file), and both files
  name that and stop on an unprivileged host - which is what the sweep, running as root, measures.
  Gate: x86_64 `pass=183 fail=0 (183 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33; aarch64 `pass=182 fail=0 (182 built)`, kunit 32/0; both machines' `ok:` lines for the three programs identical claim by claim (diffed, carriage returns aside). Counterfactual (both halves put back - no flag question, no
  `fallocate_modified`): stage 1 unchanged (`565 passed; 0 failed` - no unit test asks this question, and the writers are not what moved), and the sweep `pass=180 fail=3 (183 built)` with `FAIL t-caps`, `FAIL t-fallocate` and `FAIL t-fsflags`, **three** red claims by name: `FSETID dropped: an allocation costs the same two bits (got 4755)`, `an allocation moves mtime off the pinned second`, and `fallocate is EPERM (got errno 0, want 1)`. The other two new claims are controls and saying so is the point: `whole set: and so does an allocation` holds on the old kernel because root keeps the bits either way, and `and the file did not grow` holds because the request was `KEEP_SIZE`, so the size never depended on the fix.

- **A namespace descriptor *is* the namespace, and the pin is what makes `setns` a
  join. Named 2026-10-18, landed the same day** (`docs/uapi/` §5.1 item 44;
  `.workbuddy/probe_ns_fd_pin.c` for the pin, `.workbuddy/probe_ns_fd_stat.c` for
  the file's shape, Linux 7.2.9). Linux's `/proc/<pid>/ns/<kind>` is a procfs *link*
  whose target is an `nsfs` inode - opening it resolves the object **once**, and the
  descriptor names that namespace for as long as it is held. That is what makes
  `setns(fd)` a join rather than a question about where the maker stands now, and
  what lets a namespace outlive the process that made it. Measured as the probe's
  three readlinks - `4026531832`, the copy's `4026532932` after an `unshare`, then
  `4026531832` again - and as the file's own shape: `lstat` reports the link
  (`symlink 0777`, the procfs inode's own number), `stat` and `fstat` report a
  **regular `0444` file of size 0 whose `st_ino` is the namespace's own number**
  (two names of one namespace agree on it), `read` is `EINVAL`, `lseek` `ESPIPE`,
  and a write-ish open (`O_WRONLY`, `O_RDWR`, `O_TRUNC`) is `EPERM` for root and
  unprivileged callers alike, while `O_APPEND` alone is served. This kernel handed
  the *link* back for the opened descriptor, so `setns` re-resolved the `(pid,
  entry)` pair and the same three readlinks answered the copy's number twice - and
  `stat` of a namespace name reported the link rather than the file it is followed
  to. Both are the same missing thing, closed in one pass: `vfs::ns::NsFile` is the
  file the walk now denotes for a followed namespace name (owner: `src/vfs/ns.rs`,
  the substitution in `src/vfs/mount.rs`'s walk, the doc contract on
  `Inode::as_namespace`), with `readlink`/`lstat` untouched because they stop at the
  link, which is the split Linux makes. Pin: `t-ns-setns`' descriptor section - the
  five discriminating claims are the two shapes, the inode number, the write-ish
  refusal, and the join landing in the descriptor's namespace rather than the copy -
  beside the host unit test `an_opened_namespace_descriptor_has_the_shape_a_host_measured`.

- **A directory's `read(2)` answers `EOPNOTSUPP` where Linux answers `EISDIR` — and
  `sendfile(2)` from one answers `EOPNOTSUPP` where Linux answers `EINVAL`. Landed 2026-10-18**
  (`docs/uapi/` §5.1 item 47). Measured on a current host first (Linux 7.2.9, `read(2)`
  through `ctypes`: `-1`, errno **21** `Is a directory`, and `python3`'s `os.read` agreeing;
  `sendfile(1, dirfd, NULL, 16)`: `-1`, errno **22** `Invalid argument`, while the same call
  from a regular file copies the bytes it was asked for) and then in this kernel's own words
  on a booted machine (2026-10-18, `cat` on a directory: `mpos: syscall 0 (fd 3) ->
  EOPNOTSUPP`, `cat: read error: Not supported`, with `mpos: syscall 40 (fd 1) -> EOPNOTSUPP`
  for the `sendfile` attempt that preceded it). Both are the *shape* rule of §3:
  `EOPNOTSUPP` tells a caller "this kind of file cannot be read that way", and Linux's two
  answers tell it *what it named* — a program that probes a path with `read` branches on the
  two differently. The owner of the first was the trait's default (`Inode::read_at` is
  `Err(VfsError::Unsupported)`, and a directory overrides the *listing* door rather than this
  one): Linux's rule lives in the directory's own `f_op` (`read = generic_read_dir`, which
  returns `-EISDIR`), so it is a kind-wide answer, and the landing spelled it once in the door
  every read reaches (`File::read`, after the mode check and *before* the empty-buffer
  short-circuit — measured, a directory's zero-length read is refused too, where a file's is
  `0`). The second is `sendfile`'s own source question (`EINVAL`), asked after its own
  count-0 return (measured: `0`), and the write half needs no twin: a directory cannot be
  *opened* writable at all (`open(dir, O_RDWR)`/`O_WRONLY` are `EISDIR`), so a caller only ever
  meets a read-only description's `EBADF`. The whole table — five read doors, the two
  `sendfile` answers, the open refusals and the controls on a regular file — is
  `.workbuddy/probe_dir_io.c`, and the pin is `t-dir-io` (14 claims) beside the host unit test
  `a_directory_has_no_byte_reader`; item 47 carries the gates and the counterfactual.


- **`rmdir` inside the device tree answered `EOPNOTSUPP`, where Linux's `devtmpfs` removes the
  directory. Landed 2026-10-18** (`docs/uapi/` §5.1 item 48). Measured in this kernel's own words on
  a booted machine (2026-10-18, the first machine's GL card): `mpos: syscall 84 (path
  /dev/dri/by-path) -> EOPNOTSUPP`, which is udev doing what it does to that directory at every boot
  — remove it, then publish the `by-path` links again — while a current host serves the same call
  (measured as root on devtmpfs: `mkdir /dev/<name>` and `rmdir /dev/<name>` both answer 0), which is
  why the host's own database entry carries a `by-path` symlink beside the node (§7.4). The
  asymmetry was this tree's rather than the kind's, and it was wider than the one line that found
  it: `devfs`'s `create` *did* accept a directory (`DevDirNode`, `src/vfs/devfs.rs`), so `mkdir`
  there was served, while neither node had an `rmdir` at all **and** each `unlink` was a plain map
  removal that took a directory as readily as a node — so the two doors were not a split, and the
  trait's default `Unsupported` answered the call udev makes. Both nodes own the door now
  (`check_empty_dir` asks the kind and then the emptiness, *before* the name goes, which is what
  makes a refused `rmdir` leave the directory where it was), `unlink` gained the `EISDIR` it lacked,
  and the mount-point half needed nothing: `may_remove` already refused it with `EBUSY` (§5.1 item
  45). Pin: `t-devtmpfs`'s removal section, **14 claims** (44 in the file, was 30) beside two host
  unit tests over the same table; item 48 carries the measured host table, the gates and the
  counterfactual, and the boot that logged this line is quiet now — `/dev/dri/by-path` published, and
  no `mpos: syscall` line in the whole console. One shape stays named: a regular file and a symlink
  under `/dev` are removable through `unlink`, as on Linux, but a driver's own *node* cannot be
  re-made through `mknod` here — the device tree's names come from the drivers, which is a property
  of the tree rather than of the removal doors.

- **A read of a watched file reports `IN_ACCESS` — at the doors, not inside `File::read`.
  Landed 2026-10-19** (`docs/uapi/` §5.1 item 60). The inotify module's header had carried
  `IN_ACCESS` as a deliberate non-event since it landed, on a rationale that had gone stale:
  "the read path does not bump atime, so the event would be half a lie". The read path *does*
  bump atime now (ext4's `relatime` bump, §3.3), and Linux's answer is unconditional — one
  `IN_ACCESS` per successful read-family or copy call that moved bytes, whatever the stored
  time then says. Measured whole on a current host first (`.workbuddy/probe_inotify_access.c`,
  Linux 7.2.11, one scenario per leaf): a `read` reports one record with an empty name, and so
  do `pread`, a two-iovec `readv` (**one**, not two), `sendfile` (either offset form),
  `copy_file_range`'s source and a file→pipe `splice`; a read at EOF, a zero-length read, a
  failed read, a directory read (`EISDIR`) and an **`mmap` page fault** all report nothing; a
  pipe→file `splice` reports `IN_MODIFY` instead; and a watch that did not ask for the bit is
  silent. Where the emit sits is the measured half: Linux's `fsnotify_access` is at the syscall
  boundary, because the kernel's own readers (`read_file`, the `execve` load) reach
  `File::read` too and `kernel_read` reports nothing — so the owner is the syscall layer
  (`notice_access`, the single call-site helper the three read doors and the three copy doors
  ask) over `vfs::inotify::emit_access`. Pin: `t-inotify`'s 22 new claims (24 → 46) beside the
  host unit test `emit_access_reports_a_read_only_to_a_watch_that_asked`; item 60 carries the
  gates and the counterfactual. The one shape it named as still open — a *write* made by a copy
  door was invisible to `IN_MODIFY` — is closed by the entry below (item 61).

- **A write of a watched file reports `IN_MODIFY` once per call — the write half of the record
  above. Landed 2026-10-19** (`docs/uapi/` §5.1 item 61). The entry item 60 named as still open:
  a `copy_file_range` or a pipe→file `splice` wrote through `Inode::write_at`, below the
  `File::write` chokepoint that reports, so a watcher on the destination heard nothing — and the
  `File::write` comment claiming every copy door reached it "so the event is said once" was
  simply false. Measuring on a current host first (`.workbuddy/probe_inotify_modify.c`, Linux
  7.2.11) found the rule is the one `IN_ACCESS` already follows — *one `IN_MODIFY` per call
  however many pieces carried it*: a two-iovec `writev`/`pwritev`/`pwritev2` reports **one**, and
  so does a 10 KB `sendfile` (three 4 KiB chunks), a 10 KB `copy_file_range` and a pipe→file
  `splice`; the source of a copy is read and hears none; a zero-length write moves nothing; and a
  watch whose mask lacks the bit is not told. Only the scalar `write`/`pwrite` doors already
  reported correctly, through `File::write`. The fix is a `write_quiet` on `File`
  (`write_reporting(buf, false)` — `remove_privs` still runs, only the report is held) that the
  vectored and copy doors write their call through, each saying the one event itself from the
  syscall layer's `notice_modify(fd)`, the mirror of `notice_access`: the `handle_writev` wrapper
  (whose body moved to `writev_inner`, so `writev`/`pwritev`/`pwritev2` share it), and
  `handle_sendfile`/`handle_copy_file_range`/`handle_splice` (the last only when its output is
  not a pipe). Pin: `t-inotify` grows **21 claims** (46 → 67) beside the host unit test
  `write_quiet_moves_bytes_without_reporting_modify`; item 61 carries the gates and the
  counterfactual. The module's other two departures — `IN_UNMOUNT`, and a watched directory's
  `IN_MOVE_SELF` — are unchanged.
