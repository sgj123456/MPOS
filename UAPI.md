# MPOS UAPI — the userspace-visible surface, and what is left

`PLAN.md` is a phase roadmap: each section narrates one landing, in order, with its
measurements beside it. This file is the other view of the same truth — an **inventory**,
one row per promise the kernel makes to a binary it did not compile, so that the review
question every change here gets asked can be asked of the whole surface at once:

> *Is that Linux's answer, or a shape that looks like it?* (`AGENTS.md` §7)

"UAPI" is Linux's own word for this (`include/uapi/`): the part of the kernel that is a
contract. Here that is, concretely:

- **Syscall numbers and their argument/return contract** — `src/syscall/linux/nr.rs`,
  `src/syscall/linux/mod.rs`.
- **Device `ioctl` request words** — one word per promise, dispatched in
  `src/syscall/linux/fs.rs` and served by the device's own module.
- **Structure layouts crossing the boundary** — `struct stat`, `rt_sigframe`, `edid`,
  `sockaddr_ll`, `struct sched_attr`, …
- **Flag words and closed vocabularies** — `mmap`'s `MAP_*`, `madvise`'s advices,
  `clock_gettime`'s ids, `memfd_create`'s flags, `statx`'s flags, `prctl`'s options.
- **Wire and protocol encodings userland parses** — netlink attributes, `sockaddr_ll`,
  the BOOTP/packet frames a DHCP client reads.

A change to any of these is append-only (`AGENTS.md` extending rule 3): a number, a layout
or an encoding is a contract once it ships, because a running musl binary reads it at its
own address.

## How to read a row

Every entry below carries one of three verdicts, and the difference matters more than the
count of rows:

| Verdict | Meaning | Is it work? |
|---|---|---|
| **Served** | Behaves as Linux does, and a pin would fail without it | No |
| **Refused** | `ENOSYS` / `EOPNOTSUPP` / `EINVAL` / `EPERM` — Linux's own answer for a kernel that cannot do the call (`AGENTS.md` §4.2) | No. A refusal is the honest direction to be wrong in |
| **⚠ wrong answer** | Accepted, answers success, and does less than Linux — or an errno/shape Linux would not give. The `ENOSYS`-free §4.2 failure class | **Yes.** These are the only defects |

A refusal is listed rather than hidden because a board is only useful if it is true
(`AGENTS.md` §8). A missing feature with a named prerequisite is not a defect; a
success-for-a-thing-not-done is.

---

## 1. The syscall table

`src/syscall/linux/nr.rs` names **252** x86_64 syscall numbers. The dispatch in
`src/syscall/linux/mod.rs` has an explicit arm for the **~230** the kernel implements, plus
a `_ => ENOSYS` catch-all for the rest. That catch-all is deliberately crowded and is an
honest answer (`AGENTS.md` §4.2): a libc probes optional calls by number and reads `ENOSYS`
as "not available here".

### 1.1 Deliberately `ENOSYS` — refused, pinned

`user/abitest/src/bin/syscall/t-enosys.rs` walks these and asserts `ENOSYS`, plus a console
note printed once per number (`note_enosys`):

| Syscall | Number | Why refused | Prerequisite to serve |
|---|---|---|---|
| `keyctl` | 250 | Kernel keyring | A key store and the `KEYCTL_*` command set |
| `fanotify_init` | 300 | Filesystem-wide event notification | A fanotify watch/mark engine and permission events |
| `fanotify_mark` | 301 | " | " |
| `userfaultfd` | 323 | Userspace page-fault handling | Fault forwarding to a user task + `UFFDIO_*` |
| `io_uring_setup` | 425 | Async I/O rings | A ring ABI, SQ/CQ, registration — deferred by design (§1.3) |
| `io_uring_enter` | 426 | " | " |
| `io_uring_register` | 427 | " | " |

> The numbers for `fanotify` above are the header's (`300`/`301`), and `PLAN.md`'s board
> entry that says `302`/`303` is a typo for them — `t-enosys` pins `300`/`301`, which is
> what a caller sends.

