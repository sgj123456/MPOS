# UAPI §5.1–5.4

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

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
   Closes `docs/notes/` §12.1 A8 correction (ii).

2. **`RNDRESEEDCRNG` re-keys the generator for real and answers `0` for root.**
   This generator was seeded once and never re-keyed, so the arm answered `EOPNOTSUPP`;
   Linux answers `0` for root and `EPERM` without `CAP_SYS_ADMIN` (measured, both devices).
   `rand.rs` gains a reseed that re-rolls the ChaCha key from fresh seed material mixed
   with the current stream, so it is never a fixed point. Owner: `src/rand.rs` +
   `src/syscall/linux/fs.rs`. Pin: a host test on the key change, `t-ioctl` (root `0`,
   unprivileged `EPERM`). Closes `docs/notes/` §12.1 A8 correction (i).
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
   image (`docs/notes/` §12.1 A9).

4. **A per-process credential model, and the four `prctl` options it makes honest.**
   `src/cred.rs` holds the four capability sets, the dumpable tri-state and `KEEPCAPS`;
   the storage is the thread group's, in `sched`; and the model has **consumers**, which
   is the whole point — the effective set answers `ptrace_may_access`, the bounding set
   is applied by `execve`, a uid/gid change re-derives the sets, entering a user
   namespace hands them back full (`on_enter_user_ns`, the grant `unshare`/`setns` and a
   child's own `clone(CLONE_NEWUSER)` birth all make, 2026-10-16), the user namespace is also
   the table an id is read through — `stat`'s four
   doors and the socket families' `SO_PEERCRED`/`SCM_CREDENTIALS` (the unix family's and the
   device-event socket's) map a stored id through the
   reader's namespace (2026-10-16) — and `capget`/`capset`
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
   that never converges. It is what `niri` was spinning on (`docs/notes/` §7.4). Owner:
   `src/vfs/file.rs` + `src/syscall/linux/fs.rs`. Pin: `t-stat-consistency`, which
   checks both doors on `/tmp` and on `/run/user/0`, in the order the protocol uses.

4. **`lsattr`'s `Not a tty` was an unimplemented ioctl family, and the file it
   reads did something worse than not exist.** The four commands measured in
   `docs/notes/` §12.4b turned out to be busybox applets, three of them asking for a
   file or directory this kernel did not have (`/proc/modules`,
   `/proc/self/mountinfo`, `/sys/bus/usb/devices`, `/sys/block`) and one asking
   for a word the `'f'` family never answered. Owners:
   `src/vfs/procfs.rs`, `src/vfs/sysfs.rs`, `src/syscall/linux/fs.rs`,
   `src/vfs/ext4/`, `src/vfs/ramfs.rs`. Pins: `t-mountinfo` and `t-fsflags` on
   the machine, plus host tests for the renderings and the refusals. Closes
   `docs/notes/` §12.4b.

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
   `docs/notes/` §13.14.

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
   the alarm half and for the family on another descriptor). Closes `docs/notes/` §13.15.

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
   refusals Linux gives. The one divergence that pin used to name rather than assert —
   `VT_GETSTATE`'s layout (`docs/notes/` §13.18) — is closed: the `'V'` nr 3 arm now writes
   `{ v_active, untouched v_signal, v_state }` as Linux's `struct vt_stat` does, and both
   `t-vt` and `t-seat` assert the three shorts against a pre-filled sentinel instead of
   describing them. Closes `docs/notes/` §13.16.

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
    `crates/abi/src/lib.rs`, `src/sched/runtime/` (the `resource == 7` fd-ceiling sync),
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

13. **A `/dev` node keeps the clocks `utimensat` hands it; every device node here used to answer
    `EOPNOTSUPP`.** Linux's devtmpfs stores a node's `atime`/`mtime` — measured on a host whose
    `/dev` *is* a devtmpfs: `touch -d @1600000000 /dev/zero` answers `0` and `stat` reads the
    stamp back (`2020-09-13 20:26:40`) — and eudev's node path sets the timestamps of every
    device node it manages, which is what a boot showed: the dispatcher's once-per-number note
    printed `mpos: syscall 88 (fd -100) -> EOPNOTSUPP` on aarch64 (280 is the same call on
    x86_64) while the boot still reached `mpos login:`. Nothing failed; what a caller could not
    do was treat the refusal as fatal without taking a path Linux never sends it down, which is
    the §3.2 shape. The cell is where the plan said it would be — `NodePerms`, which already
    held a device node's mode and owner, in `src/vfs/devfs.rs` — and it holds both clocks now,
    forwarded by every node that had one: `DevNode`, and, in their own modules, the console tty,
    `/dev/tty0`, the controlling-tty alias, `/dev/ptmx`, the DRM card, `/dev/fb0`, `/dev/uinput`
    and the evdev nodes. **The kind that plan had left out was found by measuring, not by
    reading**: with all of those wired, the same boot still printed the line, and a temporary
    probe on the refusal named `BlockDev … rdev Some((254, 1))` — `/dev/vda1`, so udev's subject
    was the *disk* nodes, and `DevBlockNode` (a separate inode in the same file) was the device
    kind with no `NodePerms` at all. It has one now, born `0660 root:root` — the number
    `mode_of` already fell back to for the kind, so nothing `stat` reported moved, and
    `chmod`/`chown` on `/dev/vda2` now stick the way they do on the character nodes. Pin: a case
    of the `vfs` suite (`device_nodes_keep_the_clocks_they_are_given`, both node kinds, the two
    clocks moved independently) and `t-devtmpfs` on **both** machines (`/dev/null`, `/dev/tty0`
    and a block node — the first of the names the boot published). *Remaining, named:* a node
    reports zero until something stamps it — Linux's devtmpfs stamps one at creation, and there
    is no mount-time number here to give a node that arrives when its driver registers it. The
    directories, symlinks and socket names devfs makes, which hold no `NodePerms`, keep them too
    now — the     refusal a desktop still showed came from that same pass (`docs/notes/` §15.35) — and
    the trait's default, for a node that has nowhere to keep a clock, answers `EPERM`: measured
    on the host, that is what every anonymous-inode descriptor (`eventfd`, `epoll`, `timerfd`,
    `signalfd`, `inotify`) and every `/proc` directory answer, while `EOPNOTSUPP` is not a
    refusal Linux gives this call at all.

