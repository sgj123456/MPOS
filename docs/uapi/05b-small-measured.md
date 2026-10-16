# UAPI §5.2

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

### 5.2 Small, measured, and not yet landed

- **`AT_EMPTY_PATH` names the descriptor where this kernel serves it — and is refused outright
  where it does not.** Measured on a current host (`.workbuddy/probe_at_dirfd.c`, Linux 7.2.9),
  the flag is Linux's way of saying "the descriptor is the target" for an *empty* name, and five
  doors take it. Four are served here and agree with the measurement:
  `newfstatat(fd, "", buf, AT_EMPTY_PATH)` answers `0` and `EBADF` for a descriptor that is not
  open, and `statx`, `fchownat` and `fchmodat2` reach the descriptor the same way (the last two
  then refused `EPERM` by their own policy, which is the proof they got there). The two that do
  not are the gap: **`utimensat`'s empty-name spelling of the descriptor** answers `ENOENT` here
  where Linux answers `0` — this door serves the descriptor only for the *null*-path spelling
  `futimens` uses, and Linux measurably serves both (`utimensat(fd, "", NULL, AT_EMPTY_PATH)`
  answers `0`; the null form is what every caller this kernel has met gives) — and **`faccessat2`
  refuses the flag outright** (`EINVAL`, its vocabulary being the two it serves), where Linux has
  taken it since 5.8 and answers `0` for a directory descriptor and `EBADF` for one that is not
  open. Closing the second is one arm in the flag mask plus the `access_check` it already has; the
  first is one branch beside the null-path one. Owner: `src/syscall/linux/fs/file.rs`'s
  `handle_faccessat2` + `utimensat` (whose own doc names the gap). Pin: none yet — neither door has
  a test of its own (`t-faccessat2` covers the flag word it *does* serve, and there is no
  `t-utimens`), so this is a gap with a name and a measurement and no pin, which is why it stays
  here rather than in §5.1. `docs/notes/` §15.17.
- **`F_GETFL` reports no access mode.** Linux ORs the descriptor's access mode into the word
  `fcntl(F_GETFL)` returns, so a descriptor opened `O_WRONLY` reports `1` (measured on a
  current host); this kernel reports the description's stored status word, whose access bits
  are always zero, and keeps the mode separately (`src/vfs/file.rs`'s `ACC_*`, which the
  read/write `EBADF` refusal reads). A caller that reads `F_GETFL` to learn which way a
  descriptor opens is told `O_RDONLY` for every one of them. Not a §3.2 defect — the answer
  is a zero rather than a wrong number — but a divergence a program can see, and `t-fcntl`
  pins the current answer on **both** machines (S6 batch 11). Closing it means folding the
  access bits into the reported word, which has its own blast radius: `rustix::fs::Dir`
  reuses an `F_GETFL` word for the `openat(fd, ".")` it opens a directory's descriptor with.
  Owner: `src/syscall/linux/fs/file.rs`'s `F_GETFL` arm + the `access` field already there.
  `docs/notes/` §15.16.
- **`waitid`'s record leaves `si_uid` zero.** Linux fills it with the child's
  real uid (measured: 1000 for a child of this shell); this kernel's record
  cannot, because it outlives the task's slot — a `waitid` that finds a retired
  record reads a pid, a code, a group and a status, and the uid was never
  carried. Closing it is one field on `Retired`, the shape `pgid` took when
  `P_PGID` needed one, plus the live path's `credentials_of`. Owner:
  `src/sched/runtime/task.rs` + `src/syscall/linux/proc.rs`. Pin: `t-waitid`
  grows the claim, as `probe_waitid.c` measured the host's answer. This is the
  one field of that record still answered with a zero. `docs/notes/` §15.11
  (batch 18).

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