`rseq` (334) is the eighth: like the seven above it has no dispatch arm, but unlike them it
*once had* one that returned `0` without writing any of the fields the ABI contract puts in
the caller's block. §4.2 ranks that shape worst, so the arm was removed; the number stays
named (append-only) and `t-rseq` pins `ENOSYS`. Serving it needs a per-thread registration
whose `cpu_id`/`node_id`/`mm_cid` are maintained across preemption, plus
`RSEQ_FLAG_UNREGISTER` — a scheduler feature (`PLAN.md` A6).

### 1.2 Served with a named sub-gap

Not refusals, not wrong answers: the call is real, and one documented corner of Linux's
contract is not. Each is tracked in its phase section; the list is here so it is countable.

| Syscall | Sub-gap | Where |
|---|---|---|
| `wait4` / `waitid` | the park is fatal-signal-only, so a catchable signal does not end it with `EINTR` under `SA_RESTART` (Linux's is `ERESTARTSYS`-restartable) | `PLAN.md` §2.5 |
| `ioctl` | no per-request restart verdict: an interrupted request comes back `EINTR` even under `SA_RESTART` where Linux rewinds `DRM_IOCTL_SYNCOBJ_WAIT` and most others | `PLAN.md` §2.5 |
| `rt_sigaction` | numbers `32..=64` have no disposition storage (`EINVAL`); `SIGRTMIN` (32) alone is queueable/maskable | `PLAN.md` §2.3 |
| `rt_sigqueueinfo` / `sigqueue` | `33..=64` have no queueable number, so only `SIGRTMIN` carries a payload | `PLAN.md` §2.3 |
| `mmap` | `MAP_HUGETLB` is `ENOMEM` (an honest refusal); `MAP_HUGETLB` backing is the missing half | §3 below |
| `madvise` | `MADV_FREE` and a shared mapping's cross-process drop are deliberate no-ops | `PLAN.md` §5.2 |
| `posix_fadvise` | recognised advices are accepted as the no-ops they are (this kernel prefetches nothing) | `PLAN.md` §12.1 |
| `vhangup` | with a controlling terminal it answers `EOPNOTSUPP`: the tty re-init + `EIO` hang-up is not implemented | `PLAN.md` §12.2 B4 |
| `getpriority` / `setpriority` / `sched_setattr` | the `nice` round-trips truthfully but carries no scheduling-weight effect | `PLAN.md` §12.2 B5/B5b |
| `setxattr` family | arguments validated, then `EOPNOTSUPP`: no filesystem here stores xattrs | `PLAN.md` Remaining ABI |
| `ptrace` | `PTRACE_EVENT_SECCOMP` refused (no tracer stop); `PTRACE_SETSIGINFO` validates then `EOPNOTSUPP` (no siginfo carried through a stop) | `PLAN.md` §8.2, §12.1 A7 |
| `unshare` / `setns` | UTS served; MNT/PID/NET (and USER/IPC/CGROUP/TIME) refused `EOPNOTSUPP`; `setns` cannot name a namespace by descriptor | `PLAN.md` §6.3 |
| `process_vm_readv` / `writev` | served for the address transfer, and now gated by `ptrace_may_access` — a caller must share a uid with the target (or hold `CAP_SYS_PTRACE`) and the target must be dumpable. Linux's group-id comparison and its LSM hook are not modelled | §5.1 |
| `/proc/<pid>/maps` | served; an image's and a private file mapping's line carries no `offset`/`inode`; `smaps`/`numa_maps` are not written | `PLAN.md` §7.4 |

---

## 2. Device `ioctl`s, by family

Dispatched on the whole request word (magic byte plus command), never on the command byte
alone — the one time that was done wrong, `RNDADDENTROPY` (`'R'` nr 3) was answered as the
drain (`'R'` nr 4) and OpenRC's `seedrng` failed to seed the boot (`PLAN.md` §12.1 A8).

| Family | Device(s) | Owner module | Verdict |
|---|---|---|---|
| `'R'` (0x52) | `/dev/random`, `/dev/urandom` | `src/syscall/linux/fs.rs`, `src/rand.rs` | Served, measured: `RNDGETENTCNT`, the `RNDADDENTROPY` credit, `RNDRESEEDCRNG` (a real re-key) and `FIONREAD` (`EINVAL`, as Linux); `RNDZAPENTCNT`/`RNDCLEARPOOL` refused `EOPNOTSUPP` (no drainable count). Both former gaps closed — §5.1 |
| `'E'` (0x45) | `/dev/input/eventN` | `src/evdev.rs` | Served, measured: `EVIOCREVOKE` one-way NULL-arg, `EVIOCGPHYS`/`UNIQ` `ENOENT` for an absent string, `EVIOCSFF`/`RMFF` `ENOSYS`, keycodes `EINVAL`, `REP` stored gated on `EV_REP`, `GKEY/LED/SND/SW` return counts |
| `'U'` (0x55) | `/dev/uinput` | `src/uinput.rs` | Served; gaps: no `UI_ABS_SETUP`, no force feedback, no `/sys/class/input` entry (sysfs is built once at boot) |
| `'V'` (0x56) | virtual consoles | `src/vfs/vtmode.rs` | Served: `VT_SETMODE`/`VT_PROCESS` negotiation, `VT_ACTIVATE` non-blocking, `VT_WAITACTIVE` sleeping, `VT_SENDSIG` → `ENOTTY` (measured) |
| `'K'` (0x4B) | virtual consoles | `src/syscall/linux/fs.rs` `kd_ioctl` | **Refused as a closed set**: five requests served (`KDSETMODE`/`KDGETMODE`/`KDSKBMODE`/`KDGKBMODE`/`KDGKBTYPE`), the rest of the block `EOPNOTSUPP`, outside it `ENOTTY`. Per-device *serve* from live console state is the Phase-11 B2 move (`PLAN.md` §12.1 A2) |
| `'b'` (0x62) | dma-buf | `src/syscall/linux/fs.rs` | Served (`SYNC` as the documented coherent-memory no-op, `SET_NAME` a debug label) |
| `'d'` (0x64) | `/dev/dri/card0`, `renderD128` | `src/drivers/drm.rs` + `src/drm_*.rs` | Served across the compositor surface — **except the `DRM_VIRTGPU_*` block (`0x41..=0x4b`), which falls to this card's `ENOTTY` tail and is the one *wrong answer* left here.** Linux registers that family on *every* card and answers it per request, measured on `virtio-gpu-pci`: `GETPARAM(1)` `0`, `MAP`/`WAIT`/`TRANSFER_TO_HOST` succeed, `GET_CAPS`/`EXECBUFFER`/`TRANSFER_FROM_HOST` are `ENOSYS`, `CONTEXT_INIT`/`RESOURCE_CREATE_BLOB`/a 3D `RESOURCE_CREATE` are `EINVAL`. Instrument and table: `user/staging/virtgpu-probe/`; `PLAN.md` §7.4. Other §7.4 gaps: timeline syncobj behaviour, `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` |
| `'P'` (0x50) | `/dev/dsp` | `src/drivers/virtio/sound.rs` | Served; the family's rate/channels/format belong to the writer |
| `'t'` (0x74) + `0x8024_7009`/`0x4024_700A` | `/dev/rtc0` | `src/drivers/rtc_dev.rs` | Served (`RTC_RD_TIME`/`RTC_SET_TIME`); alarm/interrupt requests `ENOTTY` |
| `0x12` (block) | `/dev/vda`, `/dev/sda` | `src/blk/`, `src/syscall/linux/fs.rs` | Served; the size trio's buffer direction was the sole outlier and is fixed (§5.1) |
| `0x89xx` (`SIOC*`) | `eth0` | `src/net/rtnl.rs` (dispatched in `fs.rs`) | Served (`SIOCGIFFLAGS`/`SIOCSIFFLAGS`/`SIOCSIFMTU`/`SIOCGIFINDEX`/`SIOCGIFTXQLEN`/`SIOCGIFHWADDR`) |
| `0x4600`+ (`FBIO*`) | `/dev/fb0` | `src/drivers/video/` | Served (`FBIOGET_VSCREENINFO`/`GET_FSCREENINFO`/`PAN_DISPLAY`/`WAITFORVSYNC`); `FBIOPUT_VSCREENINFO` refused rather than faked |
| `0x54xx` (termios/`TIOC*`) | ttys, pty pairs, consoles | `src/vfs/tty.rs`, `src/vfs/pty.rs` | Served: `TIOCGWINSZ`/`TIOCSWINSZ`, `TIOCGPGRP`/`TIOCSPGRP`, `TIOCGSID`, `TIOCGPTN`, `TIOCSPTLCK`, `TCGETS`/`TCSETS`, `FIONBIO`, `FIONREAD` — the entropy pair answering `EINVAL` as Linux does, and a regular file/pipe keeping the honest `ENOTTY` (§5.2) |

Anything not named here falls to the `_ => ENOTTY` tail — Linux's own answer for a request
a device does not implement.

---

## 3. Flag words and closed vocabularies

`AGENTS.md` §12.1 records a swept pass over these, one landing per vocabulary. A flag the
kernel accepts and drops is a promise not kept, and a *number* it accepts and ignores is the
same failure in a different type.

| Vocabulary | Where | Verdict |
|---|---|---|
| `sched_*` flags, `pipe2`, `dup3`, `epoll_create1`, `timerfd_create`, `inotify_init1`, `renameat2`, `openat2`, `preadv2`/`pwritev2`, `splice`, `msync`, `clone`, `signalfd` | `src/syscall/linux/` | Served: unknown bits `EINVAL` |
| `eventfd2` | `src/vfs/eventfd.rs` | Served: unknown bit `EINVAL`, `EFD_CLOEXEC` applied |
| `unlinkat` | `fs.rs` | Served: unknown bit `EINVAL` and the file survives |
| `madvise` advices | `abi::madvise_advice_is_known` | Served: Linux's exact known set, measured |
| `posix_fadvise` advices | `fs.rs` | Served: `0..=5` known, `EINVAL` past, pipe `ESPIPE` |
| `clock_gettime`/`clock_nanosleep` clock ids | `proc.rs` | Served: `{0..=9,11}` accepted, `EINVAL` past |
| `memfd_create` flags | `fs.rs` | Served: unknown `EINVAL`, `MFD_HUGETLB` `EOPNOTSUPP` (no hugepage backing) |
| `getrandom` flags | `abi.rs` | Served: three `GRND_*` accepted, unknown `EINVAL` |
| `statx` flags | `abi.rs` + `fs.rs` | Served: unknown `EINVAL`, `AT_SYMLINK_NOFOLLOW` honoured |
| `open` `O_NOFOLLOW` | `fs.rs` | Served: `ELOOP` on a symlink final component |
| `mmap` flags | `proc.rs` | Served: `MAP_FIXED_NOREPLACE` → `EEXIST`, conflict `EINVAL`, `MAP_HUGETLB` `ENOMEM`; `MAP_STACK`/`NORESERVE`/`POPULATE` accepted as the no-ops Linux makes them |
| `prctl` options | `abi.rs` + `src/cred.rs` | Served for the name pair, the seccomp pair, `PR_SET/GET_DUMPABLE`, `PR_SET/GET_KEEPCAPS` and `PR_CAPBSET_READ`/`DROP`; every other option is honestly `EINVAL` |

---

## 4. Structure layouts

The layouts the kernel writes into a caller's memory are pinned by host tests in
`src/syscall/linux/abi.rs` and `src/drm_blob.rs`, and by the guest programs that read them
back. This is the list, not the byte-by-byte record (that lives in the tests).

| Layout | Consumer | Pin |
|---|---|---|
| `struct stat` / `statx` | every libc | `t-statx`, `t-fstat`, host `abi` tests |
| `rt_sigframe` + `sigcontext` | `rt_sigreturn` | `t-signal-delivery`, `t-sa-restart` |
| `siginfo_t` | signal handlers | `t-signal-delivery` |
| `struct sched_attr` | `sched_setattr`/`getattr` | `t-priority` |
| `struct rlimit` | `prlimit64` | `t-prlimit64` |
| `struct robust_list_head` | `set/get_robust_list` | `t-set-robust-list` (24 bytes) |
| EDID base block + `IN_FORMATS` blob | a DRM compositor | `src/drm_blob.rs` host tests, verified against `edid-decode`; `t-drm-props` |
| `struct sockaddr_ll` | `AF_PACKET` | `t-dhcp` (no host test — the `net` module is machine-gated, §6) |
| `struct nlmsghdr`/`rtattr`/`ifa*msg` | netlink route/uevent | `t-netlink`, `t-uevent` |
| `struct ifreq` | `SIOC*` | `t-netlink` |
| `struct rand_pool_info` | `RNDADDENTROPY` | `t-ioctl` |
| `struct fb_*` / `fb_var_screeninfo` | `/dev/fb0` | `t-fbdev` |
| `drm_mode_*` | `/dev/dri/card0` | `t-drm`, `t-drm-props`, `t-drm-lease`, `t-drm-sync` |

---

## 5. The work list

Ordered by cost, which is also the order they are worth doing. Every item names its owner
file and the pin that will hold it; an item with no pin says why.

### 5.1 Landed by these passes

Each was measured on this host's Linux before the change (the probe and its output are in
the commit bodies), and each moved its pin in the same commit.

