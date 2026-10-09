# MPOS UAPI — the userspace-visible surface, and what is left

`NOTES.md` is a phase roadmap: each section narrates one landing, in order, with its
measurements beside it. This file is the other view of the same truth — an **inventory**,
one row per promise the kernel makes to a binary it did not compile, so that the review
question every change here gets asked can be asked of the whole surface at once:

> *Is that Linux's answer, or a shape that looks like it?* (`AGENTS.md` §6)

"UAPI" is Linux's own word for this (`include/uapi/`): the part of the kernel that is a
contract. Here that is, concretely:

- **Syscall numbers and their argument/return contract** — `src/syscall/linux/nr.rs`,
  `src/syscall/linux/mod.rs`.
- **Device `ioctl` request words** — one word per promise, dispatched in
  `src/syscall/linux/fs.rs` and served by the device's own module.
- **Structure layouts crossing the boundary** — `struct stat`, `rt_sigframe`, `edid`,
  `sockaddr_ll`, `struct sched_attr`, … `struct stat` is per-ABI in both layout and
  *length*: aarch64's is 128 bytes, with Linux's asm-generic field order (`st_mode` u32 at
  16, `st_nlink` u32 at 20, `st_rdev` u64 at 32, the `timespec` seconds at 72/88/104), where
  x86_64's is 144 (`st_nlink` u64 at 16, `st_mode` at 24). `abi::write_stat` branches both on
  `target_arch` (the field order, and `STAT_BYTES`): emitting the x86_64 order on aarch64
  previously made `st_mode` read the `st_nlink` value, and emitting the x86_64 *length* wrote
  sixteen bytes past every 128-byte struct — into whichever frame the libc's own stat buffer
  sits in, which is how a non-empty `std::fs::remove_dir_all` came to die at `pc 0`
  (  `NOTES.md` §13.10). Pin: `t-fstat`/`t-newfstatat` on both machines (mode/type/`nlink`/`ino`/
  `blocks`/`size` read back through each machine's own `Stat` — the claim the retired aarch64
  payload's `MPOS_STAT_OK` carried, and why the `no_std` suite's hard-coded x86_64 `lin::Stat` had
  to go: a program that writes one machine's layout down cannot see the other's) and
  `t-stat-buffer` (a guarded `struct stat`
  handed to the kernel through `SYS_fstat`/`SYS_newfstatat`, on both machines — a caller's own
  `stat` cannot see the overflow, because musl's wrapper stats into a struct of its own and
  copies the fields out). The three clock *seconds* are the one part the two ABIs share — 72, 88
  and 104, each the seconds half of a 16-byte `timespec` whose nanosecond half follows at 80, 96
  and 112 — and x86_64's writer used to pack them at 72/80/88, so `st_mtime` read the value
  written into `st_atime_nsec` and `st_ctime` read zero (`NOTES.md` §13.11). `statx`'s `struct
  statx` is one layout on every machine, and being arch-independent is not the same as being
  right: it is 256 bytes and Linux zeroes and fills all of them, with
  `stx_atime`/`btime`/`ctime`/`mtime` as 16-byte timestamps at 64/80/96/112, `stx_rdev_*` after
  them at 128/132 and the file's device split into `stx_dev_major`/`minor` at 136/140. This
  kernel wrote its clocks at 96/112/128, its rdev pair at 72, and stopped the write at 152 — and
  no test read any of those fields back (`NOTES.md` §13.11). Pin: `t-statx-agree` (one file's
  device, inode, mode, size, link count and three clocks, read through `statx` **and** through
  the `fstatat` family, which have to be the same numbers) on both machines, and the bare-metal
  suite's `t-statx` for the argument shape and the clocks it compares against `newfstatat` —
  x86_64 only, that suite having no aarch64 port yet. `rt_sigframe`/`sigcontext` is per-ABI as well and lives with
  its machine since §13.2's Stage 2 — `arch/src/arch/x86_64/signal.rs` and, since Stage 3b,
  `arch/src/arch/aarch64/signal.rs`, both behind `hal::SignalFrame` — because `__restore_rt`
  reads the saved registers back at fixed offsets (and on aarch64 the frame's
  `fpsimd_context` record is where a handler's floating-point image comes from).
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
| **Refused** | `ENOSYS` / `EOPNOTSUPP` / `EINVAL` / `EPERM` — Linux's own answer for a kernel that cannot do the call (`AGENTS.md` §3.2) | No. A refusal is the honest direction to be wrong in |
| **⚠ wrong answer** | Accepted, answers success, and does less than Linux — or an errno/shape Linux would not give. The `ENOSYS`-free §3.2 failure class | **Yes.** These are the only defects |

A refusal is listed rather than hidden because a board is only useful if it is true
(`AGENTS.md` §3). A missing feature with a named prerequisite is not a defect; a
success-for-a-thing-not-done is.

---

## 1. The syscall table

`src/syscall/linux/nr.rs` names **252** x86_64 syscall numbers. The dispatch in
`src/syscall/linux/mod.rs` has an explicit arm for the **~230** the kernel implements, plus
a `_ => ENOSYS` catch-all for the rest. That catch-all is deliberately crowded and is an
honest answer (`AGENTS.md` §3.2): a libc probes optional calls by number and reads `ENOSYS`
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

> The numbers for `fanotify` above are the header's (`300`/`301`), and `NOTES.md`'s board
> entry that says `302`/`303` is a typo for them — `t-enosys` pins `300`/`301`, which is
> what a caller sends.

`rseq` (334) is the eighth: like the seven above it has no dispatch arm, but unlike them it
*once had* one that returned `0` without writing any of the fields the ABI contract puts in
the caller's block. §3.2 ranks that shape worst, so the arm was removed; the number stays
named (append-only) and `t-rseq` pins `ENOSYS`. Serving it needs a per-thread registration
whose `cpu_id`/`node_id`/`mm_cid` are maintained across preemption, plus
`RSEQ_FLAG_UNREGISTER` — a scheduler feature (`NOTES.md` A6).

### 1.2 Served with a named sub-gap

Not refusals, not wrong answers: the call is real, and one documented corner of Linux's
contract is not. Each is tracked in its phase section; the list is here so it is countable.

| Syscall | Sub-gap | Where |
|---|---|---|
| `rt_sigqueueinfo` / `sigqueue` | real-time signals `32..=64` are now fully queued (stage C: every copy delivered FIFO with its own `si_value`); the one divergence is overflow — a full per-task ring drops the extra copy silently, where Linux's per-user `RLIMIT_SIGPENDING` makes `sigqueue` return `EAGAIN` | `NOTES.md` §2.3 |
| `mmap` | `MAP_HUGETLB` is `ENOMEM` (an honest refusal); `MAP_HUGETLB` backing is the missing half | §3 below |
| `madvise` | `MADV_FREE` and a shared mapping's cross-process drop are deliberate no-ops | `NOTES.md` §5.2 |
| `posix_fadvise` | recognised advices are accepted as the no-ops they are (this kernel prefetches nothing) | `NOTES.md` §12.1 |
| `vhangup` | with a controlling terminal it answers `EOPNOTSUPP`: the tty re-init + `EIO` hang-up is not implemented | `NOTES.md` §12.2 B4 |
| `getpriority` / `setpriority` / `sched_setattr` | the `nice` round-trips truthfully but carries no scheduling-weight effect | `NOTES.md` §12.2 B5/B5b |
| `setxattr` family | arguments validated, then `EOPNOTSUPP`: no filesystem here stores xattrs | `NOTES.md` Remaining ABI |
| `ptrace` | `PTRACE_EVENT_SECCOMP` refused (no tracer stop); `PTRACE_SETSIGINFO` validates then `EOPNOTSUPP` (no siginfo carried through a stop) | `NOTES.md` §8.2, §12.1 A7 |
| `unshare` / `setns` | UTS served; MNT/PID/NET (and USER/IPC/CGROUP/TIME) refused `EOPNOTSUPP`; `setns` cannot name a namespace by descriptor | `NOTES.md` §6.3 |
| `pivot_root` | `chroot(2)` is now per-process — the task carries its own root and the path walk jails to it (`t-chroot`); `pivot_root(2)` and `MS_MOVE` still act on the single global mount table, which *is* Linux's answer with one mount namespace, and only become per-process with `unshare(CLONE_NEWNS)` | `NOTES.md` §6.3 |
| `process_vm_readv` / `writev` | served for the address transfer, and now gated by `ptrace_may_access` — a caller must share a uid with the target (or hold `CAP_SYS_PTRACE`) and the target must be dumpable. Linux's group-id comparison and its LSM hook are not modelled | §5.1 |
| `/proc/<pid>/maps` | served; an image's and a private file mapping's line carries no `offset`/`inode`; `smaps`/`numa_maps` are not written | `NOTES.md` §7.4 |

---

## 2. Device `ioctl`s, by family

Dispatched on the whole request word (magic byte plus command), never on the command byte
alone — the one time that was done wrong, `RNDADDENTROPY` (`'R'` nr 3) was answered as the
drain (`'R'` nr 4) and OpenRC's `seedrng` failed to seed the boot (`NOTES.md` §12.1 A8).

| Family | Device(s) | Owner module | Verdict |
|---|---|---|---|
| `'R'` (0x52) | `/dev/random`, `/dev/urandom` | `src/syscall/linux/fs.rs`, `src/rand.rs` | Served, measured: `RNDGETENTCNT`, the `RNDADDENTROPY` credit, `RNDRESEEDCRNG` (a real re-key) and `FIONREAD` (`EINVAL`, as Linux); `RNDZAPENTCNT`/`RNDCLEARPOOL` refused `EOPNOTSUPP` (no drainable count). Both former gaps closed — §5.1 |
| `'E'` (0x45) | `/dev/input/eventN` | `crates/evdev` | Served, measured: `EVIOCREVOKE` one-way NULL-arg, `EVIOCGPHYS`/`UNIQ` `ENOENT` for an absent string, `EVIOCSFF`/`RMFF` `ENOSYS`, keycodes `EINVAL`, `REP` stored gated on `EV_REP`, `GKEY/LED/SND/SW` return counts |
| `'U'` (0x55) | `/dev/uinput` | `drivers/uinput.rs` (`Uinput::ioctl`), vocabulary in `crates/uinput` | Served; gaps: no `UI_ABS_SETUP`, no force feedback, no `/sys/class/input` entry (sysfs is built once at boot) |
| `'V'` (0x56) | virtual consoles | `vfs/tty.rs` (`Tty::vt_ioctl`), state in `vfs/vcmux.rs`+`vfs/vtmode.rs` | Served: `VT_SETMODE`/`VT_PROCESS` negotiation, `VT_ACTIVATE` non-blocking, `VT_WAITACTIVE` sleeping, `VT_SENDSIG` → `ENOTTY` (measured) |
| `'K'` (0x4B) | virtual consoles | `vfs/tty.rs` (`Tty::kd_ioctl`) | **Refused as a closed set**: five requests served (`KDSETMODE`/`KDGETMODE`/`KDSKBMODE`/`KDGKBMODE`/`KDGKBTYPE`), the rest of the block `EOPNOTSUPP`, outside it `ENOTTY`. Per-device *serve* from live console state is the Phase-11 B2 move (`NOTES.md` §12.1 A2) |
| `'b'` (0x62) | dma-buf | `src/syscall/linux/fs.rs` | Served (`SYNC` as the documented coherent-memory no-op, `SET_NAME` a debug label) |
| `'d'` (0x64) | `/dev/dri/card0`, `renderD128` | `src/drivers/drm.rs`, `crates/gpu/src/virtgpu.rs`, `crates/gpu/src/*.rs` | Served across the compositor surface, and **the `DRM_VIRTGPU_*` family with it, 2D and 3D both**: all eleven requests are dispatched, and each answers per card shape, as Linux's does (measured; the instrument was `user/staging/virtgpu-probe/`, now deleted - its table is in `NOTES.md` §7.4, and the GL card's own answers too). On a card with no 3D — `GETPARAM` (`3D_FEATURES` 0, `CAPSET_QUERY_FIX` 1, unknown parameter `EINVAL`, the write four bytes wide as Linux's is), `RESOURCE_CREATE` (a real device resource, made across the mailbox to the render task), and `RESOURCE_INFO`/`MAP`/`WAIT`/`TRANSFER_TO_HOST` for the handle it returns; `GET_CAPS`/`EXECBUFFER`/`TRANSFER_FROM_HOST` `ENOSYS`, `CONTEXT_INIT`/`RESOURCE_CREATE_BLOB`/a 3D `RESOURCE_CREATE` `EINVAL`. On a GL card the same requests answer as Linux's GL card does: `VIRGL` and `CONTEXT_INIT` are negotiated (`3D_FEATURES` 1, `SUPPORTED_CAPSET_IDs` 6), `CONTEXT_INIT` is `EEXIST` because the context is created implicitly on the first resource, a 3D `RESOURCE_CREATE`, `EXECBUFFER` and `TRANSFER_FROM_HOST` complete, and the two advertised cap sets are fetched and cached. `RESOURCE_CREATE_BLOB` stays `EINVAL` on both shapes, which is Linux's answer on both too. **One named divergence left, of three**: `RESOURCE_INFO`'s `res_handle` **was** 0 for a plain dumb buffer and `TRANSFER_TO_HOST` on one **was** `EINVAL`, where Linux names a resource and completes the copy - both closed by making the resource on the first request that addresses the handle as one (`drivers/drm.rs`'s `back_dumb_with_a_resource`; Linux makes it at `CREATE_DUMB`, and this card makes it later because the console, weston and the fbdev node all allocate dumb buffers and none of them ever asks). The one left is a *refusal*: a transfer whose rectangle does not fit the backing the device was given is refused where Linux would read past it. Both closed halves are pinned by `t-drm` - a dumb buffer's `RESOURCE_INFO` is non-zero and idempotent, its `TRANSFER_TO_HOST` completes, and a handle the card never made is still `ENOENT`. A framebuffer belongs to the open file description that made it and is released with it (Linux's `drm_file_free` → `drm_fb_release`), so `GETRESOURCES`'s `count_fbs` is a list of live framebuffers rather than a list of ids a dead session left behind. The cursor plane publishes `SRC_X`/`SRC_Y`/`SRC_W`/`SRC_H` and `CRTC_W`/`CRTC_H` beside `type`/`FB_ID`/`CRTC_ID`/`CRTC_X`/`CRTC_Y`/`IN_FORMATS`, because a compositor adds them to every plane's atomic state and one the plane does not have reaches the card as property 0 and refuses the whole commit; this cursor cannot scale or crop, so the identity rectangle is the only one a commit may name and any other is `EINVAL`. Other §7.4 gaps: timeline syncobj behaviour, `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` |
| `'P'` (0x50) | `/dev/dsp` | `src/drivers/virtio/sound.rs` | Served; the family's rate/channels/format belong to the writer |
| `'p'` (0x70) + `0x8024_7009`/`0x4024_700A` | `/dev/rtc0`, `/dev/rtc` | `src/drivers/rtc_dev.rs` | Served on **both** machines (`RTC_RD_TIME`/`RTC_SET_TIME`); alarm/interrupt requests `ENOTTY`, and so is the family aimed at any other descriptor. The readiness asked for is `'p'`, which is what the shipped `linux/rtc.h` spells all twenty-four of its commands with (measured: no `'t'` spelling exists in that header tree) and what Alpine's `hwclock` sends; the arm also tolerates a `'t'` magic never seen, and a request matching only by it reaches the same `ENOTTY` tail (§5.1) |
| `0x12` (block) | `/dev/vda`, `/dev/sda` | `src/blk/`, `src/syscall/linux/fs.rs` | Served; the size trio's buffer direction was the sole outlier and is fixed (§5.1) |
| `'f'` (0x66) | any file an `open` names | `src/syscall/linux/fs.rs` `fs_flags_ioctl` + `src/vfs/ext4/`, `src/vfs/ramfs.rs` | Served: `FS_IOC_GETFLAGS`/`FS_IOC_SETFLAGS`, the inode attribute word `lsattr`/`chattr` are made of; the rest of the family `ENOTTY`; a bit this kernel cannot keep `EOPNOTSUPP` and *not stored*; `IMMUTABLE`/`APPEND` need `CAP_LINUX_IMMUTABLE` (§5.1) |
| `0x89xx` (`SIOC*`) | `eth0` | `src/net/rtnl.rs` (dispatched in `fs.rs`) | Served (`SIOCGIFFLAGS`/`SIOCSIFFLAGS`/`SIOCSIFMTU`/`SIOCGIFINDEX`/`SIOCGIFTXQLEN`/`SIOCGIFHWADDR`) |
| `0x4600`+ (`FBIO*`) | `/dev/fb0` | `src/drivers/video/` | Served (`FBIOGET_VSCREENINFO`/`GET_FSCREENINFO`/`PAN_DISPLAY`/`WAITFORVSYNC`); `FBIOPUT_VSCREENINFO` refused rather than faked |
| `0x54xx` (termios/`TIOC*`) | ttys, pty pairs, consoles | `src/vfs/tty.rs`, `src/vfs/pty.rs` | Served: `TIOCGWINSZ`/`TIOCSWINSZ`, `TIOCGPGRP`/`TIOCSPGRP`, `TIOCGSID`, `TIOCGPTN`, `TIOCSPTLCK`, `TCGETS`/`TCSETS`, `FIONBIO`, `FIONREAD` — the entropy pair answering `EINVAL` as Linux does, and a regular file/pipe keeping the honest `ENOTTY` (§5.2). `TIOCGWINSZ` answers `0x0` for a terminal nobody has sized, as Linux does: a program told zero knows it was told nothing and falls back (or asks the terminal) instead of laying out a window the numbers invented |

Anything not named here falls to the `_ => ENOTTY` tail — Linux's own answer for a request
a device does not implement.

---

## 3. Flag words and closed vocabularies

`NOTES.md` §12.1 records a swept pass over these, one landing per vocabulary. A flag the
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
| inode attribute flags (`FS_IOC_SETFLAGS`' word) | `src/vfs/mod.rs` (`INODE_FL_VISIBLE` / `INODE_FL_MODIFIABLE`) | Served for the six this kernel keeps — `IMMUTABLE`, `APPEND`, `NODUMP`, `NOATIME`, `SYNC`, `DIRSYNC` — of which the first two are enforced and the ext4 driver's three (`EXTENT`, `INDEX`, `HUGE_FILE`) are readable and not writable; every other bit `EOPNOTSUPP`, and a refused word is *not stored* |
| the flags a *filesystem* has no attribute word for | `src/vfs/mod.rs` (`Inode::flags` default) | Refused `ENOTTY` on both requests — Linux's answer for a filesystem with no `fileattr` half: procfs, sysfs, devfs, a pipe, a socket |

---

## 4. Structure layouts

The layouts the kernel writes into a caller's memory are pinned by host tests in
`src/syscall/linux/abi.rs` and `crates/gpu/src/blob.rs`, and by the guest programs that read them
back. This is the list, not the byte-by-byte record (that lives in the tests).

| Layout | Consumer | Pin |
|---|---|---|
| `struct stat` / `statx` | every libc | `t-statx`, `t-fstat`, `t-stat-buffer` (the length, **both** machines), `t-statx-agree` (the two stat families agreeing, **both** machines), `t-rmtree` (the tree walk that felt it), host `abi` tests |
| `rt_sigframe` + `sigcontext` | `rt_sigreturn` | `t-signal-delivery`, `t-sa-restart` |
| `siginfo_t` | signal handlers | `t-signal-delivery` |
| `struct sched_attr` | `sched_setattr`/`getattr` | `t-priority` |
| `struct rlimit` | `prlimit64` | `t-prlimit64` |
| `struct robust_list_head` | `set/get_robust_list` | `t-set-robust-list` (24 bytes) |
| EDID base block + `IN_FORMATS` blob | a DRM compositor | `crates/gpu/src/blob.rs` host tests, verified against `edid-decode`; `t-drm-props` |
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
   Closes `NOTES.md` §12.1 A8 correction (ii).

2. **`RNDRESEEDCRNG` re-keys the generator for real and answers `0` for root.**
   This generator was seeded once and never re-keyed, so the arm answered `EOPNOTSUPP`;
   Linux answers `0` for root and `EPERM` without `CAP_SYS_ADMIN` (measured, both devices).
   `rand.rs` gains a reseed that re-rolls the ChaCha key from fresh seed material mixed
   with the current stream, so it is never a fixed point. Owner: `src/rand.rs` +
   `src/syscall/linux/fs.rs`. Pin: a host test on the key change, `t-ioctl` (root `0`,
   unprivileged `EPERM`). Closes `NOTES.md` §12.1 A8 correction (i).
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
   image (`NOTES.md` §12.1 A9).

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
   that never converges. It is what `niri` was spinning on (`NOTES.md` §7.4). Owner:
   `src/vfs/file.rs` + `src/syscall/linux/fs.rs`. Pin: `t-stat-consistency`, which
   checks both doors on `/tmp` and on `/run/user/0`, in the order the protocol uses.

4. **`lsattr`'s `Not a tty` was an unimplemented ioctl family, and the file it
   reads did something worse than not exist.** The four commands measured in
   `NOTES.md` §12.4b turned out to be busybox applets, three of them asking for a
   file or directory this kernel did not have (`/proc/modules`,
   `/proc/self/mountinfo`, `/sys/bus/usb/devices`, `/sys/block`) and one asking
   for a word the `'f'` family never answered. Owners:
   `src/vfs/procfs.rs`, `src/vfs/sysfs.rs`, `src/syscall/linux/fs.rs`,
   `src/vfs/ext4/`, `src/vfs/ramfs.rs`. Pins: `t-mountinfo` and `t-fsflags` on
   the machine, plus host tests for the renderings and the refusals. Closes
   `NOTES.md` §12.4b.

5. **`/proc/self/mountinfo`'s third field is now the same number `stat` reports,
   and it had to be.** The column `lsblk` fills by matching it against
   `/sys/class/block/<name>/dev` was blank because this kernel put the mount
   table's attachment counter there; the *identity* the counter carries is still
   there, under `fs_of_path`/`fs_id_of_inode`, and the reported number is
   `dev_of_path`/`dev_of_inode` — the device behind the filesystem, or an
   anonymous number for a pseudo-filesystem, which is Linux's own split. The same
   change makes `/dev/vda2` report its `st_rdev`, which it never had. Owner:
   `src/vfs/mount.rs` + `src/syscall/linux/fs.rs`. Pin: host tests that a mount
   record's device is what `stat` answers for its mountpoint, and `t-mountinfo`
   comparing the file against `stat("/")` on the machine.

6. **`/proc/mounts` and `/proc/self/mountinfo` were leaving the root filesystem
   out.** The root's record required a *source*, and only a promoted mount ever
   supplied one, so a kernel-booted root was reported as not mounted at all —
   to `df`, to `findmnt`, and to `switch_root`'s own grep. Linux prints the
   filesystem's type there (`rootfs / rootfs rw 0 0`), and this does now.
   Owner: `src/vfs/mount.rs`. Pin: `t-mountinfo` (the `-`-field line for `/`,
   and its device against `stat("/")`).

7. **`clock_gettime(CLOCK_REALTIME)` answers a live wall time for the whole boot, not
   only until the first milestone check.** The value this kernel promised was right; a
   *boot check* changed it. `check_wallclock` proves `set_boot_epoch`
   moves the anchor by setting it to `1 << 40` and had never restored it, so on aarch64
   — where the milestone checks run on every boot, in front of the sweep and in front of
   a distribution's `init` — every program after the check read a year of **36812**.
   Measured on this host's Linux before the change: the same `localtime_r` → `asctime_r`
   path yields a 25-character timestamp and a live year, and musl's `asctime_r` reaches
   `a_crash()` (a store through NULL) when a timestamp will not fit its 26-byte buffer —
   which is how the wrong clock became a dead `syslogd` rather than a wrong log line.
   Owner: `arch/src/arch/aarch64/mod.rs` (+ `pl031`). Pin: `t-localtime` on **both** machines
   (the anchor read back as `CLOCK_REALTIME - CLOCK_MONOTONIC`, every `struct tm` field
   ranged, the timestamp's length measured through `asctime`'s own format, then the real
   `asctime_r` call) — the first test either suite has had for the calendar. Closes
   `NOTES.md` §13.14.

8. **`/dev/rtc0` is a device of the machine's clock, not of x86's CMOS - so it exists
   outside x86_64 too, and the clock behind it writes.** The node, its two names and the
   `RTC_*` ioctls were gated `target_arch = "x86_64"` while everything they touch was
   already `crate::hal::WallClock`; the second machine's OpenRC `hwclock` service
   therefore found no `/dev/rtc`, could not set the system clock, and reported `[ !! ]`
   on every boot. The second half was a recorded measurement the machine refutes: this
   tree's PL031 module carried "a programmatic write does not stick" and answered
   `write_epoch` with `false`, and the write that had been tried was `RTC_DR` - the
   counter's read door. Measured on the machine before the change: loading `RTC_LR`
   (`0x08`) with `0x6ac360c7` left `DR` reading `0x6ac360c7`. `write_epoch` now loads the
   counter and reads it back (a dropped write is still `false`), which is what
   `RTC_SET_TIME` needed. Owner: `src/drivers/{mod,devices,rtc_dev}.rs`,
   `arch/src/arch/aarch64/pl031.rs`, `src/syscall/linux/fs/ioctl.rs`. Pin: `t-rtc` on **both**
   machines (`RTC_RD_TIME` agreeing with `clock_gettime` and with `gmtime_r` field for
   field, `RTC_SET_TIME` served, `EINVAL` for a month no calendar carries, `ENOTTY` for
   the alarm half and for the family on another descriptor). Closes `NOTES.md` §13.15.

9. **A virtual console is a terminal on the second machine too.** `/dev/tty1`..`/dev/tty6` have
   always been minted on both machines - they are the slave ends of the pty pairs the devfs
   builds - but the console multiplexer that reads their masters was x86_64's, because it
   named `drivers::{video::render,drm}` itself (four `render::notify` calls, `master_held`, and
   the `revoke_now`/`grant_and_resent` pair - measured by un-gating the module and counting:
   exactly 7 errors, all of them that seam). So on aarch64 a getty on `tty1` printed into a pty
   nobody drained, and the `'V'`/`'K'` families answered `ENOTTY`. The display half now goes
   through four `crate::console` seams (`display_notify`, `display_master_held`,
   `display_handover`, `display_card_grabbed`), whose no-framebuffer bodies are the no-op or the
   answer a machine with no card owes, and the machine's receiver hands its bytes to
   `vcmux::route_input` - one function over the same kind of wire as the 16550's. Owner:
   `src/vfs/vcmux.rs`, `src/console.rs`, `arch/src/arch/aarch64/pl011.rs`, `src/board/aarch64.rs`,
   `src/syscall/linux/fs/ioctl.rs`. Pin: `t-vt` on **both** machines (the ioctl words read out
   of the shipped headers rather than recalled, the mode families as round trips over
   `/dev/tty1` with the mode put back, the `VT_*` family over `/dev/tty0` where a serial tty is
   `ENOTTY`, the switch driven by `VT_ACTIVATE`/`VT_WAITACTIVE` and seen on the wire, and the
   refusals Linux gives). One divergence the pin names rather than asserts: `VT_GETSTATE`'s
   layout is not Linux's (`NOTES.md` §13.18). Closes `NOTES.md` §13.16.

10. **`RLIMIT_NOFILE` is 7, not 5 — the resource table was indexed by this kernel's own
    numbering.** `getrlimit`/`setrlimit`/`prlimit64` speak `asm-generic/resource.h`'s numbers:
    CPU 0, FSIZE 1, DATA 2, STACK 3, CORE 4, **RSS 5, NPROC 6, NOFILE 7**, MEMLOCK 8, AS 9,
    LOCKS 10, SIGPENDING 11, MSGQUEUE 12, NICE 13, RTPRIO 14, RTTIME 15. `crates/abi`'s
    `default_rlimits` held the descriptor ceiling on row 5 (Linux's `RLIMIT_RSS`) and the
    process count on row 7 (Linux's `RLIMIT_NOFILE`), so a caller asking for the descriptor
    ceiling read the process count, and `setrlimit(RLIMIT_NOFILE)` wrote a row nothing
    enforced. Measured against this host's own `/proc/self/limits`, which prints Linux's
    sixteen rows in `RLIMIT` order; the label table in `src/vfs/procfs.rs` had fourteen and
    was shifted by the same slip, so `ulimit` read the wrong names as well. Owner:
    `crates/abi/src/lib.rs`, `src/sched/runtime.rs` (the `resource == 7` fd-ceiling sync),
    `src/vfs/procfs.rs`. Pin: `crates/abi`'s `default_rlimits_budgets_stack_nofile_and_nproc`
    (which now also asserts that row 5 is unbudgeted), `src/syscall/linux/abi.rs`'s prlimit64
    test, and `t-prlimit64` on **both** machines (S6 batch 10) - whose "the `RLIMIT_NOFILE`
    soft limit is 1024" claim could only be pinned once the constant came from a maintained
    table instead of from the kernel it was checking.

11. **`setpriority` clamps a nice outside `-20..=19`; it used to answer `EINVAL`.** Linux
    clamps (`if (niceval < MIN_NICE) niceval = MIN_NICE;`, `kernel/sys.c`), and its man page
    lists `EINVAL` under ERRORS only for a `which` that is none of
    `PRIO_PROCESS`/`PRIO_PGRP`/`PRIO_USER`. Measured on this host:
    `setpriority(PRIO_PROCESS, 0, 20)` answers `0` and leaves the task at `19`, and `-21`
    answers `EPERM` - the privilege refusal for the raise the clamp turns it into, where a
    range check running first would have answered `EINVAL` before consulting any privilege.
    The two doors onto the same nice slot disagree, and that disagreement is Linux's:
    `sched_setattr` *does* refuse an out-of-range `sched_nice` with `EINVAL`. Owner:
    `src/syscall/linux/posix.rs`. Pin: `t-priority` on **both** machines (S6 batch 10), which
    makes the call and reads the value back for each side of the range, plus the no-std
    `t-posix`, which had asserted the same `EINVAL`.

12. **`F_SETFD` masks its word to `FD_CLOEXEC`; it used to answer `EINVAL` for anything else.**
    POSIX says `F_SETFD` sets the descriptor's flags, and Linux's `set_close_on_exec` reads
    `arg & FD_CLOEXEC` — so a word with other bits in it is *taken*, and the one bit it names
    is what the descriptor keeps. Measured on a current host **through the raw syscall**, so
    no libc's masking can be mistaken for the kernel's:
    `fcntl(1, F_SETFD, 0xFFFFFFFF)` answers `0` and `fcntl(1, F_GETFD)` then reads `1`, while
    `fcntl(1, F_SETFD, 0xFFFFFFFE)` answers `0` and clears it. This kernel refused such a word
    with `EINVAL` and stored nothing, which is the shape a caller that builds the word from a
    variable cannot recover from the way Linux's answer lets it. Owner:
    `src/syscall/linux/fs/file.rs`'s `F_SETFD` arm. Pin: `t-fcntl` on **both** machines (S6
    batch 11), which reads the stored word back and so tells the three possible answers apart
    — refused, stored whole (`0xFFFF_FFFF`), and masked to `1`.

### 5.2 Small, measured, and not yet landed

- **An absolute path does not make the `*at` family ignore its dirfd.** Linux's rule is that
  when the path is absolute the dirfd is not used at all, and it is the *dirfd* that stops
  being a question rather than the path: measured here, `newfstatat(77, "/tmp", buf, 0)` with
  `77` not open answers `0`, errno `0`. This kernel resolves the dirfd first whatever the path
  looks like — one `base_of` does it for the whole family — so the same call answers `EBADF`.
  Not a §3.2 defect (an honest refusal rather than a wrong number), but a caller holding a
  stale dirfd and an absolute path takes its fallback where Linux succeeded, and the *same*
  difference is reachable through `openat`, `unlinkat`, `linkat` and the rest. Owner:
  `src/syscall/linux/fs/file.rs`'s `base_of` and its twenty call sites. `t-newfstatat`'s
  bad-dirfd claim therefore names its path *relatively*, which is `EBADF` on both machines,
  and does not pin this either way (S6 batch 12). `NOTES.md` §15.17.
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
  `NOTES.md` §15.16.
- **A `flock` dies on *any* close of its description, not the last.** Linux releases the
  description's `flock` when the last descriptor referring to it closes, so closing one of
  several `dup`s leaves the lock in force — measured on a current host: `flock(a, LOCK_EX)`,
  `close(dup_of_a)`, then a fresh `open()` of the file still answers `EWOULDBLOCK`. This
  kernel releases it on any close naming that description, because the close path passes the
  description's identity and nothing about how many descriptors still hold it — so a process
  that dups its lock descriptor and closes the dup silently loses its lock, and a second
  process can take it. `src/vfs/locks.rs`'s own `release_on_close` record already states the
  Linux rule, so the code and its documentation disagree. Owner: `src/syscall/linux/fs/file.rs`'s
  close path (which already tests `Arc::strong_count` after the close for the evdev half) and
  `src/vfs/locks.rs`. `NOTES.md` §15.16.
- **`waitid(2)` ignores its `id`, serves two of Linux's four idtypes, and calls
  "no event asked for" a success.** Three divergences, each measured on a
  current host. (a) The `id` argument is not used: `P_PID` does not name the
  child waited for, so a caller naming one child is handed whichever child
  finishes first (`wait4` *does* honour its pid, which is what makes this
  `waitid`'s own). (b) `P_ALL` and `P_PID` are served and `P_PGID` (2) and
  `P_PIDFD` (3) are answered `EINVAL`, where Linux serves `P_PGID`: measured,
  `idtype = 2` with `id = 0` is accepted and a childless caller is answered
  `ECHILD` (errno 10). (c) A call whose `options` asks for none of
  `WEXITED`/`WSTOPPED`/`WCONTINUED` is answered `0` with the record cleared,
  where Linux clears the record and then refuses with `EINVAL` (errno 22) —
  measured, and the *zeroing* is the half both machines agree on; (c) is the
  §3.2 shape in the milder direction, a success where Linux refuses, rather
  than a wrong number. Owner: `src/syscall/linux/proc.rs`'s `handle_waitid` (the
  `_id` binding, the `idtype` check, and the `options & WEXITED == 0` arm).
  `t-waitid` pins this kernel's answers on **both** machines and names each
  host answer beside it under `MPOS_ON_HOST`. `NOTES.md` §15.11 (batch 18).

- **`rt_sigaction` refuses a bare *query* of `SIGKILL`/`SIGSTOP`.** Linux's rule
  is `act && sig_kernel_only(sig)` — a disposition cannot be *set* for either,
  but a call that hands over no `act` and merely asks what is in force is
  served: measured on a current host, `rt_sigaction(SIGKILL, NULL, NULL, 8)`
  answers `0`, and `t-rt-sigaction`'s two claims print the note that records it
  under `MPOS_ON_HOST`. This kernel validates the signal number before it looks
  at `act` at all (`sched::signal::catchable`), so the same call answers
  `EINVAL`. Not a §3.2 defect — an honest refusal where Linux succeeds, the mild
  direction — but a caller that probes a disposition before setting one takes a
  fallback it did not need. Owner: `src/syscall/linux/mod.rs`'s `sigaction`,
  where the `catchable` check would split into "is this a signal" (unconditional)
  and "may it be given a disposition" (only when `act != 0`). `t-rt-sigaction`
  pins this kernel's answer on **both** machines. `NOTES.md` §15.11 (batch 20).

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
  no-std version pass without blocking anything. Owner: `src/sched/runtime.rs`'s
  `queue_signal` call (the record's code, and the second table) with
  `src/vfs/signalfd.rs`'s `take_signal` (which source wins). `t-signalfd` blocks
  `SIGCHLD` the way its own contract says, claims the *count* of records with
  this kernel's answer on the payload and Linux's under `MPOS_ON_HOST`, and pins
  the exit record's `CLD_EXITED` and pid on **both** machines. `NOTES.md` §15.11
  (batch 21).

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
  Owner: `src/vfs/tty.rs`'s `read`/`foreground` and `src/sched/runtime.rs`'s ctty
  bookkeeping. `t-pty` pins the *delivery* on **both** machines by giving the pair
  a session the ABI's way - a getty's `setsid` + open + `TIOCSCTTY` - and names
  this rule in its comment instead of pinning it. (b) *The number `TIOCGPTN`
  mints.* This kernel numbers its pairs past the six virtual consoles
  (`src/vfs/pty.rs`'s counter starts at `VC_COUNT + 1`); Linux's `devpts` numbers
  from 0 - measured on a current host, which answered 5. Both name a real slave,
  so no caller is misled, but a program that hard-codes a first number does not
  meet it here. `t-pty` pins this kernel's answer on the payload and accepts the
  host's under `MPOS_ON_HOST`, printing what the host said. `NOTES.md` §15.11
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
  mechanism (`NOTES.md` §3.4).
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
  in `NOTES.md` §7.4, as are the GL card's own answers -
  `virgl=1 context_init=1 capsets=2 cached=2 mask=0x6`), with
  its one named divergence left in the `'d'` row above (of the three it carried, two
  are closed). The **mailbox** (`render::job` - an
  ioctl describes a device command and parks, because the control queue belongs to the
  render task) and a **device resource** behind a GEM handle (`Dumb.resource` in
  `drivers/drm.rs`) carry both halves, and the 3D commands ride the same machinery:
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
  `NOTES.md` §7.4. **And one fix the 2D half already owes**: `CREATE_DUMB` should create a
  device resource as Linux's does, which would remove two of the three divergences above.
  It is not done because it would make every dumb buffer eagerly backed and add a device
  round trip to the path the console, weston and `/dev/fb0` all allocate through.

### 5.3 Needs its prerequisite named before it can be honest

- **Real-time signals `33..=64`.** `SIGRTMIN` (32) is queueable, maskable and waitable; the
  rest have no queueable number and no disposition storage. Prerequisite: a 64-bit
  disposition table and a queued-signal list per task (`NOTES.md` §2.3).
- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **`PTRACE_EVENT_SECCOMP`** — carrying a `siginfo_t` through a stop/resume, which the
  stop model does not have (`NOTES.md` §8.2).
- **Real `MSG_PEEK` over TCP/UDP.** Refused `EOPNOTSUPP` today (honest); serving it needs
  peek-preserving delivery in the stack (`NOTES.md` §12.2 B2).
- **`SO_LINGER{on, 0}` and per-socket buffer sizes.** Both are record-only; the first needs
  an unsolicited reset on egress, the second needs buffers not sized once at set
  construction (`NOTES.md` §4.3.3/§4.3.4).
- **`AF_PACKET` `SOCK_RAW`** (frames with their header) — `EOPNOTSUPP`; needs the header
  passed through untouched (`NOTES.md` §4.5).
- **A named `SOCK_SEQPACKET` `AF_UNIX` connection** carries no message boundaries — served
  as the byte stream this family has (`NOTES.md` §4.1).
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`NOTES.md` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).

### 5.4 The structural work, parked with Phase 11

These are not UAPI defects; they are the refactors that keep the UAPI surface honest as it
grows, and each is named so it is not rediscovered as a shape.

- **B2 — each device's `ioctl` contract moves down to its driver.** The `syscall/linux/fs.rs`
  switchboard is the V-2 edge the architecture audit flags, and the `'K'`/`'E'` families
  that are refused "until B2" are refused *because* of it (`NOTES.md` §11.2, §12.1 A2/A3).
- **The `net` module's `target_os = "none"` gate** means `AF_PACKET`'s parsers and the
  `sockaddr_ll` decode have no host test and no fuzz target. Closing it moves the
  wire-format halves to an ungated pure module (`NOTES.md` §4.5, `ARCHITECTURE.md` V-9/V-11).

---

## 6. Where the pins live

- **Host unit tests** — `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`.
  Pure encodings, flag verdicts, struct sizes (`src/syscall/linux/abi.rs`,
  `crates/gpu/src/blob.rs`, `crates/evdev`, `src/net`'s ungated halves).
- **Guest conformance programs** — `cargo xtask test`. `user/abitest/src/bin/syscall/t-*.rs`
  (one syscall family) and `user/abitest/src/bin/integration/t-*.rs` (a device, the
  network, a subsystem end to end), plus the `std`/`rustix` programs in
  `user/tests/src/bin/`.
- `tests.toml` decides which of them the default sweep runs. A behaviour a machine can only
  show where a device or the host's network is present is pinned there or not at all — the
  rule is that a test which cannot work is deleted or made to work, never `#[ignore]`d
  (`AGENTS.md` §3).

The gate's latest measured state is quoted from its own summary line in `NOTES.md` §12.4a;
this file deliberately carries no counts of its own.