14. **`AF_PACKET` `SOCK_RAW` — the frame *with* its Ethernet header, on both sides.** The
    family refused every type but `SOCK_DGRAM` with `EOPNOTSUPP`, which was wrong twice: a
    caller that wants the header (an ARP resolver, a bridge, `busybox`'s own raw paths) is owed
    it, and an unsupported *type* is `ESOCKTNOSUPPORT` (94) on Linux, not `EOPNOTSUPP`.
    Measured on this host before the change (`.workbuddy/probe_afpacket_{raw,raw2,addr,type}.c`,
    on a veth pair): a `SOCK_RAW` read is the whole frame with its header in front; a `SOCK_RAW`
    send puts the caller's frame on the wire untouched — the header's destination won over
    `sll_addr` when the two disagreed and `sll_protocol` is ignored — while a frame shorter than
    14 is `EINVAL` and a send with no `sockaddr_ll` is `ENXIO`. Owner: `src/net/packet.rs`
    (`PacketSock::raw`, a whole-frame ring, `received`, `send_raw`), `crates/link`
    (`frame_payload`), `src/net/socket.rs` (create, send, both receive doors). Pin: `t-dhcp`
    on **both** machines (a raw socket reads the same answer beside the DGRAM one, and a
    self-built frame goes out the raw socket and draws an answer to the id it wrote) plus a
    `link::frame_payload` host unit test. Closes `docs/notes/` §4.5.

15. **A description's `flock` and `F_OFD_*` lock die with the *last* descriptor, not any
    close.** Linux releases a lock owned by the open file description when the last
    descriptor referring to it goes, so closing a `dup` leaves the lock in force — measured
    on this host before the change: `flock(a, LOCK_EX)`, `close(dup_of_a)`, then a fresh
    `open()` of the file still answers `EWOULDBLOCK`, and case D of `.workbuddy/probe_ofd.c`
    is the same measurement for an `F_OFD_SETLK`. This kernel dropped both on *any* close
    naming the description, so a process that dups its lock descriptor and closes the dup
    silently lost its lock and a second process could take it — what `apk`'s database lock
    exists to prevent. Owner: `src/vfs/locks.rs` (`drop_on_close` for the process's POSIX
    record locks on any close, `drop_with_description` for the description's own on the last)
    and `src/syscall/linux/fs/file.rs`'s close path, which asks the second from the
    description's reference count after the descriptor is gone. Pin: `t-filelock` on **both**
    machines (26 `ok:` lines, byte-identical — each class held through a description whose
    `dup` is closed, and a fresh `open()` refused) plus three host unit tests over the two
    pure halves. Closes `docs/notes/` §15.16 (b).

16. **`rt_sigaction` serves a bare *query* of `SIGKILL`/`SIGSTOP`; only an install is
    refused.** Linux's rule is `!valid_signal(sig) || sig < 1 || (act && sig_kernel_only(sig))`
    (`do_sigaction`), so the two questions are asked *separately*: the number is validated
    whether or not a disposition is handed over, and "may it be given one" only when `act` is
    non-null. Measured on a current host — Linux 7.2.9,
    `.workbuddy/probe_sigaction_query.c`, which declares the kernel's 32-byte record rather than a
    libc's: `rt_sigaction(SIGKILL, NULL, &old, 8)` answers `0` with `handler=0` (`SIG_DFL`),
    `flags=0`, `mask=0`, while the same number with a non-null `act` answers `-1`/`EINVAL`; `0` and
    `65` are `EINVAL` either way. This kernel validated the number with `catchable` before it looked
    at `act`, so the query was refused too, and a caller that probes a disposition before
    installing one took a fallback it did not need. `sched::signal` spells the two questions out
    now (`is_signal` is Linux's `valid_signal`; `catchable` is that answer minus the kernel's own
    two), and the handler asks the second only when `act != 0`. Owner: `src/sched/signal.rs` +
    `src/syscall/linux/mod.rs`'s `sigaction`. Pin: `t-rt-sigaction` on **both** machines (a bare
    query of either answers `0` with a cleared record, an install of either is `EINVAL`) plus
    `signal.rs`'s `a_signal_is_nameable_and_installable_except_for_the_kernels_two`. Closes
    `docs/notes/` §15.11 (batch 20).