1. **`FIONREAD` (`0x541B`) on `/dev/random` and `/dev/urandom` answers `EINVAL`, not 512.**
   Measured on Linux 7.2.7, root and unprivileged: both entropy devices answer `EINVAL`
   with the caller's word untouched, while a regular file reports its remaining bytes
   (`0`, value 10) and a pipe its queued bytes (`0`, value 5). The old 512 was a
   divergence whose stated reason was false — `seedrng` never asks `FIONREAD`; the binary
   in this image sends exactly one ioctl, the `RNDADDENTROPY` credit, and takes its seed
   length from `/proc/sys/kernel/random/poolsize`. Owner: `src/syscall/linux/fs.rs`.
   Pin: `t-ioctl` (the `EINVAL` line is the counterfactual against the old 512).
   Closes `PLAN.md` §12.1 A8 correction (ii).

2. **`RNDRESEEDCRNG` re-keys the generator for real and answers `0` for root.**
   This generator was seeded once and never re-keyed, so the arm answered `EOPNOTSUPP`;
   Linux answers `0` for root and `EPERM` without `CAP_SYS_ADMIN` (measured, both devices).
   `rand.rs` gains a reseed that re-rolls the ChaCha key from fresh seed material mixed
   with the current stream, so it is never a fixed point. Owner: `src/rand.rs` +
   `src/syscall/linux/fs.rs`. Pin: a host test on the key change, `t-ioctl` (root `0`,
   unprivileged `EPERM`). Closes `PLAN.md` §12.1 A8 correction (i).
   `RNDZAPENTCNT`/`RNDCLEARPOOL` stay `EOPNOTSUPP` for root: there is still no drainable
   count, and answering `0` would be the fake that `RNDGETENTCNT` contradicts.

