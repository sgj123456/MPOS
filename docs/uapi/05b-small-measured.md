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


- **`CAP_MKNOD` is not checked.** Linux refuses a device node to a caller without `CAP_MKNOD`
  (`EPERM`, measured on a current host through `syscall(2)`, `.workbuddy/probe_mknod.c`), and the
  *order* it refuses in is measured too: after the type's own answers, after the name, after the
  name's existence (`EEXIST` wins), and after the directory's writability (a device node in a
  directory the caller may not write is `EACCES`, not `EPERM`). This kernel has no such gate:
  `handle_mknodat` builds the node for any caller. The impact is small and the reason it is named
  rather than fixed is the same in both halves - a node made here routes nowhere (there is no
  number-to-driver table, the entry above), so the node a caller should not have been able to make
  also cannot do anything; and asking the question is not one line, because a `setuid` here does not
  re-derive the capability sets (`src/syscall/linux/posix.rs`'s `identity_set_one` changes the ids
  and leaves the sets alone), so a dropped-privilege process would still hold `CAP_MKNOD` and the
  gate would answer `EPERM` for nobody. Owner: `src/syscall/linux/fs/file.rs`'s two `mknod` doors,
  once `setuid`'s capability re-derivation lands.

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

  (d) **The link's own metadata is not Linux's.** `lstat` of the name (and `readlink`) is the door
  that *should* stop at the link, and it does — but `ProcLink` reports the trait's `mode()` of `0`
  and its own `size: 0`, where Linux derives the mode from the descriptor's *direction* — measured
  `0700` for `O_RDWR`, `0300` for write-only, `0500` for read-only, and a directory's descriptor
  `0500` — beside `st_size` 64. A listing tool that colours or filters by the link's mode reads a
  different one. Owner: `src/vfs/procfs.rs`'s `ProcLink::metadata`/`mode`.

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
  clears it. What is left is the *rest* of `filp->f_flags`, and it is measured rather than
  recalled: on a current host through `syscall(2)` (`.workbuddy/probe_getfl.c`, Linux 7.2.9),
  `open(O_RDONLY)` reports `0x8000` and `open(O_WRONLY)` `0x8001`, where `0x8000` is
  `O_LARGEFILE` - the kernel *adds* that bit to every path open, and machine-dependently
  (`0o100000` on x86_64, `0o20000` on arm64, where this repository's own uapi table deliberately
  defines neither). Beside it the open keeps what it was asked for:
  `open(dir, O_RDONLY|O_DIRECTORY)` reports `0x18000`, `open(O_RDONLY|O_NOFOLLOW)` reports
  `0x28000` - `O_NOFOLLOW` *is* reported, so the bit is not open-only on Linux, which
  `src/syscall/linux/abi.rs`'s comment about it used to say it was - `open(path, O_PATH)` reports
  `0x200000`, and `O_SYNC`/`O_DSYNC`, `O_ASYNC`, `O_DIRECT` and `O_NOATIME` each come back the
  same way. This kernel stores none of them: its word is the access mode plus `O_APPEND` and
  `O_NONBLOCK`. A caller that compares the whole word against what it opened with therefore sees
  a difference - the shape `rustix::fs::Dir` would meet if it reopened a directory from the word
  it read, though for the `O_RDONLY` dirfd it reads those extra bits are as harmless here as they
  are on Linux. Owner: `src/vfs/file.rs`'s `SETFL_MASK` and the `F_GETFL` arm, whose
  answer for `O_LARGEFILE` is a cross-machine decision (`t-open-flags` pins that this kernel
  ignores the flag on both machines today). `docs/notes/` §15.16.
- **`__WCLONE` is accepted and ignored, where Linux narrows with it.** Measured
  on a current host: `waitid(P_ALL, 0, &si, WEXITED|__WCLONE)` answers `ECHILD`
  (errno 10) for a child that exits with `SIGCHLD`, because the bit means
  "children cloned with an exit signal *other* than `SIGCHLD`" — and `__WALL` is
  the word that asks for both. This kernel accepts the bit and reports the child
  anyway, which is the treatment `wait4` has given all three Linux-only bits
  since it landed (`.workbuddy/probe_waitid.c` also measured `__WALL` and
  `__WNOTHREAD` served and a *undefined* bit, `0x10000000`, refused `EINVAL`).
  Prerequisite: the clone exit signal would have to survive on the task —
  `validate_clone_flags` reads it out of the flag word and drops it. Owner:
  `src/sched/runtime/` + `src/syscall/linux/proc.rs`. Pins: `t-waitid`,
  `t-wait4`. `docs/notes/` §15.11 (batch 18).

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