- **`signalfd(2)` and `SIGCHLD`: one death, two records, and the wrong code.**
  Three divergences, each measured on a current host, all of them on the path the
  compositor-shaped caller this descriptor exists for walks. (a) A child's exit
  lives in one table (`sched::read_sigchld`) and a generated `SIGCHLD` is *also*
  queued when the caller has it blocked (`sched::runtime`'s `wants_sigchld`), so
  a descriptor whose mask names `SIGCHLD` hands over **two** records for one
  death: the queued copy first, then the exit record. Linux's pending queue is
  the one table, so one death is one record. (b) That queued copy's `ssi_code`
  is `SI_USER` (`SigInfo::user`), where Linux's record for a child's exit is
  `CLD_EXITED`, with the status beside it — the divergence `sched::runtime`'s own
  comment measured (`si_code=1`) without implementing. (c) An *unblocked*
  `SIGCHLD` is discarded by Linux (`sig_ignored()`, and `SIGCHLD`'s default
  action is the ignoring one), so a `signalfd` watching it sees nothing until the
  caller blocks it — measured with the pair one child's exit makes: unblocked the
  read answers `-1`/`EAGAIN` (errno 11), blocked it answers the 128-byte record.
  This kernel reports the exit whatever the mask says, which is what let the
  no-std version pass without blocking anything. Owner: `src/sched/runtime/`'s
  `queue_signal` call (the record's code, and the second table) with
  `src/vfs/signalfd.rs`'s `take_signal` (which source wins). `t-signalfd` blocks
  `SIGCHLD` the way its own contract says, claims the *count* of records with
  this kernel's answer on the payload and Linux's under `MPOS_ON_HOST`, and pins
  the exit record's `CLD_EXITED` and pid on **both** machines. `docs/notes/` §15.11
  (batch 21). **Re-measured after the program count grew, and the count had
  drifted to four**: the three extra records named *other* slots (`8`, `10`) and
  the read before the test's own fork already answered one, because an unreaped
  child kept a `parent` that names a **slot** — and a slot is reused, so the next
  process to occupy it was handed the dead process's orphans. Fixed where Linux
  fixes it (`forget_original_parent`: a process that exits gives its remaining
  children to init, `src/sched/runtime/`'s exit path), which also makes this
  entry's "two records for one death" deterministic rather than
  neighbourhood-dependent. `docs/notes/` §6.

- **A UDP send's full buffer answered `EMSGSIZE`, so a burst stopped at four —
  and the blocking half of Linux's rule is not implemented yet, by name.** A
  socket's send buffer is finite, so a burst outruns it; what the caller hears
  then is Linux's question, and Linux's answer has two halves — a *blocking*
  sender waits for room, a non-blocking one is `EAGAIN` — with `EMSGSIZE` kept
  for the datagram wider than the buffer that would have to hold it. This kernel
  had one mapping for both of `smoltcp`'s meanings of `BufferFull` ("wider than
  the buffer" and "no room at this instant") and answered **`EMSGSIZE`** to each,
  so a perfectly ordinary 1400-byte datagram was refused as though it were
  oversized. Measured by `t-netstats`, which sends 256 of them back to back:
  **four were accepted** — the depth of the virtio TX ring — and every send after
  that was refused, so `tx_packets` moved by four and the burst's rate said
  nothing. `src/net/socket.rs`'s `udp_send` now asks which case it is
  (`data.len() > sock.payload_send_capacity()` keeps `EMSGSIZE`) and answers
  **`EAGAIN`** to the momentary full. **The wait is not implemented, and that is
  the named half**: the buffer drains into the device's TX ring, the ring drains
  when the device completes descriptors, and this driver takes no interrupt for a
  completion — so there is no event a blocking sender could wait on, and a park
  there sleeps until something else happens to poll the stack. Both shapes were
  tried and measured: a park with no deadline stopped the sweep dead at
  `t-netstats`'s first parked send, and a tick-bounded park spun at **97% CPU**
  because the wait's own readiness answer (`can_send()`, the metadata ring) was
  true while `enqueue` still refused on the payload ring — a wait that can never
  end. **Follow-up, and the way this closes**: give the TX ring a completion
  wakeup (a TX interrupt, or a wake from the driver's `transmit`) and the blocking
  sender can wait as Linux's does. Until then `t-netstats`'s burst retries on
  `EAGAIN` (a bound, not a tolerance) and prints the retry count as a measurement,
  and its exactness pair — one `sendto` moves `tx_packets` by exactly one and
  `tx_bytes` by exactly the frame's size — is the pin. `docs/notes/` §10.4.

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

- **`FIONREAD` on a regular file and on a pipe.** Linux answers `0` and the bytes
  remaining/queued; this kernel answers `ENOTTY` for everything that is not the entropy
  pair. The `ENOTTY` is an honest "this descriptor has no queue-length story here" rather
  than a wrong number, so it is not a §3.2 defect — but it is a divergence a program can
  see. Closing it needs a per-descriptor byte count (a pipe has one; a file has
  `size - offset`). Owner: `fs.rs` + the node's own `poll` story.
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