3. **The block-size ioctl trio validates its destination as writable.**
   `BLKGETSIZE64` (`0x80081272`), `BLKGETSIZE` (`0x1260`) and `BLKSSZGET` (`0x1268`) write
   into the caller's buffer but validated it with `write = false`, so a read-only
   destination was accepted and filled. Measured on a real device: a `PROT_READ` mapping
   is `EFAULT` for all three, as Linux's `put_user` faults it. Owner:
   `src/syscall/linux/fs.rs`. Pin: **none reachable in the gate today** — the self-test
   image boots with no block node, and the host `user_accessible` stub answers accessible
   in both directions — so this is the one landed item that is verified by the change
   alone; the `t-ioctl` `PROT_READ`-buffer pin lands when a block node enters the test
   image (`PLAN.md` §12.1 A9).

4. **A per-process credential model, and the four `prctl` options it makes honest.**
   `src/cred.rs` holds the four capability sets, the dumpable tri-state and `KEEPCAPS`;
   the storage is the thread group's, in `sched`; and the model has **consumers**, which
   is the whole point — the effective set answers `ptrace_may_access`, the bounding set
   is applied by `execve`, a uid/gid change re-derives the sets, and `capget`/`capset`
   round-trip all of it. So `PR_SET_DUMPABLE`/`GET`, `PR_SET_KEEPCAPS`/`GET` and
   `PR_CAPBSET_READ`/`DROP` are answered with the measured Linux contract rather than the
   `EINVAL` they used to share with every unmodelled option. Named divergence: this kernel
   has no file capabilities, so an `execve` can only ever *restrict* the sets — the
   conservative direction, and the reason the derivation is `pP' = pB` for root and
   unchanged otherwise. Pins: thirteen host tests in `src/cred.rs`, `t-prctl`'s new option
   table, and `t-posix`'s `capset`/`capget` round-trip.