- **A terminal's foreground group, and a pty's number: two conventions this kernel
  answers its own way.** (a) *Whose signals a terminal carries.* Linux keeps a
  terminal's foreground group strictly as session state: it is set when a session
  leader makes the tty its controlling terminal (`__proc_set_tty`) or by
  `TIOCSPGRP` from that session, and a terminal no session holds has none -
  measured on a current host, `TIOCSWINSZ` on a fresh pair's master runs no
  handler at all (five setups probed: nothing claimed, `tcsetpgrp` without a
  session, a read first, and a session leader with the slave as its ctty - only
  the last two deliver, and the third only on this kernel), `TIOCGPGRP` on its
  slave answers `-1`/`ENOTTY` (errno 25) and `tcsetpgrp(slave, getpgrp())` is
  refused with the same number. This kernel answers "the reader": `Tty::read`
  records the reading task's group as the terminal's foreground
  (`src/vfs/tty.rs`, "the same answer for a program that never asks"), and
  `TIOCGPGRP` reports zero rather than refusing when nobody owns the terminal
  (`src/syscall/linux/fs/ioctl.rs`'s `0x540F`/`0x5410` arm). A program that reads
  a tty it never claimed therefore *receives* the typed `SIGINT` and the resize
  `SIGWINCH` here, where Linux would aim at nobody, and a supervisor asking
  `tcgetpgrp` on a pair no session holds reads `0` here and an error there.
  Owner: `src/vfs/tty.rs`'s `read`/`foreground` and `src/sched/runtime/`'s ctty
  bookkeeping. `t-pty` pins the *delivery* on **both** machines by giving the pair
  a session the ABI's way - a getty's `setsid` + open + `TIOCSCTTY` - and names
  this rule in its comment instead of pinning it. (b) *The number `TIOCGPTN`
  mints.* This kernel numbers its pairs past the six virtual consoles
  (`src/vfs/pty.rs`'s counter starts at `VC_COUNT + 1`); Linux's `devpts` numbers
  from 0 - measured on a current host, which answered 5. Both name a real slave,
  so no caller is misled, but a program that hard-codes a first number does not
  meet it here. `t-pty` pins this kernel's answer on the payload and accepts the
  host's under `MPOS_ON_HOST`, printing what the host said. `docs/notes/` §15.11
  (batch 22).

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
- **`/proc/<pid>/smaps` and `numa_maps`.** `maps` is served in Linux's exact format; the
  two siblings are unwritten. Owner: `src/vfs/procfs.rs`.
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

- **The two wait doors' validation order, and where a bad `rusage` pointer leaves the record.**
  **Landed 2026-10-17** (`docs/uapi/` §5.1 item 34): both doors fill the caller's `struct rusage`
  now, for every event they report. Two corners of that are Linux's shape and are *not* built, and
  they are one sentence each because they are visible only to a caller that passes a pointer the
  kernel cannot write to. (i) **`waitid` clears its `siginfo_t` before it waits**, where Linux
  validates and writes at the end: with a bad `rusage` pointer this kernel leaves a *zeroed* record
  where Linux leaves the caller's own bytes — the refusal itself (`EFAULT`, record untouched) is
  right, only the record's contents differ. (ii) The same clearing means a bad `infop` is `EFAULT`
  *before* the wait, where Linux waits first and refuses after: a caller that asks for an event with
  an unwritable record gets its error either way, and in one of the two this kernel has not yet
  collected the child Linux would have. Both come from clearing up front, which is what makes the
  three refusal paths (§5.1 item 17) leave the zeroed record Linux leaves, so the fix is a
  restructure of the record path rather than a line — named here rather than approximated. Owner:
  `src/syscall/linux/proc.rs`'s `handle_waitid`. Measured: `.workbuddy/probe_wait4_rusage.c`
  scenarios O (a bad `rusage` leaves the `siginfo_t` untouched, `0xAAAAAAAA`) and the same probe's
  ordering finding for `wait4`'s `*status`.