5. **`stat(path)` and `fstat(fd)` name the same `(dev, ino)`.** They did not: for one
   and the same file the path door answered `dev=1` and the descriptor door `dev=0`.
   `stat` looks the filesystem up by path and a descriptor cannot, and an inode does
   not name its own filesystem, so the descriptor fallback could only match a mount by
   its *root* inode — which a plain file inside the mount is not. `File` now records
   the filesystem id at `open`, where the path is known, and `fstat` prefers it. This
   is not cosmetic: a lock-and-verify caller (lock the file, confirm the name still
   names the inode you locked, retry if not — what `wayland-backend` does on
   `wayland-N.lock`) compares exactly these two answers, so a disagreement is a retry
   that never converges. It is what `niri` was spinning on (`PLAN.md` §7.4). Owner:
   `src/vfs/file.rs` + `src/syscall/linux/fs.rs`. Pin: `t-stat-consistency`, which
   checks both doors on `/tmp` and on `/run/user/0`, in the order the protocol uses.

### 5.2 Small, measured, and not yet landed

- **`FIONREAD` on a regular file and on a pipe.** Linux answers `0` and the bytes
  remaining/queued; this kernel answers `ENOTTY` for everything that is not the entropy
  pair. The `ENOTTY` is an honest "this descriptor has no queue-length story here" rather
  than a wrong number, so it is not a §4.2 defect — but it is a divergence a program can
  see. Closing it needs a per-descriptor byte count (a pipe has one; a file has
  `size - offset`). Owner: `fs.rs` + the node's own `poll` story.
- **`RNDRESEEDCRNG` provenance.** Now that the reseed is real, `RNDGETENTCNT`'s 4096 stays
  a constant across it; a caller that reseeds and re-reads sees the same figure Linux shows,
  so no change is needed — recorded so the next reader does not "fix" it into a lie.
- **`/proc/<pid>/smaps` and `numa_maps`.** `maps` is served in Linux's exact format; the
  two siblings are unwritten. Owner: `src/vfs/procfs.rs`.
- **`devfs` birth modes.** Devices are born `0666 root:root`; `devtmpfs` births `0600`
  and lets udev widen it. Needs per-class birth modes and group-name resolution, not a new
  mechanism (`PLAN.md` §3.4).
- **The `DRM_VIRTGPU_*` family (`0x41..=0x4b`).** The one *wrong answer* left in §2's `'d'`
  row: this card's catch-all answers `ENOTTY` for every request in it, and Linux answers
  each one differently. Measured on `virtio-gpu-pci` (no 3D behind it): `GETPARAM(1)` is
  `0`, `MAP`/`WAIT`/`TRANSFER_TO_HOST` succeed, `GET_CAPS`/`EXECBUFFER`/`TRANSFER_FROM_HOST`
  are `ENOSYS`, and `CONTEXT_INIT`/`RESOURCE_CREATE_BLOB`/a 3D `RESOURCE_CREATE` are
  `EINVAL`. The work splits in two with different prerequisites: the answers above need
  only the family's own bookkeeping, while `VIRTGPU_PARAM_3D_FEATURES = 1` needs the
  `virtio-gpu` 3D control queue — which the `virtio-drivers` crate does not implement at
  all, and does not negotiate. Instrument and full table: `user/staging/virtgpu-probe/`;
  `PLAN.md` §7.4.

### 5.3 Needs its prerequisite named before it can be honest

- **Real-time signals `33..=64`.** `SIGRTMIN` (32) is queueable, maskable and waitable; the
  rest have no queueable number and no disposition storage. Prerequisite: a 64-bit
  disposition table and a queued-signal list per task (`PLAN.md` §2.3).
- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **`PTRACE_EVENT_SECCOMP`** — carrying a `siginfo_t` through a stop/resume, which the
  stop model does not have (`PLAN.md` §8.2).
- **Real `MSG_PEEK` over TCP/UDP.** Refused `EOPNOTSUPP` today (honest); serving it needs
  peek-preserving delivery in the stack (`PLAN.md` §12.2 B2).
- **`SO_LINGER{on, 0}` and per-socket buffer sizes.** Both are record-only; the first needs
  an unsolicited reset on egress, the second needs buffers not sized once at set
  construction (`PLAN.md` §4.3.3/§4.3.4).
- **`AF_PACKET` `SOCK_RAW`** (frames with their header) — `EOPNOTSUPP`; needs the header
  passed through untouched (`PLAN.md` §4.5).
- **A named `SOCK_SEQPACKET` `AF_UNIX` connection** carries no message boundaries — served
  as the byte stream this family has (`PLAN.md` §4.1).
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`PLAN.md` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).

### 5.4 The structural work, parked with Phase 11

These are not UAPI defects; they are the refactors that keep the UAPI surface honest as it
grows, and each is named so it is not rediscovered as a shape.

- **B2 — each device's `ioctl` contract moves down to its driver.** The `syscall/linux/fs.rs`
  switchboard is the V-2 edge the architecture audit flags, and the `'K'`/`'E'` families
  that are refused "until B2" are refused *because* of it (`PLAN.md` §11.2, §12.1 A2/A3).
- **The `net` module's `target_os = "none"` gate** means `AF_PACKET`'s parsers and the
  `sockaddr_ll` decode have no host test and no fuzz target. Closing it moves the
  wire-format halves to an ungated pure module (`PLAN.md` §4.5, `ARCHITECTURE.md` V-9/V-11).

---

## 6. Where the pins live

- **Host unit tests** — `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`.
  Pure encodings, flag verdicts, struct sizes (`src/syscall/linux/abi.rs`,
  `src/drm_blob.rs`, `src/evdev.rs`, `src/net`'s ungated halves).
- **Guest conformance programs** — `cargo xtask test`. `user/abitest/src/bin/syscall/t-*.rs`
  (one syscall family) and `user/abitest/src/bin/integration/t-*.rs` (a device, the
  network, a subsystem end to end), plus the `std`/`rustix` programs in
  `user/tests/src/bin/`.
- `tests.toml` decides which of them the default sweep runs. A behaviour a machine can only
  show where a device or the host's network is present is pinned there or not at all — the
  rule is that a test which cannot work is deleted or made to work, never `#[ignore]`d
  (`AGENTS.md` §3).

The gate's latest measured state is quoted from its own summary line in `PLAN.md` §12.4a;
this file deliberately carries no counts of its own.
