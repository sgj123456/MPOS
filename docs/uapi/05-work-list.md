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

17. **`waitid(2)`'s `id`, its four idtypes and its options word are Linux's.** The door had
    three divergences, each measured on the host before it was written down
    (`.workbuddy/probe_waitid.c`): `id` was ignored, so `P_PID` handed back whichever child
    finished first; `P_PGID` (2) and `P_PIDFD` (3) were `EINVAL` where Linux serves the first
    (`idtype = 2`, `id = 0` accepted, a childless caller `ECHILD`); and a call naming no event
    class answered `0` with the record cleared, where Linux clears the record *and then* refuses
    `EINVAL` — the one answer a caller cannot tell from "no event yet". Landing them meant one
    thing underneath: a way to name a *set* of children, which is `sched::WaitFor` (the
    `refactor` commit before this one), so the door now resolves `P_ALL`/`P_PID`/`P_PGID` into
    the same selector `wait4`'s named child uses — a group through its leader's number, the way
    `setpgid`/`getpgid` resolve one, with `0` the caller's own (a negative id is `EINVAL`, a
    group with no child of ours `ECHILD`). `P_PIDFD` was `EINVAL` here and moved to §5.3 with its
    prerequisite named: no pidfd family existed then to resolve a descriptor through, where Linux
    answers `EBADF` for one that is not a pidfd. That prerequisite landed as item 92, which serves
    the idtype and moves the claim to the host's `EBADF`/negative-`EINVAL` pair. The options word is Linux's whole: the three
    event classes are served rather than faked (`WSTOPPED` and `WCONTINUED` were a zero-filled
    0), `WNOWAIT` reports without collecting, the three Linux-only bits are accepted and ignored,
    and an undefined bit — or no class at all — is `EINVAL` with the record cleared first, which
    is Linux's *order* as measured (the first draft of the fix cleared after the refusal and the
    probe said otherwise). The record's `CLD_*` vocabulary came with it, each code measured:
    `CLD_STOPPED` for a group stop, `CLD_TRAPPED` for the same stop under a tracer (which is why
    `WaitEvent::Stopped` carries `traced`), `CLD_CONTINUED` with `SIGCONT` for a resume. Two
    things stay named in §5.2 rather than guessed: `__WCLONE` narrows on Linux and this kernel
    has no exit signal on a task to narrow with, and `si_uid` is left zero because the record
    outlives the task's slot. Owner: `src/syscall/linux/proc.rs`'s `handle_waitid`,
    `src/syscall/linux/abi.rs` (`exit_chld`, `write_waitid_info`, the `CLD_*` codes),
    `src/sched/runtime/`. Pin: `t-waitid` on **both** machines, 46 `ok:` lines, every one of them
    green on this host first, plus     `abi`'s `waitid_info_reports_the_child_the_siginfo_way` and
    `waitid_info_spells_a_stop_and_a_resume_too`. Closes `docs/notes/` §15.11 (batch 18).

18. **An absolute path makes the `*at` family stop asking about its dirfd — neither whether it
    is open nor what kind it is.** Linux reads the name's first character before it touches the
    descriptor (`path_init` before `fdget_raw`), so every member serves an absolute name through
    a dirfd nothing is open on, and through one open on a *regular file*: measured on a current
    host — Linux 7.2.9, `.workbuddy/probe_at_dirfd.c`, which calls each one through `syscall(2)`
    because glibc rewrites `fchmodat`/`faccessat` into a `/proc/self/fd/<n>/<name>` open when the
    dirfd is not `AT_FDCWD` — `newfstatat(1000, "/tmp", &st, 0)` with `1000` not open answers `0`,
    and so do `openat`, `statx`, `faccessat2`, `readlinkat`, `utimensat`, `mkdirat`, `unlinkat`,
    `symlinkat`, `renameat`, `renameat2`, `linkat` (both of its dirfds), `fchmodat`,
    `fchmodat2`, `fchownat` and `mknodat`, while the same calls with a *relative* name answer
    `EBADF` through a closed descriptor and `ENOTDIR` through a regular-file one. This kernel
    resolved the descriptor first whatever the name looked like, so a caller holding a stale
    dirfd and an absolute path took its fallback where Linux succeeded — and the same difference
    was reachable through every one of those doors, because they all went through one `base_of`.
    The name is read before the descriptor is now, and that order carries Linux's other half
    with it: the name is copied in `getname()` before `path_init()`, so a name pointer the
    kernel cannot read at all (a bad one, or a null one) is `EFAULT` rather than the descriptor's
    `EBADF` — measured for every one of the nineteen doors. The two-name calls split into the two
    shapes Linux has, both measured: `renameat`/`renameat2` read both *pointers* before either
    descriptor (a missing old name beside a bad new dirfd is that descriptor's `EBADF`, while an
    unreadable pointer is `EFAULT` on either side), and `linkat` asks its old name, its descriptor
    and its walk *before it reads the new name at all*, so a missing old name is `ENOENT` with any
    new-side answer — a bad new dirfd included, which alone would be `EBADF`. Owner:
    `src/syscall/linux/fs/file.rs`'s `base_for` and its nineteen call sites. Pin: `t-newfstatat` on
    **both** machines (the served pair, the relative `EBADF` kept beside them, and the `EFAULT`
    order through a raw `libc::syscall`), `t-openat` (the same pair for the family's busiest
    member) and `t-link` (the old-name-before-the-new-dirfd pair, whose two claims tell "asked
    second" from "never asked"). Closes `docs/notes/` §15.17, except for the *empty* name's
    precedence, which stays named in §5.2.

19. **An empty name is answered by the name — `ENOENT` — before the descriptor is asked
    anything.** Measured on a current host across all nineteen doors of the family
    (`.workbuddy/probe_at_dirfd.c`, Linux 7.2.9): an empty name answers `ENOENT` through a dirfd
    nothing is open on, through one open on a *regular file* and through one open on a directory,
    where the relative name beside it answers `EBADF`, `ENOTDIR` and the walk's `ENOENT`
    respectively. This kernel reached the descriptor first, so the same call was `EBADF` (a stale
    descriptor) or `ENOTDIR` (one naming a regular file) — a statement about the descriptor where
    Linux's is about the name. It is the same rule the item above is, so it lives in the same
    place: the refusal is in `base_for`, and every door inherits it from the one helper they all
    come through rather than from a check of its own. `readlinkat` is the one door that must *not*
    come through it with an empty name, and Linux is why: that call consults its descriptor first
    (`EBADF` while it is not open) and only then refuses the empty name — `ENOENT` through a
    regular-file dirfd as well, so its kind is not asked either — which its handler now says out
    loud. Owner: `src/syscall/linux/fs/file.rs`'s `base_for` + `readlinkat`. Pin:
    `t-at-empty-name` on **both** machines — the empty name door by door over the three descriptor
    shapes, plus the `readlinkat` exception's three (41 of that program's claims; item 20 adds the
    flag's fourteen). Closes `docs/uapi/` §5.2's empty-name entry.

20. **`AT_EMPTY_PATH` names the descriptor — including in the two doors that would not.**
    `AT_EMPTY_PATH` is Linux's way of saying "the descriptor is the target" for an *empty* name,
    and six doors take it: `newfstatat`, `statx`, `fchownat`, `fchmodat2`, `utimensat` and
    `faccessat2`. Measured on a current host (`.workbuddy/probe_at_dirfd.c`, Linux 7.2.9), every
    one of the six answers `0` for a descriptor the caller owns — with `fchownat` and `fchmodat2`
    setting the uid and the mode that are already there, so the answer does not depend on who is
    running — and `EBADF` for a descriptor nothing is open on, while beside a name that is really
    there the flag is inert and the *name* answers. Four doors were already right; the two that
    were not are served now: **`utimensat`** took the descriptor only for the null-name spelling
    `futimens` gives and answered `ENOENT` for this one, so the name is read once and its absence
    is what selects the route; **`faccessat2`** refused the flag outright (`EINVAL`) where Linux
    has taken it since 5.8, so it is in the vocabulary now and the empty name it marks asks about
    the descriptor's inode through the same `access_check` and the same identity the flags select.
    Owner: `src/syscall/linux/fs/file.rs`'s `utimensat` + `handle_faccessat2`. Pin:
    `t-at-empty-name` on **both** machines — its `AT_EMPTY_PATH` section walks the six doors over
    both descriptor shapes and pins the flag's inertness beside a real name for two of them.
    Closes `docs/uapi/` §5.2's `AT_EMPTY_PATH` entry.

21. **`fcntl(F_GETFL)` answers the access mode the description was made under, and `F_SETFL`
    cannot change it.** Linux reports `filp->f_flags`, which is two halves: the `O_ACCMODE` the
    caller opened with, and the status bits the description kept. `F_SETFL` writes that word as
    `(arg & SETFL_MASK) | (old & ~SETFL_MASK)`, and `SETFL_MASK` has no member for the mode — so a
    caller that asks for `O_NONBLOCK` alone keeps its direction, and one that asks for `O_RDWR`
    does not get to read. Measured on a current host through `syscall(2)`
    (`.workbuddy/probe_getfl.c`, Linux 7.2.9, so no libc's rewrite is mistaken for the kernel's
    rule): `open(O_RDONLY)` answers `0x8000`, `open(O_WRONLY)` `0x8001`, `open(O_RDWR)` `0x8002`
    (`0x8000` is `O_LARGEFILE`, which that kernel adds to every path open and this one does not
    carry at all); `F_SETFL(O_NONBLOCK)` on the `O_WRONLY` one answers `0x8801` — mode intact, flag
    set — and `F_SETFL(O_RDONLY)` or `F_SETFL(O_RDWR)` answer `0x8001` unchanged; a `pipe()` read
    end answers `0` where its write end answers `1`; a `socketpair` end, a socket, an `eventfd`, a
    `timerfd`, a `signalfd`, an `epoll` and a `memfd` answer `2`; an `inotify` queue answers `0`.
    **What this kernel answered before was two divergences, and the entry that named them was
    wrong about its own subject.** `docs/uapi/` §5.2 and `docs/notes/` §15.16(a) claimed the
    reported word's access bits were "always zero" and that every descriptor therefore read as
    `O_RDONLY`; but the open had stored `flags & OPEN_STATUS_FLAGS` — a mask that has carried `0o3`
    since `b659a62` (and is named `SETFL_MASK` now that it is nothing else) — so a *path* open has
    reported its mode all along, and `t-fcntl`'s claim (which pinned `0` on stdout) could not have
    caught it either way. What was really wrong is the pair a word-wide store hides: the doors that mint their own direction
    recorded none (a `pipe2` write end reported `0` where Linux reports `1`; `inotify_init1`'s
    queue reported `2` where Linux reports `0`), and `F_SETFL` stored the caller's word *whole*, so
    `fcntl(fd, F_SETFL, O_NONBLOCK)` on an `O_WRONLY` descriptor **cleared the mode** — the
    direction the kernel still enforced, but a word a caller reads to decide whether it may read
    or write would have lied to it. **The fix is one owner for the fact and one mask for the
    door.** The mode lives only in `src/vfs/file.rs`'s `access` field — the field the read/write
    `EBADF` refusal already asked — and `File::open_flags` composes the reported word from it and
    the status flags, so there is no second copy to drift; `File::set_status` keeps only
    `SETFL_MASK` (`O_APPEND | O_NONBLOCK`), which is Linux's rule rather than a simplification of
    it, and that constant lost the `0o3` that made it two spellings of one fact.
    `pipe2` records `ACC_RDONLY` on its read end and `ACC_WRONLY` on its write end, and
    `handle_inotify_init1` records `ACC_RDONLY` on the queue — which is also what makes `write` on
    a pipe's read end and `read` on its write end answer `EBADF`, Linux's answer, rather than
    whatever the description happened to permit, and what moves two errnos toward Linux: a pipe's
    read end now answers `EINVAL` to `ftruncate` and `EBADF` to `fallocate`, because the mode
    refuses before the file type is asked — Linux's own order, measured — leaving only the *write*
    end's `EOPNOTSUPP`/`ENODEV` named in §5.2. Owner: `src/vfs/file.rs`, and
    `src/syscall/linux/fs/file.rs`'s `F_GETFL`/`F_SETFL` arms, `pipe2` and `handle_inotify_init1`.
    Pins: `t-fcntl`, whose `F_GETFL` section is rewritten onto descriptors the program *makes* —
    a file opened each way, a `pipe2` pair with and without `O_NONBLOCK`, a `socketpair` end — so
    the section measures the same thing under `MPOS_ON_HOST=1` as it does in a sweep (22 claims,
    green on this host first), plus the host unit test
    `vfs::file::tests::the_reported_word_keeps_the_access_mode_setfl_cannot_change`. **What is
    named rather than implemented** is the rest of Linux's word: it carries `O_LARGEFILE` (added
    to every path open, machine-dependent — `0o100000` on x86_64, `0o20000` on arm64, and this
    kernel deliberately defines neither) and whatever else the open recorded (`O_DIRECTORY`,
    `O_NOFOLLOW`, `O_PATH`, `O_SYNC`/`O_DSYNC`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME`), none of which
    this kernel stores or implements. `docs/uapi/` §5.2 carries that set with the probe's
    transcript, which is §15.16(a) corrected.

22. **The size-changing calls tell what is not a regular file apart, with Linux's own errnos.**
    `ftruncate(2)`, `truncate(2)` and `fallocate(2)` all mean "this descriptor's file changes
    size", and all three asked the *kind* of the node here — which a pipe answers
    `NodeKind::File`, the vocabulary's only word for an unnamed channel (and the word `epoll`'s
    pollable test reads it by), so a pipe slipped past the "not a regular file" refusals and
    reached the VFS default `truncate`'s `EOPNOTSUPP`; `truncate(path)` had no kind test at all,
    so a *directory* and a character device reached it too. Measured on a current host through
    `syscall(2)` (`.workbuddy/probe_fsize.c`, Linux 7.2.9 — the whole descriptor zoo in one run).
    **The descriptor spelling**: `ftruncate` answers `EINVAL` for every non-file (a pipe's two
    ends, a socket, a `socketpair` end, an `eventfd`, a `timerfd`, a `signalfd`, an `inotify`
    queue, an `epoll`, a character device, a directory) and `0` for a regular file and a `memfd`;
    `fallocate` answers `EBADF` when the *mode* refuses first (a pipe's read end,
    `inotify_init1`'s readable-only queue, a directory opened `O_RDONLY`), `ESPIPE` for a FIFO
    whose mode permits the write, and `ENODEV` for everything else that is not a file. **The path
    spelling answers differently**, which is why it is its own measurement: a directory is
    `EISDIR` (where `ftruncate` on one is `EINVAL`, because its write-mode check answers first
    and a directory cannot be opened for writing), and everything else that is not a regular file
    is `EINVAL`. So the divergences were one per call, per end and per door: `ftruncate` on a
    pipe's write end answered `EOPNOTSUPP` here (`t-truncate` had named it since batch 15) where
    Linux answers `EINVAL`; `fallocate` on it answered `ENODEV` where Linux answers `ESPIPE`; and
    `truncate(path)` answered `EOPNOTSUPP` for a directory and for a device. The two *read*-end
    answers had already moved to Linux's in §5.1 item 21, because the description's mode refuses
    them before the type is asked — Linux's own order, and the probe is what says so. **The fix
    asks the node** where the kind lies, and asks directly where there was no test: `ftruncate`
    takes `as_pipe` beside the kind (`EINVAL`), `handle_fallocate` answers a pipe `ESPIPE` (the
    `ENODEV` arm keeps the rest), and `truncate(path)` gained the pair of refusals
    `src/vfs/ext4/`'s own inode already spelled for its tree (`Dir` → `EISDIR`, else `EINVAL`).
    Nothing else in the zoo moved: a socket, an anon-inode descriptor and a character device all
    report `NodeKind::CharDev` here, so the kind tests already told them apart, and a `memfd` is a
    real file on both machines. Pins: `t-truncate` (31 claims) and `t-fallocate`, each claiming
    **both** ends of its pipe, plus the path spelling's two refusals — and `t-truncate` is now a
    program with no per-machine branch and no named divergence of its own. The `ENODEV` those two
    tests had carried for *both* ends was the fallocate man page's word rather than either
    machine's answer for a pipe. Two things this item left named in §5.2 have both moved since: the
    pipe reached by its own `/proc/<pid>/fd` name was the *same* question asked from the other end
    — the door was acting on the link rather than on the file the name denotes — and closing it
    (§5.1 item 24) is what settled it, the path spelling answering Linux's `EINVAL` for that pipe
    because it now reaches the pipe's own inode; and the *other* inode that reports
    `NodeKind::File` without being a regular file — a FIFO made by `mknod` — is gone: §5.1 item 25
    stopped `mknod` making a `File` there, and §5.1 item 26 now makes a real `S_IFIFO` node (and
    refuses the size-changing calls to it by kind), so no node this kernel can make reports `File`
    without
    being one.

23. **`waitid`'s record carries the child's real uid.** `si_uid` was the one field of that record
    this kernel answered with a zero, and the reason was structural rather than an oversight: the
    record outlives the task's slot, and the uid was never carried on it, so a *death* had nothing
    to read it from (`docs/uapi/` §5.2, and `docs/notes/` §15.11's batch 18, which named it rather
    than guessing). Measured on a current host before the fix (`.workbuddy/probe_waitid.c`, through
    `syscall(2)`): `si_uid` is the child's uid for **every** event class — `CLD_EXITED`,
    `CLD_TRACED`, `CLD_STOPPED` and `CLD_CONTINUED` all carry it — while a call that faces no event
    at all is a *cleared* record instead, so the zero written here was indistinguishable from
    "nothing happened yet". Linux's answer is `task_uid(tsk)`, which is `cred->uid`: the **real**
    uid, not the effective one. **The fix is the field that entry prescribed.** `Retired` grows a
    `uid` (the shape `pgid` took when `P_PGID` needed one), taken at exit from the task's own
    identity, and the *live* path — a stop or a resume, reported while the child is still in the
    table — asks the new `sched::identity_of(id)` for it, the shape `credentials_of` has. The uid
    rides `WaitEvent::Exited` too, because a collection takes the record out of the table before the
    caller sees the event: `take_retired_for` now hands back the whole record rather than its code
    (with `take_exit_code_current` the thin view `wait4` keeps, whose status word has no room for a
    uid), and `write_waitid_info` writes it where the host record's own dump put it — `si_uid` at
    offset 20, with its unit test reading that word back. Pin: `t-waitid`, whose record claims now
    read `si_uid` for an exited, a stopped and a resumed child — each against the uid the child
    actually ran as, so the claim means the same thing on either machine — plus three claims that
    need a child which *moved* an id, because that is the one way to tell a real uid from an
    effective one (and from the zero a record that never filled the field also carries). Moving an
    id needs `CAP_SETUID`: the payload runs as root and has it, a hand host run often does not
    (measured: this sandbox's shell has an empty effective capability set), so those are made where
    they can be and named where they cannot — `t-posix`'s shape. 56 claims in a sweep, 49 in a host
    run, and the privilege-dependent section moves the program's own real uid, which is why it is
    the last thing the program does.

24. **A descriptor's *second name* denotes the file behind it, in the walk every following door
    comes through.** `/proc/<pid>/fd/<N>` — and `/dev/stdin`, `/dev/stdout`, `/dev/stderr` and
    `/dev/fd/N`, which are devfs's links into it — is a "magic link": its `readlink` answers a
    *report* (the path the descriptor was opened by, or `pipe:[<ino>]` for a channel that has no
    name), and every path operation on it is an operation on the file the descriptor holds.
    Measured on a current host through `syscall(2)` (`.workbuddy/probe_fdlink.c`, Linux 7.2.9 —
    through the raw call because glibc's `stat` reaches for `statx` and its `open` rewrites, either
    of which would measure the libc): `stat` of the name reports the **file** (regular file, the
    file's own size and inode, `st_dev` 52 — the file's filesystem) while `lstat` reports the
    **link** (`S_IFLNK`, `st_dev` 24 — procfs — size 64, and a mode derived from the descriptor's
    direction: `0700` for `O_RDWR`, `0300` for write-only, `0500` for read-only); `truncate(name,
    4)` moves the size the descriptor reads back, a directory is `EISDIR`, a pipe's two ends and a
    character device are `EINVAL`, and a file whose *path* no longer resolves still truncates,
    because the name is not the path; `fchmodat`/`chownat`/`utimensat`/`access` act on the file
    (the mode and the clock read back through the descriptor follow); a name **inside** a path —
    `/proc/self/fd/<dirfd>/<name>`, the spelling glibc's `*at` fallbacks build — resolves to the
    child of the directory the descriptor holds; `chdir` into a directory's name works; a
    descriptor nothing is open on is `ENOENT`, not `EBADF`; and `open` of the name is a **fresh
    description of the file**, not a `dup` of the descriptor: `open(name, O_RDWR)` reads from
    offset 0 while the descriptor it names stands where it was, and `open(name, O_WRONLY)` on an
    `O_RDWR|O_APPEND` descriptor reports `F_GETFL` `0x8001`, the word the *call* asked for.
    **What this kernel did was two shapes, and only one of them was ever right.** `open_opt` cloned
    the descriptor — position and status flags shared, the requested mode ignored — and *every
    other* following door acted on the **link**, so `stat` returned a success carrying the link's
    metadata (`S_IFLNK`, size 0, procfs's `st_dev`) where Linux returns the file's, `truncate` on a
    regular file answered `EINVAL` (§5.2's entry for that read as `ENOENT` from the code and was
    wrong about its own half), a directory's `EISDIR` came back `EINVAL`, a name inside a path was
    `ENOTDIR`, and a foreign process's channel was `ENOENT`. The resolver handed the link back on
    purpose and the comment said why — "opening one has to clone the descriptor rather than reopen
    a path" — and the measurement is what refutes it. **The fix is one owner: the walk denotes, and
    the doors inherit it.** `MountTable::resolve_walk` replaces a descriptor's payload link with
    the inode the descriptor holds — the new `crate::vfs::file::descriptor_inode`, which answers
    `NoEntry` for a descriptor that is gone, Linux's `ENOENT` — and *keeps walking*, so a payload
    link mid-path is a directory the walk descends into and one at the end is the object every door
    acts on. A **namespace** link is the other payload and stays as it was: it denotes an object
    `setns` joins rather than an inode, so the walk still hands the link back and the door asks
    (`Inode::as_namespace`). `open_opt` loses its clone branch — the walk has already denoted the
    file, so it installs a fresh description, which is Linux's own answer — and the reading the
    clone was built on survives it, because `> /dev/stdout` still lands wherever standard output
    goes: the *file* is what the name denotes in both kernels, and what the clone changed was the
    half a caller cannot see. `followed_link` (linkat's `AT_SYMLINK_FOLLOW`) collapses to the plain
    resolution the walk now performs, `descriptor_file` moves into `vfs` as the one helper, and
    `linkat`'s `via_descriptor` test — which still asks the *no-follow* resolution, whose inode is
    the link — is unchanged. Owner: `src/vfs/mount.rs`'s `resolve_walk`, `src/vfs/file.rs`. Pin:
    `t-fd-name` (49 claims, green on this host first, its `ok:` lines byte-identical on both
    machines), which
    claims the pair `stat`/`lstat` draws, `statx` both ways, the four refusals, the mutators with
    their answers read back through the descriptor, the name inside a path, `chdir`, the fresh
    description with its own flags and position, the deleted file, the closed descriptor and
    `/dev/fd/N`. **Three things stay named in §5.2**, each measured, each its own owner: a *foreign*
    process's descriptor (this kernel's link lends a descriptor only to the caller's own thread
    group, so a foreign one falls back to splicing its report as a path — a named file is then
    reached by the path it reports, and a channel answers `ENOENT` where Linux hands over the
    pipe); `st_dev` (the file's through the name on Linux, the *name's* filesystem here — the
    limitation §5.1 item 5 already owns, and the reason `t-fd-name` compares kind, size and inode
    and merely prints both devices); and `..` after the name (Linux jumps the walk onto the file, so
    the parent is the *file's*; here it is the name's directory, because the walk's chain is what
    answers `..` and a jumped-to node's parent is not a fact this kernel's inodes carry). The
    fourth it named — the link's own metadata — has since been closed by a commit of its own: the
    link answers the mode its descriptor's direction gives and the 64 bytes Linux reports, and the
    descriptor *directory* beside it answers `0500` and the number of descriptors it lists (this
    pin grew by eight claims for it, green on both machines). A fifth thing the same pin
    measured became an entry of its own in §5.2, because it
    belonged to another model rather than to this name: `chdir` through a directory's name serves —
    the walk's half, and it was `ENOTDIR` before — while the `getcwd` after it read back the *string*
    the caller used where Linux canonicalises, a divergence any symlinked `chdir` had here, which
    item 31 has since closed.

25. **The `mknod` doors answer Linux's answers, ask their questions in Linux's order, and build Linux's
    node for the type field's whole vocabulary — with `S_IFIFO` refused rather than faked.**
    `mknodat(2)` (and the legacy `mknod`, 133 on x86_64 alone, which Linux spells
    `do_mknodat(AT_FDCWD, …)`) is what an initramfs pre-seeds its `/dev` with and what a package makes
    a device node with, and the type field of its mode picks the node. Measured on a current host
    through `syscall(2)` (`.workbuddy/probe_mknod.c`, Linux 7.2.9, an unprivileged caller, and it
    needs no privilege but for the two device types): **`S_IFREG` and a plain permission word — the
    type `0`, including a mode of `0` — create a regular file** (`0`, mode `0000` for mode `0`);
    **`S_IFSOCK` creates a socket node** (`0`, unprivileged); **`S_IFDIR` is `EPERM`** (the
    privilege word, not the argument word — `mknod` does not make directories); `S_IFLNK` is
    `EINVAL`; `S_IFCHR`/`S_IFBLK` are `EPERM` without `CAP_MKNOD` and served with it; `S_IFIFO`
    creates a **FIFO**, whose contract is measured whole in `.workbuddy/probe_fifo.c` and recorded
    in §5.2. **The name's own answer, measured the same way: a name that already exists is
    `EEXIST`** — over a node of the same type, over one of a different type, over a *dangling link*
    (the question is about the name), and through both spellings. **And the order of the call's
    questions is Linux's, which the same probe measured from both sides**: the type's own refusals
    come **first, before the name is even copied** (`S_IFDIR` is `EPERM` and `S_IFLNK` `EINVAL` over
    an *empty* name, which would be `ENOENT` had the name been asked first, and over a name that
    already exists, which would be `EEXIST`); then the name; then the name's existence; then the
    directory's own writability (`EACCES` — measured: a device node in a directory the caller may
    not write is `EACCES` rather than the capability's `EPERM`); and only then the kind, which is
    the position Linux asks for a device node's `CAP_MKNOD` in. **The fixes**: `abi.rs` gains
    `mknod_type_is_creatable` (the first question: `S_IFDIR` → `EPERM`, an uncreatable type →
    `EINVAL`, the five it makes plus a plain mode pass) and keeps `mknod_kind` for the kind itself,
    asked last; its unit test asserts each of Linux's answers, including the two refusals' *words*;
    both `mknod` doors answer `EEXIST` for an existing name, asked *without following* so a dangling
    link is the name that exists, and both ask the directory's bits after it. **`S_IFIFO` was
    `EOPNOTSUPP` here** (superseded by §5.1 item 26, which builds the node): this kernel had no
    channel node yet, and a node that is a regular file under a
    FIFO's name is a wrong *shape* — a caller cannot tell it from an empty channel — where an honest
    refusal is one the caller can see (rule 4); it is asked last, so `mknodat(existing, S_IFIFO)` is
    the `EEXIST` Linux answers for a type it serves. The design a real one needs was written into
    §5.2 beside the measurement, so the next pass built it from a contract rather than a guess (and
    that is item 26).
    Owner: `src/syscall/linux/abi.rs`, `src/syscall/linux/fs/file.rs`'s two doors. Pin: `t-mknod`
    (22 claims as root, 21 where the device-node claim is named rather than made — the payload runs
    as root and a hand host run need not; now 36 as root and 33 as an ordinary user, item 36's
    capability section), its `ok:` lines identical on both machines; the `S_IFIFO`
    section claims the node on both machines now that item 26 serves it. **A third fix
    the pin forced, in the same subject**: `stat`'s permission half collapsed a stored `0000` into
    the kind's default — `mode_of` was handed a bare `u32` and read `0` as "nobody said" — so a file
    `mknod` was told to make with mode `0` (or one `chmod 000`ed) *enforced* its empty mode through
    `may_as`, which asks `keeps_modes`, while *reporting* `0644`. The stat doors now pass
    `Option<u32>` through the new `reported_mode` (`src/syscall/linux/fs/file.rs`): a filesystem that
    keeps modes reports what it holds, **including `0000`**, and one that keeps none answers its
    kind's default. Two things stay named in §5.2 rather than fixed: the **device number**
    `mknod`'s `dev` argument names is not written on the node; and **`CAP_MKNOD` is not checked**
    (superseded by item 36, which asks it), so
    an unprivileged caller can make a device node here where Linux answers `EPERM`. The FIFO's
    *node*, which this item left as `EOPNOTSUPP` with its prerequisite spelled out
    (`open_replacement` takes no flags, and the direction an open asked for is what picks the end),
    is built by item 26, which meets that prerequisite.

26. **A FIFO is a channel, not a file: `mknodat(S_IFIFO)` makes one, and its `open` runs Linux's
    rendezvous.** Until this pass the `S_IFIFO` arm of `mknod_kind` was `EOPNOTSUPP` (item 25), and the
    pass before that even made a **regular file** under the name — and a rendezvous that is a regular
    file is a wrong *shape*: it reads end-of-file at once, where Linux parks the caller until a peer
    opens the other end, a difference no caller can read off the number alone (rule 4). The contract
    was measured whole on a current host through `syscall(2)` (`.workbuddy/probe_fifo.c`, Linux 7.2.9)
    before any of it was built. **The node**: `mknodat(S_IFIFO)` makes a node whose `stat` is
    `S_IFIFO`, size `0`, `nlink` 1, and a second `mknod` on the name is `EEXIST` (item 25's half).
    **The rendezvous**: `open(O_RDONLY)` with no writer and `open(O_WRONLY)` with no reader both
    **park** until a peer opens the other end — and a reader parked in `open` counts as a reader, so
    the arriving writer releases it; `open(O_RDWR)` is its own peer and does not park;
    `O_RDONLY|O_NONBLOCK` returns at once and `O_WRONLY|O_NONBLOCK` with no reader is **`ENXIO`**.
    **The stream**: a nonblocking reader with no writer reads `0`, with a live writer that wrote
    nothing `EAGAIN`, and after the last writer closes `0` again; the last reader closing makes a
    write `EPIPE`; and the buffer is **discarded** when both counts reach zero, so the next open
    starts empty. **The file-shaped answers**: `ftruncate` and `truncate(name)` are `EINVAL`,
    `fallocate` is `ESPIPE`, `lseek` on an opened end is `ESPIPE`, an *opened* description's `stat` is
    `S_IFIFO` too, and `execve` of the name is `EACCES`. **The fix is a channel that lives as long as
    the name, plus a rendezvous at open**, and it needed three things the pipe module did not have: a
    `NodeKind::Fifo` — the vocabulary's new word, which `mode_of`/`dt_type_of` answer `S_IFIFO`/
    `DT_FIFO` for and which `fsync`/`fallocate`/the size-changing calls refuse by (items 21–22);
    a `Channel` in `src/vfs/pipe.rs` carrying the pipe's `PipeState` and `Ring`, but **minted at the
    name** and joined by every open, with reader/writer counts taken at **open** and given back at
    the last close of a *description* (not at node drop — a `dup` and a `fork` share a description,
    which is why the pipe's counts are already per description); and `Inode::open_replacement` gains
    the open's `flags`, because the **direction** an open asked for is what picks the end —
    `O_RDONLY` a reader, `O_WRONLY` a writer, `O_RDWR` one `Role::Both` description, `O_NONBLOCK`
    deciding park-versus-`ENXIO`. The end an open returns *is* a `PipeEnd`, so `as_pipe`, the
    size-changing calls and `splice` follow the pipe's rules with no second answer, and the end's
    `metadata()` reports the FIFO's own kind and ino — which is how an opened description's `fstat`
    says `S_IFIFO`. The rendezvous is `sched::block_until_interruptible` over a `ready` predicate
    plus a `waiting_open` set that every count change wakes, the shape `PipeEnd`'s read and write
    already use — and `open_replacement` reaching `flags` touched every node that mints on open
    (`/dev/ptmx`, the ptys, the console tty, the socket node), each passing `_flags` it does not read.
    Owner: `src/vfs/pipe.rs` (`Role`, `Channel`, `rendezvous`, the discard), `src/vfs/ramfs.rs`'s
    `RamFifo`, `src/vfs/mod.rs`'s `NodeKind::Fifo` + the `open_replacement` signature,
    `src/syscall/linux/abi.rs`'s `mode_of`/`dt_type_of`/`mknod_kind`,
    `src/syscall/linux/fs/file.rs`'s `open_linux` (passing `flags`) and its `fsync`/`fallocate` arms.
    Pin: `t-fifo` (33 claims, byte-identical on both machines, green on this host first) for the whole
    contract, and `t-mknod`'s `S_IFIFO` section for the node — each claimed, not described, and the
    pipe's four new host tests (`a_fifo_channel_refuses_a_lonely_nonblocking_writer`,
    `a_fifo_channel_pairs_a_reader_and_a_writer`, `a_fifo_open_readwrite_is_its_own_peer`,
    `closing_a_fifos_last_two_ends_discards_the_buffer`) hold the pure halves. **One residual was
    named rather than hidden**, and item 28 closes it: a **fatal** signal (`SIGKILL`) delivered while
    a task is parked in a FIFO's `open` ended the task inside the wait, so the count that open took
    was never given back and the FIFO remembered a peer that never arrived. `t-fifo`'s rendezvous
    claims therefore *release* every parked opener (a peer arrives) rather than killing it; the
    section item 28 added is the one that kills them, and the answer it claims is Linux's.

27. **A blocking UDP send waits for the send buffer: `EAGAIN` is the non-blocking answer, and a park
    is the blocking one.** A datagram socket's send buffer is finite, so a burst outruns it, and what
    the caller hears then is the caller's question. Linux answers it two ways and this kernel answered
    one of them to both: the §5.2 entry named the second half — "the wait is not implemented, and that
    is the named half" — and the reason it gave was that there is no event to wait on, because the
    driver takes no interrupt for a TX completion. What that reasoning got wrong is the wakeup
    source's shape rather than its existence: `park_until` registers the sender with the wake list
    **every stack poll** wakes (the background service task each tick, `irq_service` on a device
    interrupt), and a poll is exactly the event that drains a socket's send buffer. So the wait had a
    wakeup all along; what it needed was a condition true only when the buffer can really take the
    datagram — and both earlier shapes failed on *that*: a park with no deadline stopped the sweep
    dead at `t-netstats`'s first parked send, and a tick-bounded park spun at **97% CPU** because the
    condition (`can_send()`, the **metadata** ring) was true while `enqueue` still refused on the
    **payload** ring, and a park whose condition holds returns at once — so the caller retried, was
    refused and parked again without ever sleeping.
    **The contract was measured whole on a current host first** (`.workbuddy/probe-udpblock*.c`, Linux
    7.2.9), in the one state that fills the buffer without root: an **on-link neighbour that never
    answers ARP** holds the first datagram, and an skb sitting in that queue keeps its
    `sk_wmem_alloc` charge, so the socket's send buffer fills behind it exactly as it would behind a
    slow device — and the neighbour gives up about three seconds later, which is also when a parked
    sender's room appears. With `SO_SNDBUF` at its floor (2048 asked, 4608 reported) and 1400-byte
    datagrams: **three went in**, the fourth *blocking* send slept **3072-3098 ms** and then returned
    the datagram, and the non-blocking send at the same point answered `EAGAIN` at once. A signal
    inside the park answers **`EINTR`** when the handler has no `SA_RESTART` and is swallowed — the
    send restarted and completed — when it has one; measured with the handler's own timestamp, and
    the measurement that caught a probe bug worth its own note: glibc's `signal()` defaults to
    `SA_RESTART`, so the "no restart" arm was silently restarting until a **pipe-write control** (a
    blocking write to a full pipe, `EINTR` at exactly the alarm's 300 ms) proved the delivery setup
    was sound. Two further answers separate the *size* question from the *room* one, and this kernel
    does not serve either: Linux's `EMSGSIZE` line is the **maximum UDP datagram** — 65507 accepted,
    65508 refused — not the send buffer, and a datagram *wider* than `SO_SNDBUF` is still taken (5600
    bytes into a 4608-byte buffer, measured).
    **The fix** is `udp_send`'s loop: the buffer-full arm is no longer one answer for both modes but a
    park for a blocking caller and `EAGAIN` for a non-blocking one, over
    [`park_until_deadline`]'s registration. The condition is [`udp_room_for`], which asks **both**
    questions `PacketBuffer::enqueue` asks — the packet count (`can_send()`, four slots here whatever
    their size) and the payload window (`payload_send_capacity() - send_queue()`, what a burst of
    MTU-sized datagrams fills first) — because a predicate that is true while the enqueue still
    refuses is a wait that cannot end. The one case it cannot see is the ring's wraparound, which
    clears as the buffer drains, so it costs one more turn of a loop bounded by the four packet slots.
    Owner: `src/net/socket.rs`'s `udp_send`, `udp_room_for`, `park_until_deadline` and `DRAIN_WAIT_MS`.
    Pin: `t-udpblock` — the fill with per-send `MSG_DONTWAIT` (so the fill itself does not park), the
    non-blocking answer at once, the blocking send's park cut short by a `SIGALRM` with no
    `SA_RESTART` (answered `EINTR` after the alarm's own delay, which is what makes it a *wait*
    rather than a refusal wearing a park's clothes), the same send restarted under `SA_RESTART` and
    then answered `EAGAIN` after the bound, and a fresh socket still sending afterwards.
    **Three residuals are named rather than hidden.** (a) The **`EMSGSIZE` boundary is this kernel's
    payload ring** (4 KiB), not Linux's 65507: a datagram wider than the ring can never be enqueued,
    so it is refused as oversized where Linux takes it and lets it exceed the soft limit — that is
    the buffer-size item (`SO_SNDBUF`), which §5.2 and the ROADMAP carry separately, and it moves the
    line the day the ring does. (b) The park is **bounded**, at `DRAIN_WAIT_MS` (three seconds, the
    window Linux's own neighbour resolution takes): past it the answer is `EAGAIN`, because smoltcp
    *keeps* a datagram its egress could not dispatch — an unknown neighbour's silent second, a
    datagram the MTU cannot carry, a link taken down — where Linux's device drops the frame and
    releases the charge, so an unbounded park here is a hang rather than a wait. The bound's worst
    case is the behaviour this pass replaced, which is what makes it safe. (c) The wraparound above.

28. **A FIFO's parked `open` gives its count back when the task dies inside the park.** Item 26's
    residual, and the last thing the rendezvous left named: the count an open takes *before* it parks
    is what the arriving peer sees, so a task ended inside that park took a count with it and the
    FIFO went on counting a peer that never arrived — every later writer opening into a reader that
    was gone, every later reader reading `EAGAIN` against a writer that had died. **Measured on a
    current host first** (`.workbuddy/probe_fifo_park.c`, Linux 7.2.9), in the one state a fixture can
    build without a peer: an opener arms its own `ITIMER_REAL` and then parks in a *blocking* `open`,
    so the clock is the only thing that can end the park. A reader killed there leaves
    `open(O_WRONLY|O_NONBLOCK)` answering **`ENXIO`** (errno 6, printed) where a phantom reader would
    have made it a success, and a parked **writer**'s death leaves a reader reading **`0`** (end of
    stream; errno 0, printed) where a phantom writer owes `EAGAIN`; a third scenario confirms the
    premise the whole item rests on — while the child is *parked*, `open(O_WRONLY|O_NONBLOCK)`
    succeeds, because a parked reader counts as a reader — and a fourth that after both deaths the
    rendezvous still pairs (writer's blocking open `3`, one byte written, the reader reads it).
    **The fix is Linux's own order, and it is one wait rather than a new owner.**
    `Channel::rendezvous` parks on the new [`block_until_interruptible_rolling_back`] instead of
    `block_until_interruptible`: the two differ in exactly one thing, and it is the whole item — a
    **fatal** signal *ends* that park and stays pending instead of ending the task inside it, so the
    give-back `Channel::open` already runs for a catchable signal runs for `SIGKILL` too, and the
    death happens where Linux's does, at the syscall gate, which finds the signal pending on its way
    out and takes its default action. Linux's `fifo_open` is the same shape one `goto` away: its
    `wait_event_interruptible` returns `-ERESTARTSYS` for a fatal signal as well, the `err_rd:` label
    gives the count back, and the task dies after the syscall has unwound rather than in the middle of
    it. The parked opener's *record* of itself in `waiting_open` is cleaned by the same path, since
    the retain after the wait is now reached. Nothing about the death changes: the same number ends
    the task, with the same exit code, and a traced task now gets the signal-delivery stop Linux gives
    it rather than dying without one. Owner: `src/sched/runtime/task.rs`'s
    `block_until_interruptible_rolling_back` (+ its export in `src/sched/mod.rs`) and
    `src/vfs/pipe.rs`'s `Channel::rendezvous`. Pin: `t-fifo`'s killed-opener section (nine claims:
    the parkedness, the death, and the FIFO's answer for each direction, plus the three that prove the
    counts are back to zero), and the counterfactual is exact — with the park put back to
    `block_until_interruptible`, the section's answers are the phantom peer's (`ENXIO` becomes a
    success, `0` becomes `EAGAIN`, and the blocking reader no longer parks).

29. **A child's death is one `SIGCHLD` record, and it is Linux's record.** §5.2's
    `signalfd`/`SIGCHLD` entry named three divergences on the path a descriptor reader walks, and
    this pass serves all three — which meant deleting a table rather than adding a branch.
    **Measured whole on a current host first** (`.workbuddy/probe_signalfd_chld.c`, Linux 7.2.9),
    one scenario per disposition, **each in its own forked child because the first draft measured
    its own leftovers** (a handler installed by scenario 3 was still installed in scenario 8's
    reading, and the record it captured named the *previous* scenario's child — a fixture bug that
    read exactly like a kernel one):

    * blocked `SIG_DFL`, a child exits 7 → **one** record
      `signo=17 code=1[CLD_EXITED] pid uid=1000 status=7 ptr=0 utime=0 stime=0`, then `EAGAIN`;
      `wait4` still collects it, and the read does *not* collect it (the record survives the reap);
    * the same with a child that burned 60 ms of CPU → `utime=6`, `stime=0`;
    * unblocked `SIG_DFL` → **nothing**, before or after the reap (the embarrassing half: the entry
      this replaces read its own refusal as "measured", and this is the door that says otherwise);
    * blocked + a handler → one record, and the read **consumes** it: the handler never runs, even
      after the unblock;
    * `SIG_IGN` (blocked or not) → nothing, and `waitpid(WNOHANG)` is **`ECHILD`** — the kernel reaped
      the child;
    * a child that takes `SIGKILL` → `code=2[CLD_KILLED] status=9`;
    * a child that takes `SIGSEGV`, `RLIMIT_CORE` 0 → `code=3[CLD_DUMPED] status=11`, and `wait4`'s
      status word carries `WCOREDUMP`;
    * the same death through a handler's `siginfo_t` and through `sigtimedwait` → `si_code=1`,
      `si_status=7`, `si_pid` the child, `si_uid` its uid (the word at offset 24 is the status, not a
      `sigval`).

    **The fix is Linux's shape: the pending queue is the one table.** `SIGCHLD_PENDING`, the second
    book this kernel kept for "an exit whose signal is still unread" (with `read_sigchld` and
    `child_exited_unreaped`, the descriptor's synthetic `ChildExit` record, and the retains in
    `take_retired_for`/`take_exit_code`/`record_retired`) is **deleted**; the exit path queues the
    real signal instead, and it queues Linux's record for it — `signal::SigInfo::child(pid, ruid,
    code)`, whose `(si_code, si_status)` pair is `crate::abi::exit_chld`'s, the same function
    `waitid`'s record uses, moved into the leaf crate so the scheduler and the syscall layer cannot
    come to spell a child's death two ways. The predicate is refined to the measured rule: **never
    for a parent that ignores `SIGCHLD`**, otherwise a handler *or* a blocked signal — the entry this
    replaces had the ignore case wrong (it queued for a blocked `SIG_IGN`), and the `is_ignore()`
    test comes first, because generation and delivery ask different questions. The status travels to
    every door that renders it: `signalfd`'s `ssi_status` (offset 40), and the `siginfo_t` union at
    offset 24 for a handler's frame and for `sigtimedwait` — where the class test (`1..=6` is the
    `_sigchld` member, everything else the `_kill`/`_rt` one) is now one function,
    `hal::SignalInfo::chld_union`, instead of two machines' worth of `if` and a third in the syscall
    layer. Owner: `src/sched/runtime/child_exit.rs`'s exit path and `src/sched/runtime/task.rs`
    (the deleted table), `src/sched/signal.rs`'s `SigInfo::child`/`status`,
    `crates/abi`'s `exit_chld`/`CLD_*`, `src/vfs/signalfd.rs`'s `take_signal`, `arch/src/hal/mod.rs`'s
    `chld_union`, the two machines' `SignalInfo` writers and `src/syscall/linux/proc.rs`'s
    `sigtimedwait`. Pin: `t-signalfd`, 46 claims with **no per-machine branch left** (the count it
    used to claim with a `note:` is now `1` on both), green on a current host first — it claims the
    record field by field (`ssi_status`, the real uid, the pid), the unblocked death's `EAGAIN`, the
    `SIGKILL` record's `CLD_KILLED`, and the frame's rendering of the same record through a handler.
    Two of those are named rather than made on a **host** run, where the real uid cannot be moved
    (`t-waitid`'s shape for the same field): 46 claims in a sweep, 44 with two notes there (**62**/
    **60** after item 33's CPU claims — measured). Two residuals of this paragraph have since landed:
    the record's `si_utime`/`si_stime` (the child's CPU in ticks — item **33**) and the
    **`CLD_DUMPED`/`WCOREDUMP`** half (item **32**), whose measurement also corrected a *reason*: the
    core bit is not about a file being written (a `SIGSEGV` child reports it with `RLIMIT_CORE` 0),
    so `wait_status`'s comment that said "mpos writes no core files" was explaining the wrong thing.
    **One residual stays named rather than hidden** (§5.2 carries it with the measurement): the
    **ignored** child's auto-reap (`ECHILD` on Linux, a zombie here) — the only one of the three
    left.

30. **`FIONREAD` answers Linux's per-type byte count on a regular file and a pipe.** The request
    means "how many bytes can I read before I would block", and Linux answers it per *file type*
    rather than with one rule; this kernel answered `ENOTTY` to everything but the entropy pair,
    which §5.2 named as a divergence a caller can see. Measured whole on a current host first
    (`.workbuddy/probe_fionread.c`, Linux 7.2.9, the descriptor zoo in one run): a **regular file**
    answers `i_size - f_pos` — `10` at offset 0 of a ten-byte file, `6` after a four-byte read, `0`
    at end of file, and **`-10`** at offset 20, because Linux's
    `put_user(i_size_read(inode) - file->f_pos, int __user *)` truncates the wide subtraction to a
    signed 32-bit word instead of clamping it (and a `/proc` file, a regular file by `S_ISREG`
    with size 0, answers `-f_pos`); a **pipe** answers its queued bytes, on *both* ends (`5` after
    a five-byte write, and the same for a FIFO, whose opened end is the same `PipeEnd`); the two
    **entropy devices** answer `EINVAL` before the argument is read (item 1); and a **directory**,
    an `eventfd`, `/dev/null` and `/dev/zero` answer `ENOTTY`. The pointer is looked at only where
    there is a value to write — the same bad pointer is `EFAULT` on a file and a pipe, `ENOTTY` on
    a directory and `EINVAL` on the pool. **The fix is one trait method and the dispatch that reads
    it**: the node that holds the queue answers through `Inode::fionread` — a pipe its queued
    bytes, a connected stream socket the whole readable stream (`3 + 4` written answers `7`), a
    datagram one its **first** pending message (`5` then `9` queued answers `5`), a terminal its
    input queue (a completed canonical line counts, the line still being typed does not, and raw
    mode counts every byte) — while the arm keeps the two answers only it can give, the regular
    file's `size - pos` (written as `size.wrapping_sub(pos) as u32 as i32`, Linux's truncation) and
    the kind refusal (`ENOTTY` for a directory, an eventfd, a **netlink** socket). A **listener**
    (TCP or AF_UNIX) has no connection and therefore refuses `EINVAL`, and a fresh unconnected
    socket answers `0`. Owner: `src/syscall/linux/fs/ioctl.rs`'s `0x541B` arm and the
    `Inode::fionread` overrides in `src/vfs/pipe.rs`, `src/vfs/tty.rs`, `src/vfs/pty.rs`,
    `src/net/socket.rs`, `src/net/unix.rs`, `src/net/mod.rs`. Pins: `t-ioctl` (regular file, pipe,
    `socketpair` stream + datagram), `t-pty`, `t-tcp-listen`, `t-udp-loopback`, all green on this
    host first. A **raw** socket's Linux answer stays unmeasured and it is refused `ENOTTY` rather
    than guessed (§5.2 names it). The earlier text's claim that a tty "answers `0`" was a
    canonical-mode artifact and is corrected: an earlier probe had left raw bytes in the line
    buffer, so the clean per-case measurement is the one above.

31. **`getcwd` derives the cwd's name from the directory, not from the argument `chdir` was given.**
    §5.2 named this: this kernel's cwd was the path *string* `chdir` stored (lexically normalised,
    no link followed), so a `chdir` through a symlink - or through a descriptor's
    `/proc/self/fd/N` name - read back as the name the caller used, where Linux canonicalises by
    walking the dentry chain. Measured whole on a current host first (`.workbuddy/probe_getcwd.c`,
    Linux 7.2.9): a regular path is itself; a **symlink** at the end *or* in the middle of the
    argument answers the target; a **descriptor's name** and a **`fchdir` through a symlinked
    descriptor** answer the canonical path too; a `..` taken after a link that **crosses to
    another parent** climbs from the *target's* parent, not the link's; a path with `./` and a
    trailing `..` folds; the root answers `/`; and a cwd removed underneath the caller makes
    `getcwd` answer `ENOENT`. **The fix is one place**: `adopt_cwd` - the single writer of the cwd
    - now stores the name `MountTable::path_of` finds for the directory (the same bounded search
    `/proc/mounts` uses, and the one `fchdir` used to run for itself, since deleted), falling back
    to the caller's spelling when the tree cannot name the node. That fixed the `..`-through-a-link
    half as well, because relative resolution reads the same string. Owner:
    `src/syscall/linux/fs/file.rs`'s `chdir`/`fchdir`/`adopt_cwd`. Pin: `t-fd-name`'s new claims -
    the descriptor's name after a `chdir`, a symlink, a cross-parent link, and the `..` it takes.
    **Two consequences stay named**: the search can answer a *different* name for a directory the
    tree reaches several ways, or `None` for a cwd deeper than its visit budget (the caller's
    spelling is kept then); and a cwd **removed underneath the caller** is `ENOENT` on Linux, where
    this kernel keeps no parent edge to notice the disconnect and answers the stale name.

32. **A signalled death is `CLD_DUMPED`/`WCOREDUMP` when the signal dumps core *and the child was
    dumpable*.** §5.2 (and item 29) named this as the third residual of the `SIGCHLD` landing: this
    kernel answered `CLD_KILLED` and left `wait4`'s status-word bit clear for *every* signalled
    death, where Linux tells two apart. **Measured whole on a current host first**, in two probes,
    because the rule turned out to have two halves. `.workbuddy/probe_coredump_signals.c` (Linux
    7.2.9, `RLIMIT_CORE` 0) walks every signal whose default action ends a task and reads back both
    the `wait4` status word and `waitid`'s `si_code`: exactly **`SIGQUIT`, `SIGILL`, `SIGTRAP`,
    `SIGABRT`, `SIGBUS`, `SIGFPE`, `SIGSEGV`, `SIGXCPU`, `SIGXFSZ`, `SIGSYS`** are
    `code=3[CLD_DUMPED]` with the bit set, and every other fatal signal (`SIGKILL`, `SIGTERM`,
    `SIGPIPE`, …) is `code=2[CLD_KILLED]` with it clear — the set is `sig_kernel_coredump`'s. That
    probe also killed the *reason* this kernel's `wait_status` comment gave ("mpos writes no core
    files"): a `SIGSEGV` child reports the bit with **`RLIMIT_CORE` 0 and read back as 0**, so the
    bit is not about a file having been written. The second probe,
    `.workbuddy/probe_wcore_dumpable.c`, then separated *signal* from *dump attempted*: this host's
    `core_pattern` is a **pipe** (`|/usr/lib/systemd/systemd-coredump …`), so `RLIMIT_CORE` cannot
    stop a dump being attempted — but `PR_SET_DUMPABLE 0` can, because `do_coredump()` bails at its
    first `__get_dumpable()` check. Under it the *same* `SIGSEGV` death is `CLD_KILLED`/bit-clear,
    while `dumpable` **1** reports `CLD_DUMPED`/bit-set, and a child that sets its own flag back to
    1 under a non-dumpable parent reports `CLD_DUMPED` too — so the flag is the *child's* and the
    gate is `!= 0`. (The kernel's own `SUID_DUMP_ROOT` value, 2, is *not* settable through
    `PR_SET_DUMPABLE` — Linux answers `EINVAL`, measured — so 1-versus-0 is the only pair the door
    can show; the predicate reads `!= 0`, which is `__get_dumpable`'s comparison.) **The fix threads
    the classification, computed once at the death.** The signal half is the leaf crate's
    `abi::signal_dumps_core` (the ten-signal set above, host-tested exhaustively over `1..=31`), and
    `abi::exit_chld` now takes a `dumped: bool` beside the code — because *neither* half can be
    recovered from the code once the child is gone.
    The exit path (`src/sched/runtime/child_exit.rs`) computes `dumped = code < 0 &&
    signal_dumps_core(-code) && child's dumpable != 0` while the `mm` is still there (it reads the
    child's credentials before `release_exited`), and carries it on both records a `wait` reads: the
    retired one (`Retired` and `WaitEvent::Exited` gained the field, with `take_retired_current`
    handed over whole rather than as a bare code) and the `SIGCHLD` one (`signal::SigInfo::child`
    gained it). `wait4`'s `abi::wait_status` and `waitid`'s record both read that one value, so the
    two doors cannot classify one death two ways. Owner: `crates/abi`'s
    `signal_dumps_core`/`exit_chld`/`CLD_DUMPED`, `src/syscall/linux/abi.rs`'s `wait_status`,
    `src/sched/runtime/child_exit.rs`'s exit path, `src/sched/runtime/task.rs`'s
    `Retired`/`WaitEvent`, `src/sched/signal.rs`'s `SigInfo::child`, and `src/syscall/linux/proc.rs`'s
    `finish_wait`/`waitid`. Pins: `t-wait4` (a dumpable `SIGSEGV` death sets `WCOREDUMP`, `SIGTERM`
    clears it, and a `PR_SET_DUMPABLE 0` `SIGSEGV` clears it too) and `t-signalfd` (the same death is
    `CLD_DUMPED` through the descriptor, and `CLD_KILLED` when the child is non-dumpable), both green
    on this host first. The counterfactual is exact: with only the kernel half reverted and both
    programs kept, the sweep comes back `pass=178 fail=2 (180 built)`, the kernel's line unmoved at
    `mpos-kunit 32 passed, 0 failed` — and the two reds are exactly the new claims:
    `t-signalfd`'s `FAIL: as CLD_DUMPED, not CLD_KILLED, with the signal in ssi_status` and
    `t-wait4`'s `FAIL: and WCOREDUMP (0x80) is set for a core-dumping signal`.

33. **A `SIGCHLD` record carries the child's CPU in `si_utime`/`si_stime`.** Item 29's second
    residual, and the last of the `_sigchld` member this kernel left unfilled: Linux puts the
    child's CPU in the two `clock_t`s (in `USER_HZ` ticks) and this kernel wrote zero on the one
    record every door renders. **Measured whole on a current host first**
    (`.workbuddy/probe_sigchld_cputime.c`, Linux 7.2.9, `_SC_CLK_TCK` 100): a child that burns
    ~100 ms of CPU arrives as `si_utime` **9**, `si_stime` **0** through a blocked-`SIGCHLD`
    `sigwait`, through a `signalfd` read and through a `SA_SIGINFO` handler's frame — where a child
    that only exits is `0`/`0`, so the gap is a child that *worked* (the totals exist here; the
    record simply did not carry them). The probe also settled the door that does **not** carry them,
    and that is a finding rather than an omission: **`waitid` never fills these**. `struct
    waitid_info` has no such field, the syscall writes exactly six words
    (signo/errno/code/pid/uid/status), and it leaves the caller's other bytes alone — the probe
    reads `0` through every `waitid` for the same 100 ms child the record reports as 9. So
    `t-waitid`'s record stays zero, which is Linux's answer. **The fix is one conversion and one
    field pair carried to every renderer.** The `USER_HZ` conversion — `ticks * 100 / TICK_HZ`,
    truncated, Linux's `nsec_to_clock_t` one unit up — moves into the leaf crate
    (`abi::clock_ticks_to_user_hz`/`USER_HZ`, a `u128` intermediate so a long uptime cannot wrap,
    with a host test), and `proc.rs`'s private `times()` copy delegates to it so a `times(2)` total
    and a child's record cannot come to different units. `signal::SigInfo` gains `utime`/`stime`
    (already converted — the sender computes them once), the exit path fills them from the group's
    `Mm::cpu_all()` where the `mm` still holds the counters (the group's totals are Linux's
    `tsk->utime + tsk->signal->utime`, a dead thread already folded in), and the three doors write
    them: `signalfd`'s `ssi_utime`/`ssi_stime` at offsets 56/64, and the `siginfo_t` `_sigchld`
    member's `si_utime`/`si_stime` at 32/40 for a handler's frame and `sigtimedwait` alike. Owner:
    `crates/abi`'s `clock_ticks_to_user_hz`/`USER_HZ`, `src/sched/signal.rs`'s `SigInfo`,
    `src/sched/runtime/child_exit.rs`'s exit path, `arch/src/hal/mod.rs`'s `SignalInfo`, the two
    machines' `signal.rs` frame writers, `src/vfs/signalfd.rs`'s `read_at`, and
    `src/syscall/linux/proc.rs`'s `sigtimedwait`. Pin: `t-signalfd` — a child that only exits is
    `0`/`0` (a shape guard that passes either way), a child that burns 100 ms is `ssi_utime > 0`
    through the descriptor and `si_utime > 0` through the handler's frame (the counterfactual pair)
    — green on a current host first, plus a host test on the conversion in `mpos-abi`. The
    counterfactual is exact: with only the kernel half reverted and the pin kept, the sweep is
    `pass=179 fail=1 (180 built)` and the kernel's line unmoved at `mpos-kunit 32 passed, 0 failed`,
    with exactly the two new claims red — `FAIL: the record carries the child's CPU (ssi_utime > 0)`
    and `FAIL: and the frame carries the child's CPU (si_utime > 0)` (the `0`/`0` guard passes on
    both, being a description rather than a pin). Gate: `pass=180 fail=0 (180 built)`,
    `mpos-kunit 32 passed, 0 failed`, exit 33.

34. **`wait4`/`waitid` fill their `struct rusage`: the child's CPU, for every event they *report*.**
    Item 29's last residual, and the one a caller could not work around — both doors read their
    status/record arguments and never touched the fourth/fifth, so a program that asked for the
    child's CPU was handed a success and a record it had to fill itself. `rustix`'s `waitpid`
    hardcodes `NULL` there, which is why no sweep ever saw it. **Measured whole on a current host
    first** (`.workbuddy/probe_wait4_rusage.c`, Linux 7.2.9, `sizeof(struct rusage)` 144, fourteen
    scenarios each in its own forked child): a **death** fills it with the child's own **plus its own
    reaped children** — Linux's `RUSAGE_BOTH`, which a grandchild's 200 ms of CPU shows arriving
    through the child that reaped it — and the number is *exactly* what `getrusage(RUSAGE_CHILDREN)`
    reports once the reap has folded it in (0.199640 both ways); a **stop** (`WUNTRACED`, 0.149642)
    and a **resume** (`WCONTINUED`) fill it too, and so does `waitid` through `WEXITED`, `WSTOPPED`
    and `WCONTINUED`; a `WNOHANG` that finds nothing running, and an `ECHILD`, both leave a
    0xAA-seeded record **untouched**; a `NULL` rusage is "not interested", not an error. **The two
    doors also differ, and that is Linux's own order**: `wait4` writes `*status` inside the wait and
    copies the record afterwards, so an unwritable `rusage` pointer is `EFAULT` with the status word
    already written and the child already collected; `waitid` copies the record **before** it writes
    the six-word `siginfo_t`, so the same bad pointer leaves the caller's own bytes there. **The fix
    is one writer and one carried pair.** `proc.rs` gains one `write_rusage` (two opening `timeval`s
    off a tick count, the rest zeroed) which `handle_getrusage` now calls too, so
    seconds-and-microseconds off a tick count has one owner rather than two; `wait4` reads `args[3]`
    and `waitid` `args[4]`, and the write happens only where an event was *reported* — after
    `*status` in `finish_wait`/`write_wait_status`, before the record in `handle_waitid` (which also
    takes the caller's provenance for its own pointer checks now, as `wait4` always did). The CPU
    rides the event: `Retired` and all three `WaitEvent` variants gain `utime`/`stime` in raw
    internal ticks (a `rusage` is a microsecond `timeval`, so a `USER_HZ` pair would be the wrong
    unit), a death takes them off the record — read at the exit where the `mm` is still the live
    group's — and a stop or a resume reads them from the live table inside the scan's own borrow,
    through a new `Mm::cpu_both()` beside `cpu_all()`. The fourteen byte/page counters stay zero,
    which is the residual `getrusage` has carried since it landed. Owner:
    `src/syscall/linux/proc.rs`'s `handle_wait`/`handle_waitid`/`handle_getrusage`,
    `src/sched/runtime/{task,child_exit,child_mm,trace}.rs`. Pin: `t-wait4` — the death fill, the
    `RUSAGE_CHILDREN` delta the same reap moved (two independently truncated readings, and here the
    fold lands on the scheduler's next turn rather than inside the wait, so it is a bounded re-read:
    measured, 200 reaps land within 0..=2 us, `.workbuddy/probe_wait4_delta.c`), the unmeasured
    counters reading zero, a `WNOHANG` that leaves the seed, a `WUNTRACED` stop and a `WCONTINUED`
    resume each filled, and the `EFAULT` order with the child already collected — and `t-waitid`,
    the exit fill and the `WNOHANG` no-report half. Both green on a current host first. The record
    itself moved to the crate's lib (`mpos_user_tests::Rusage`) because a second and third program
    needed it, and the CPU burner with it. The two corners left standing are named in §5.2. Gate:
    `pass=180 fail=0 (180 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (the aarch64 arm, which
    these `[default]`-group pins also run under, is `pass=179 fail=0`, `mpos-kunit 32 passed, 0
    failed`, exit 0). The counterfactual is exact: with only the kernel half reverted (the six source
    files) and the pins kept, the sweep is
    `pass=178 fail=2 (180 built)` with the kernel's `mpos-kunit` line unmoved — the two red programs
    are `t-wait4` and `t-waitid`, and every red claim is new (the fill and its `RUSAGE_CHILDREN`
    delta, the counters, the `EFAULT`, the stop and the resume in `t-wait4`; the exit fill in
    `t-waitid`), while the shape guards — the `WNOHANG` no-report half, the stop/resume records, and
    `waitid`'s report — pass on both kernels.

35. **A full real-time signal queue is `EAGAIN` from `sigqueue`, and the limit is `RLIMIT_SIGPENDING`.**
    Stage C's last named divergence, and the one that lost data rather than merely reporting oddly:
    the per-task ring dropped a copy past `RT_QUEUE_LEN` and every door answered **success**, so a
    caller's queued signal was gone with nothing to tell it (the §3 rule-2 shape — silence where
    Linux has an errno). **Measured whole on a current host first**
    (`.workbuddy/probe_sigpending{1,5,7,8,9,10}.c`, Linux 7.2), one scenario per process so no
    scenario's quota leaked into the next: with `RLIMIT_SIGPENDING`'s soft limit at N, exactly N
    `sigqueue`s of one real-time number are taken and the **N+1th is `EAGAIN`** (N=8 and N=4, at
    errno 11); the quantity is the **user's total** — filling one number to the limit refuses a
    *different* number, and a same-uid child that queued nothing is refused while the parent holds
    the quota (the same child queues its own N again once the parent drains); the limit that governs
    is the **target's**, not the sender's (a sender lowered to 1 still delivered to a target whose
    limit was 4, while a send to *itself* was refused); a standard signal is never refused
    (`sigqueue(SIGUSR1)` succeeds at the limit and the bit is pending); and **`kill` never reports**
    — at the same moment it returns 0 and its extra copies are simply not queued. **The fix is one
    enum at the send door and one resource row.** `send_signal_info` now answers Linux's `Sent`
    (`Taken`/`NoTask`/`QueueFull`) instead of a bool, `enqueue` answers whether the ring took the
    copy, and the budget is decided before the mutable borrow because the count walks the table:
    `queued_for_user(ruid)` sums `RtQueue::queued()` over every task of the **target's** real uid,
    compared against the **target's** `rlimits[RLIMIT_SIGPENDING]`, after the "discarded anyway"
    rules (an ignored signal needs no slot — Linux's `prepare_signal` discards before the allocation
    that can fail). `rt_sigqueueinfo` maps `QueueFull` to `EAGAIN`; `kill`/`tgkill` and both
    group/process fan-outs keep answering 0, which is the measured split. The resource row gets a
    budget in `mpos_abi::default_rlimits`: row 11, `RT_QUEUE_LEN` both ways, because the records
    live in that ring — so `getrlimit` tells a caller the truth about this machine, and Linux's own
    default (`max_threads / 2`) is policy rather than ABI. Owner: `crates/abi`
    (`RLIMIT_SIGPENDING`, the default), `src/sched/runtime/child_signal.rs` (`Sent`, the budget
    check, `queued_for_user`), `src/sched/signal.rs` (`RtQueue::queued`), `src/syscall/linux/posix.rs`
    (the door's `EAGAIN`). Pin: `t-rt-signal-queue` — the fill stopping at the lowered limit, the
    copy past it `EAGAIN`, a second RT number meeting the same total, `kill` answering 0 at the same
    moment, a forked sibling with an empty queue refused too, and the soft limit restored — plus
    `abi.rs`'s `rlimits_report_the_startup_defaults_and_refuse_unknown_rows`, which reads row 11
    against `RT_QUEUE_LEN` itself, and `mpos_abi`'s default-table test. Note the pin lowers only the
    **soft** limit: the hard one cannot be raised back without privilege even on this host, so a
    claim that needed that would not be the program's to make. `t-rt-signal-queue`'s earlier three
    sections are what empty the queue first (their own drain claims are the evidence), which is why
    the fill meets the limit exactly. Gate: `pass=180 fail=0 (180 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 33 (aarch64, which these `[default]`-group pins also run under: `pass=179 fail=0`,
    `mpos-kunit 32 passed, 0 failed`). The counterfactual is exact: with only the kernel half
    reverted (the six source files) and the pin kept, the sweep is `pass=179 fail=1 (180 built)` with
    the kernel's `mpos-kunit` line unmoved — the red program is `t-rt-signal-queue`, and the four red
    claims are all new (the fill's stop, its `EAGAIN`, the second number, the sibling), while every
    shape guard (the FIFO drain, the two kills, the handler-per-copy, `kill`'s 0, the setrlimit and
    its restore) passes on both kernels. The one corner left standing — a caller that raises the
    limit past the ring still meets the ring — is named in §5.2.

36. **A device node asks for `CAP_MKNOD` — and only a device node does, asked last, of the caller's
    effective set.** Item 25 named the half of `mknod`'s answers it left standing: Linux refuses a
    device node to a caller without `CAP_MKNOD` (`EPERM`) where this kernel built the node for anyone,
    and it recorded *why* that was not a one-line gate — a `setuid` here did not re-derive the
    capability sets, so a dropped-privilege process would still have held the capability and the gate
    would have refused nobody. That prerequisite landed on its own (item 4: a uid/gid change
    re-derives the sets through `Credentials::on_identity_change`, the measured `cap_emulate_setxuid`
    rule `t-caps` drives), so this pass asks the question. **Measured whole on a current host first**
    (`.workbuddy/probe_mknod_cap.c`, Linux 7.2.9, an unprivileged caller, one scenario per answer):
    `S_IFCHR` and `S_IFBLK` are `EPERM`; `S_IFREG`, `S_IFIFO` and `S_IFSOCK` are **served** to a
    caller holding not one capability; the character node whose `dev` is `0` is served too — the
    *whiteout* overlayfs spells that way — and the exception is the character type's alone
    (`makedev(0, 1)`, `makedev(1, 0)` and a *block* node with the same `0` are all `EPERM`); and the
    refusal comes **after every one above**, so an existing name is still `EEXIST`, an unwritable
    directory still `EACCES` and a path that is not there still `ENOENT`. **The fix is one predicate
    and one gate per door**: `abi::mknod_needs_capability(mode, dev)` (the mode's type, and the low 32
    bits that are the whole of the uapi's `dev` argument), asked after `may_create_in` — the last
    question the call has — against `current_credentials().has_effective(CAP_MKNOD)`, with `crates/cred`
    gaining the constant. Owner: `crates/cred`, `src/syscall/linux/abi.rs`, and the two doors in
    `src/syscall/linux/fs/file.rs`. Pin: `t-mknod`'s capability section (14 new claims — the window's two
    directories, the name it finds already there, the node a *capable* caller makes, the lowering, the
    character and block refusals, the whiteout with the block-`0` refusal beside it, the three types
    that need none, the three ordering refusals, and the raise-back with the same node served again),
    which runs on both machines *and* on this host — where it makes the same claims as an ordinary
    user, since a host run is already without the capability — plus `abi.rs`'s
    `mknod_mode_field_picks_the_node_kind` for the predicate itself. Gate: x86_64 `pass=180 fail=0
    (180 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33; aarch64 `pass=179 fail=0`, `mpos-kunit 32
    passed, 0 failed`, and its `ok:` lines are identical to the first machine's. The counterfactual is
    exact: with the three kernel files reverted and the pin kept, the sweep is `pass=179 fail=1 (180
    built)` with the kernel's `mpos-kunit` line unmoved — the red program is `t-mknod` and the three
    red claims are the device refusals themselves (each answered `0`, the wrong-shape success this
    pass removes), while the whiteout, the three no-capability types and all three ordering claims
    stay green on both kernels. One corner is named in §5.2 rather than fixed: Linux asks
    `capable(CAP_MKNOD)`, which is the **initial** user namespace, where this kernel's checks ask the
    caller's own — measured (`.workbuddy/probe_mknod_userns.sh`), and the capability model's own named
    gap rather than this call's.

37. **A procfs/sysfs node reports its own word, and it is the word its own *shape* is worth.** §5.2's
    second and last named residual of item 24 ("every other proc/sys node still answers the kind's
    default where Linux answers the node's own mode") — the tree was a filesystem that keeps no
    modes, so every directory answered `0755`, every file `0644` and every link `0777`, whatever
    Linux's creator had said about the node. **Measured whole on a current host first**
    (`.workbuddy/probe_proc_node_modes.c`, Linux 7.2.9, `lstat` through `syscall(2)`, over every path
    this kernel's two trees build, plus a `find`-shaped tally of both trees by kind and word):
    a proc directory is **`0555`** — `/proc` itself, `/proc/sys/...`, `/proc/net`, `/proc/bus/pci`,
    every `/proc/<pid>` and `/proc/<pid>/task` — with the `w` bit absent from all three triads; a
    generated file nothing can write is **`0444`** (`/proc/version`, `/proc/meminfo`,
    `/proc/<pid>/status`, `/proc/net/tcp`, `/proc/sys/kernel/random/poolsize`); a file that *can* be
    written is **`0644`** (every sysctl this kernel builds — `hostname`, `protected_hardlinks`,
    `tcp_syncookies`, the whole `conf/*` block — beside `/proc/<pid>/oom_score_adj`, `uid_map`,
    `gid_map`, `timens_offsets` and the live configuration space `/proc/bus/pci/<bus>/<dev>.<func>`);
    `/proc/<pid>/environ` is **`0400`** where its neighbour `cmdline` is `0444`; `/proc/<pid>/ns` is
    **`0511`** (reachable by name, not listable — the door `setns` walks); and `/proc/cmdline`'s own
    `st_size` is **the length of the line it answers** (86 for this host's own, its whole content
    with the newline, where `/proc/<pid>/cmdline` beside it reports zero). In `/sys`, the tree's root
    is **`0555`** while every directory under it is the kind's `0755` (`class`, `bus`, `devices`,
    `dev`, `kernel`), a read-only attribute is **`0444`** (a block device's `dev`, `size` and
    `partition`; a PCI function's `vendor`, `device`, `class`, `irq`, `modalias`, `resource`,
    `power_state`; an input node's `name`) and the attributes that can be written — a device's
    `uevent`, which is the door `udevadm trigger` writes, and a function's `config` — are **`0644`**.
    **The fix is the word following the node, and it turns out to be Linux's own rule on both
    trees**: `proc_create` with no mode means `S_IRUGO`, `DEVICE_ATTR_RO` and `DEVICE_ATTR_RW` are
    the two sysfs words, and a directory carries what its creator asked for. So the *types* answer —
    `ProcCtl` and `ProcConfig` `0644`, `ProcFile` `0444`, `ProcDir` `0555`, the sysfs
    attribute types `0444`/`0644`, and `SysDir` the kind's own unless it is the root — with the
    nodes whose word is narrower than their shape saying it themselves: `env_file` for
    `/proc/<pid>/environ` (`0400`, both spellings under the process's directory and the thread's),
    `ProcDir::own(.., 0o511, false)` for `/proc/<pid>/ns`, and `file_of_size` for `/proc/cmdline`,
    which is the one generated file whose length is known before it is read — the command line cannot
    change after the boot — so its `st_size` is a fact rather than the generator being run during a
    listing. **Three things are named in §5.2 rather than hidden.** The nodes Linux makes writable
    and this kernel did not (`/proc/sys/kernel/pid_max`, `threads-max`, `pty/max`, `/proc/<pid>/comm`)
    report the **read-only** word, because the file refuses the write and the word now says so. The
    word is a *report* and not a gate: procfs keeps no modes to enforce, so an `0400` node is readable
    by any caller exactly as its `0444` neighbours are — the mode-less filesystem's own limit,
    unchanged by this pass. And three names differ in *kind* before the mode can: Linux answers `0777`
    links for `/proc/self`, `/proc/thread-self` and `/proc/mounts`, where this kernel answered a
    directory — **closed by item 46**, which deleted the `MirrorDir` node those two names were built
    from.     Owner: `src/vfs/procfs.rs`'s
    `ProcFile`/`ProcCtl`/`ProcDir`/`ProcConfig` and `src/vfs/sysfs.rs`'s
    `SysDir`/`SysFile`/`SysConfig`/`SysPciEvent`/`SysEventAttr`. Pin: `t-proc-modes` (18 claims at
    this landing - **20** since item 45, **35** since item 46, the same on both machines and green on
    this host first), plus two host unit tests for the mapping
    itself (`a_proc_nodes_own_mode_follows_its_shape`, and sysfs's
    `a_sysfs_node_reports_its_own_mode_rather_than_the_trees_kind_default`), and both fixtures — which
    block device, which PCI bus — are *asked for* by walking rather than named, so the same claims
    hold on a machine whose disks are `vda` and on one with `nvme0n1`.     **The pass' own first gate run
    found one more divergence, and it was named in §5.2 until item 45 closed it**: the two *mount
    names* (`/proc` and `/sys` themselves) went red at `0755` — the word of the directory the mount
    *covers*, because the no-follow resolver split the last name off its parent and never crossed
    the mount for it — so the program claimed those two through the door that does cross (`stat`)
    alone. Both doors agree now, so each of the two is claimed twice, and the count above moved with
    it. Gate: x86_64 `pass=181
    fail=0 (181 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33; the second machine's arm of the
    same gate is `pass=180 fail=0`, `mpos-kunit 32 passed, 0 failed`. The counterfactual is exact:
    with only the kernel half reverted (two source files) and the pin kept, the sweep is `pass=180
    fail=1 (181 built)` with the kernel's own line unmoved — the one red program is `t-proc-modes`
    and its twelve red claims are exactly the words this pass changed, while the six that pass on
    both kernels are the shape guards (the sysctl `0644`, `oom_score_adj` `0644`, the
    configuration-space window `0644`, `/sys/class` and `/sys/class/block` `0755`, and `uevent`
    `0644`).

38. **A capability is held *in* a namespace, and the two questions Linux asks about that are two
    different ones.** Item 36's named corner (`docs/uapi/` §5.2): `vfs_mknod` asks
    `capable(CAP_MKNOD)`, which is the **initial** user namespace, where this kernel asked the
    caller's own — so a caller that had entered a user namespace, was uid 0 in it and read the whole
    capability set back was served a device node that Linux refuses (§15.53's measurement,
    `.workbuddy/probe_mknod_userns.sh`). The corner was named as the *capability model's* rather
    than that call's, and this pass closes it where the model can: `Credentials` gains the
    distinction, and the four call sites whose Linux rule is `capable()` ask the new question.
    **Measured whole on a current host first** (`.workbuddy/probe_capable_scope.{c,sh}`, Linux
    7.2.9, one table per namespace — unprivileged in the initial one, uid 0 in a user namespace,
    uid 0 in a user namespace made *with* a network namespace, and root in a user namespace whose
    `0` is mapped to the outside root): a user-namespace root, `CapEff` `0x1ffffffffff`, is
    **served** `chroot` (`0`) and `unshare(CLONE_NEWNS)` (`0`) and **refused** `mknod(c 1 3)`
    (`EPERM`), `reboot` with an invalid magic (`EPERM`), `syslog`'s ring size (`EPERM`) and
    `FS_IOC_SETFLAGS` with `FS_IMMUTABLE_FL` on a file it owns (`EPERM`); the ordinary file beside
    the device node is served (`0`) and the `dev 0` whiteout too (`0`), so the refusal is the
    capability and not the namespace's unmapped ids. Three more answers fall out of the same table
    and are what makes the split visible rather than assumed: `AF_PACKET` is `EPERM` in a *bare*
    user namespace and **served** in one made with `CLONE_NEWNET` (Linux asks the *network*
    namespace's owner), `setns` into a mount namespace owned by the initial one is `EPERM` from a
    child namespace (Linux asks the namespace *being joined*), and a file owned by the outside user
    is `EACCES` to a caller whose uid 0 is mapped to the outside root (Linux asks the *file's*
    filesystem's namespace, `capable_wrt_inode_uidgid`). **The fix is one field and one predicate,
    and it is deliberately not a namespace pointer.** `Credentials::user_ns_initial` records
    whether the sets are held in the initial namespace at all — a flag rather than a level or an id,
    because every door here moves a caller *deeper* (`unshare`, `clone(CLONE_NEWUSER)`, and a
    `setns` that refuses a caller's own namespace with `EINVAL`), so it can only ever go from true
    to false; `on_enter_user_ns` clears it beside the grant it already makes. `capable(cap)` is the
    initial-namespace question and `has_effective(cap)` — whose doc now says which namespace it is
    asked of — stays the caller's own. The four sites switch: `mknod`'s two doors (`CAP_MKNOD`),
    `FS_IOC_SETFLAGS`' immutable and append bits (`CAP_LINUX_IMMUTABLE`), `reboot`
    (`CAP_SYS_BOOT`) and `syslog` (`CAP_SYSLOG`). **Three shapes stay named in §5.2** because this
    word cannot express them, each with the measurement above: `setns`'s and `AF_PACKET`'s ask the
    *object's* namespace, and `CAP_DAC_OVERRIDE` asks the *file's* filesystem's (the permission
    model's root branch, which asks uid 0 rather than a capability — its own item). Two smaller
    members of that family are named with them: `reboot`'s rule is one step finer than `capable()`
    (`ns_capable(task_active_pid_ns(current)->user_ns, …)`, which agrees with the initial namespace
    for every caller here) and asks the capability *before* the magic where this kernel asks the
    magic first; and `/proc/<pid>/uid_map`'s writer rule is the *parent* namespace's `CAP_SETUID`
    or the map's owner. Owner: `crates/cred`'s `Credentials::capable`/`user_ns_initial`, and the
    four gates (`src/syscall/linux/fs/file.rs`, `fs/ioctl.rs`, `proc.rs`). Pin: four programs, seven
    new claims (five refusals and two controls), all built on one new
    `mpos_user_tests::in_user_namespace` — `t-mknod` (a device node inside a user namespace is
    `EPERM`, an ordinary file in the same directory is served), `t-fsflags` (`IMMUTABLE` is `EPERM`
    there, `NODUMP` is served), `t-reboot-libc` (`CAD_OFF` is `EPERM` there) and `t-syslog` (the
    ring's size, and an unknown type *before* its `EINVAL`) — beside a host unit test in
    `crates/cred` for the mapping itself — the five refusals and the two controls, identical
    `ok:` lines on both machines. Gate: x86_64 `pass=181 fail=0 (181 built)`, `mpos-kunit` 32
    passed, 0 failed, exit 33 (181 staged, 11 left out); the second machine's arm of the same gate
    is `pass=180 fail=0`, `mpos-kunit` 32 passed, 0 failed (180 staged, 12 left out). The
    counterfactual is exact: with only the kernel half reverted (the leaf crate and the four
    gates) and the pins kept, the sweep is `pass=177 fail=4 (181 built)` with the kernel's own
    `mpos-kunit` line unmoved at 32 passed, 0 failed — the four red programs are exactly the four
    pins, and their five red claims are the refusals themselves, each served with the namespace's
    capability (`got Some(0)` for `IMMUTABLE`, the device node and `CAD_OFF`; `got Some(16384)`,
    the ring's size, for `syslog`; and `got Some(-22)` where the type check came first) — while
    the two controls (`an ordinary file in the same directory is served`, `a bit needing no
    capability is served there`) pass on both kernels, which is what keeps the refusals on the
    capability rather than on the namespace.

39. **The DAC override is two capabilities asked of the file's namespace, and never "is the caller
    root".** §5.2's third capability scope, and the one with teeth: this kernel's permission model
    answered a `chmod 000` file, a `0444` file's write and a `0000` directory's search with *uid 0*
    and a "some triad carries the bit" clause, where Linux asks
    `capable_wrt_inode_uidgid(idmap, inode, cap)` - **two** capabilities, of the *file's*
    filesystem's user namespace, with no triad clause at all. So a caller that had entered a user
    namespace (uid 0 in it, the whole set read back) read what Linux refuses it, and root could not
    read what Linux lets it. **Measured whole on a current host first**
    (`.workbuddy/probe_capable_scope.sh dac`, Linux 7.2.9: root, each capability dropped from the
    effective set in turn, mode `0000` and `0444` fixtures, and a `1777` directory whose file and
    directory both belong to another user). The table: `CAP_DAC_OVERRIDE` grants a read **and a
    write** on a non-directory whatever its bits are (`0` on both, and `EACCES` for both once it is
    dropped) and grants execute only where `S_IXUGO` (`access(X_OK)` is `EACCES` on a `0644` file
    even with the whole set, and `0` on another user's `0700` one); `CAP_DAC_READ_SEARCH` grants a
    read on its own (`access(R_OK)` on a `0000` file is `0` with `OVERRIDE` dropped, and with
    `READ_SEARCH` dropped the write answers too, so the two cover each other) and everything on a
    directory but a write; the compound masks follow from the execute clause (`R_OK|W_OK` is `0`
    where `R_OK|W_OK|X_OK` is `EACCES`); and with **both** dropped every row is `EACCES`. The
    sticky-bit rule is the same family: `check_sticky`'s last clause is
    `capable_wrt_inode_uidgid(dir, CAP_FOWNER)`, measured by removing another user's file from
    another user's `1777` directory - `0` with `CAP_FOWNER`, `EPERM` without it. **The fix is a
    caller-shaped identity and a rule that reads the table.** `may_as` takes a `Caller` (the two ids
    the door asked about, plus `Overrides` - the three capabilities this family consults) instead of
    a bare id pair, because the ids are the *door's* (`access(2)` asks about the real pair,
    `AT_EACCESS` about the effective one) while the capabilities are the caller's either way; the
    triads are asked first and only what they refuse is offered to `overrides_allow`, which is the
    measured per-kind rule; `may_remove`'s sticky half asks `CAP_FOWNER` rather than uid 0; and
    `root_override()` - the vestigial `true` whose doc admitted the predicate lived elsewhere - is
    gone, its prose folded into the two new types. `Overrides::of` reads the three capabilities
    through `Credentials::capable`, which is item 38's initial-namespace question, so the *scope*
    comes for free: a caller inside a user namespace holds none of them **for a file whose
    filesystem belongs to the machine**, which is what the probe's mapped-root block measured and
    what `t-caps` pins from inside a namespace. **Two things stay named in §5.2**: the *owner-shaped*
    calls (`chmod`, `chown`, the `utimes` family) still ask "is the caller root or the owner" where
    Linux asks `inode_owner_or_capable` = `fsuid == owner || capable_wrt_inode_uidgid(CAP_FOWNER)`,
    and `chmod`'s set-group-id rule asks `CAP_FSETID` beside the caller's groups where this kernel
    asks `uid 0` - the same shape, each with its own capability, and each needing its own
    measurement. Owner: `src/vfs/mod.rs`'s `Caller`/`Overrides`/`overrides_allow`/`may_as`, and
    `crates/cred`'s three new capability constants. Pin: `t-caps`'s DAC table (eleven claims: the
    three fixtures, the whole-set rows, each capability dropped, both dropped, the sticky pair and
    the namespace pair) and `t-faccessat`'s two `access(2)` rows that separate `R_OK` from
    `R_OK|W_OK|X_OK`, beside a host unit test that walks the same table
    (`the_dac_overrides_are_linuxs_two_capabilities_and_not_a_uid`, which replaced the old rule's
    own test). Gate: x86_64 `pass=181 fail=0 (181 built)`, `mpos-kunit` 32 passed, 0
    failed, exit 33 (181 staged, 11 left out); the second machine's arm of the same gate is
    `pass=180 fail=0`, `mpos-kunit` 32 passed, 0 failed (180 staged, 12 left out). The counterfactual is exact: with only the kernel
    half reverted (the leaf crate's constants, `may_as`, `may_remove` and the three test call sites
    - five files) and the pins kept, the sweep is `pass=179 fail=2 (181 built)` with the kernel's
    own `mpos-kunit` line unmoved at 32 passed, 0 failed - the two red programs are exactly
    `t-caps` and `t-faccessat`, and their nine red claims are the rows the old rule got wrong: five
    of them read `got 13` (EACCES where the capability grants `0` - the `0000` read, the `0444`
    write, and each of `OVERRIDE`- and `READ_SEARCH`-dropped's rows), one is `t-caps`'s namespace
    row **served** (`got 0`) where no capability is held in the file's namespace, one is the sticky
    row, and two are `t-faccessat`'s `R_OK` and `R_OK|W_OK` rows - while every control passes on
    both kernels (`access(X_OK)` on the `0644` file, the compound mask, "the whole set removes it",
    "the file the namespace's own uid owns is served", and the stranger's `EACCES`)..

40. **The owner-shaped calls ask a capability of the file's namespace, and never "is the caller
    root" — and what a `chown` takes away is nobody's to buy back.** The half of
    `inode_owner_or_capable` item 39 left for its own pass, and it is four rules rather than one.
    `chmod` on a node asks `inode_owner_or_capable` — the caller's `fsuid` is the node's owner, **or**
    `CAP_FOWNER` — where this kernel asked "`me.0 != 0 && me.0 != owner.0`"; the set-group-id bit
    that `chmod` may keep asks `in_group_or_capable`, whose stand-in is `CAP_FSETID` and *not* the uid
    `0` this kernel asked (an owner is outside a group it is not in); `chown` asks `CAP_CHOWN`, and
    the ids being the ones the node already holds is not a standing either; and the timestamps family
    is **two** questions, not one, split by the *shape* of the request. **Measured whole on a current
    host first** (`.workbuddy/probe_capable_scope.sh own` — a root caller with one capability dropped
    from the effective set at a time, every fixture belonging to uid 1000 so no row can pass on the id
    arm):

    ```text
                                          whole set  -FOWNER  -CHOWN  -FSETID
    chmod  0600 on another's file               0     EPERM      0       0
    chmod  0640 on my own file                  0       0        0       0
    chmod  2755 on my own, foreign group      2755    2755     2755    0755
    chown to a uid (another's file)             0       0     EPERM      0
    chgrp to a group (my own file)              0       0     EPERM      0
    utimensat, explicit times (another's)       0     EPERM      0       0
    utimensat, both clocks to *now*             0       0        0       0
    ```

    Two things are read off it beyond the capabilities themselves. The **owner arm is the uid**, not a
    privilege: a stranger that owns the node passes every row with the whole set gone, which is what
    keeps the refusals on the capability rather than on being root. And the two times rows are two
    different questions — `utimensat`'s null-pointer form is Linux's `ATTR_TOUCH` and accepts **write
    permission** (a `-FOWNER` caller touches another user's `0666` file, and its refusal on a `0600`
    one is `EACCES`, the bits' answer), while any pointer that names a clock is `ATTR_TIMES_SET` and
    is `EPERM` whatever the bits say; a pointer naming `UTIME_OMIT` for **both** clocks asks nothing
    at all and is served. **The `chown` half is not a caller's question in either direction**, which is
    what this kernel had wrong the other way: it stripped the set-uid and set-group-id bits only for a
    caller whose uid was not `0` and only when an id actually moved. Measured
    (`.workbuddy/probe_chown_bits.c`, the same root caller, four fixtures carrying both bits):

    ```text
                                          whole set  -FSETID  -CHOWN  -FOWNER
    chown(0,-1)  6755 own group (no change)    0755     0755    0755     0755
    chown(-1,-1) 6755 (nothing named at all)   0755     0755    0755     0755
    chown(0,-1)  6644 (sgid, no group-x)       2644     2644    2644     2644
    chown(0,-1)  4644 (suid, no exec)          0644     0644    0644     0644
    chown(0,-1)  6755 directory                6755     6755    6755     6755
    ```

    Naming an id is not required and neither is changing one — `chown(-1, -1)` takes them away too —
    so the test is not "did the owner move". The set-uid bit goes whatever else is true, even on a file
    with no execute bit at all. The set-group-id bit goes only where the group may execute, because
    without it the bit is mandatory locking and not an authority. And a directory keeps both. **The
    fix**: `crates/cred` gains `CAP_CHOWN` (0) and `CAP_FSETID` (4); `Overrides` grows the two beside
    the DAC pair and `Overrides::of` reads them through item 38's `capable`, so the scope is the
    file's namespace for free; `src/vfs/mod.rs` gains `may_setattr` (the ownership question, `EPERM`),
    `may_touch` (the same question with write permission standing in, `EACCES`) and
    `in_group_or_capable` (the group's stand-in), plus `S_IXGRP`, which is what separates an authority
    from a locking mark; and `src/syscall/linux/fs/file.rs` routes all three owners through them —
    `chmod_mode`, `chown_allowed`'s two arms (an owner may name the uid it already has, and the gid it
    already has or one of its own groups), and a `TimesShape`/`store_times` pair that the three
    timestamps handlers now share, so the two questions cannot drift apart the way three copies of
    them could. Owner: those three files. Pin: `t-caps`'s owner-shaped table (four rows — the whole
    set and each capability dropped — 18 claims, including the four `chown(-1, -1)` shapes) and
    `t-faccessat`'s stranger's rows (nine claims, which are the same two questions asked from a
    caller that owns nothing: a `chmod` and both `chown` arms are `EPERM`, and the two timestamps
    forms are `EACCES` and `EPERM` on the *same* world-writable file), beside the host unit test
    `the_owner_shaped_questions_are_a_capability_and_not_the_uid`. Gate: x86_64 `pass=181 fail=0 (181
    built)`, `mpos-kunit` 32 passed, 0 failed (181 staged, 11 left out); the second machine's arm is
    `pass=180 fail=0`, `mpos-kunit` 32 passed, 0 failed (180 staged, 12 left out); `t-caps`' 70 `ok:`
    lines and `t-faccessat`'s 27 are byte-identical across the two. The counterfactual is exact: with
    only the kernel half reverted (the leaf crate's two constants, the three `vfs` questions and the
    three call sites — three files) and the pins kept, the sweep is `pass=179 fail=2 (181 built)` with
    `mpos-kunit` unmoved at 32 passed, 0 failed — the two red programs are exactly `t-caps` and
    `t-faccessat`, and their sixteen red claims are the rows the old rule got wrong: eleven in
    `t-caps` (three of the four `chown(-1,-1)` shapes reading `got 6755`/`got 6644`/`got 4644`, the
    `FOWNER`-dropped `chmod` and explicit-times rows both `served`, the `FSETID`-dropped `2755`
    storing `2755`, the `CHOWN`-dropped uid row served, and the three "the chown still takes both
    bits" rows) and five in `t-faccessat` (both `chown` arms naming the ids already held, the
    `EACCES`/`EPERM` split between the two timestamps forms, and the explicit pair) — while every
    control passes on both kernels (`chmod 0640 on the caller's own file`, `the owner's own uid is
    still served`, "a world-writable file is served the now form", and `chmod on root's 0600 file is
    EPERM for a stranger`). **What stays named rather than landed** is the same capability's other
    door — a **write** to a set-uid file, which was §5.2's newest entry and is item 41's.

41. **A change to a file's bytes costs it the set-id bits, and `CAP_FSETID` is the only thing that
    spares them.** The third door of the capability item 40 put behind `chmod`'s set-group-id bit,
    and the reason a set-uid program cannot be handed to somebody by being edited in place: Linux's
    `file_remove_privs` runs the two bits off a **write** and a **resize** as it does off a `chown`,
    so the file's authority belongs to whoever owned it when the bytes were written and a caller that
    rewrites them has not been granted it. This kernel's write path touched neither bit.
    **Measured whole on a current host first** (`.workbuddy/probe_suid_write.c`, Linux 7.2.9, a root
    caller with the one capability dropped, fixtures carrying both bits):

    ```text
                                        whole set  -CAP_FSETID
    write 1 byte to a 4755 file            4755        0755
    write 1 byte to a 2755 file            2755        0755
    write 0 bytes to a 4755 file           4755        4755
    write 1 byte to another's 4666         4666        0666
    truncate a 4755 file to 0              4755        0755
    ftruncate a 4755 file up to 8          4755        0755
    open a 4755 file with O_TRUNC          4755        0755
    write 1 byte to a 2644 (no group-x)    2644        2644
    ```

    Three things beside the capability. A write that writes **nothing** costs nothing, so the strip
    is the bytes' and not the door's. Being the file's owner buys nothing and owning nothing costs
    nothing — the only question is the capability, and a caller writing another user's
    world-writable file loses the bits too, which is where this door differs from `chmod`'s (there,
    being in the group spares the bit). And the set-group-id-with-no-group-execute shape is the same
    exception the `chown` door honours and the `chmod` door does not. **The fix is one shared helper
    and four doors.** `crate::vfs::without_set_id` holds the two clauses — the set-uid bit always,
    the set-group-id bit only where the group may execute — so `chown_apply` and the write path
    cannot drift apart; `remove_privs` is Linux's `file_remove_privs`, asking `CAP_FSETID` through
    the same `Overrides` item 40 gave the two new bits, and `remove_privs_as` is its rule of a caller
    a host unit test can name. The four doors are [`File::write`](crate::vfs::file::File::write)
    (every write, `writev`, `pwrite` and `pwritev` funnels through it, and it asks only once `n` is
    not zero), `ftruncate`, `truncate` and the `O_TRUNC` of an `open`. The caller is asked only once
    the node is known to carry a bit, because `current_caller` reads the task's whole group set.
    Owner: `src/vfs/mod.rs` (the three helpers) and the two files above. Pin: `t-caps`' six new
    claims (the whole set's write and resize keep the bits; `CAP_FSETID` dropped costs a `4755` file
    both of them on a write *and* on a resize; the locking-mark shape is nobody's to take; and a
    `CAP_FOWNER`-dropped caller still keeps them, the control that puts the door on the right
    capability) beside the host unit test
    `a_change_to_the_bytes_costs_the_set_id_bits_and_only_fsetid_spares_them`. Gate: x86_64
    `pass=181 fail=0 (181 built)`, `mpos-kunit 32 passed, 0 failed` (181 staged, 11 left out); the
    second machine's arm is `pass=180 fail=0`, `mpos-kunit 32 passed, 0 failed` (180 staged, 12 left
    out); `t-caps`' 76 `ok:` lines are byte-identical across the two. The counterfactual is exact:
    with only the four call sites removed (two files) and the pins kept, the sweep is `pass=180
    fail=1 (181 built)` with `mpos-kunit` unmoved at 32 passed, 0 failed — the one red program is
    `t-caps` and its two red claims are exactly the discriminating ones (`FAIL: FSETID dropped: a
    write costs a 4755 file both set-id bits (got 4755)` and `FAIL: FSETID dropped: and so does a
    resize (got 4755)`), while every control passes on both kernels (the whole set's two rows and
    the `CAP_FOWNER`-dropped one). The locking-mark row is green on both by construction — there is
    nothing for either kernel to take — which is what makes the other two the ones that carry it.
    **Two shapes stay named in §5.2**: `fallocate`'s resize, which reaches `Inode::truncate` without
    going through either truncate door and was not measured; and a write through a mapping, which
    Linux does not strip either.

## item 42 — a capability question about a namespace object is asked of that object's owner

    The two object-shaped scopes item 38's measurement named in §5.2, landed together because they
    are one question: whose the *object* is. `setns` asked "does the caller hold `CAP_SYS_ADMIN`"
    where Linux asks `ns_capable(ns->user_ns, CAP_SYS_ADMIN)` about the namespace being **joined**,
    and `AF_PACKET` asked the same of the caller where Linux asks it of the network namespace the
    socket is opened in. Both doors had the same hole: a caller inside a bare user namespace,
    holding the whole set *in it*, was allowed where Linux refuses.
    **Measured whole on a current host first** (`.workbuddy/probe_ns_owner.c`, Linux 7.2.9, a root
    caller placed in each of four positions):

    ```text
                                      the machine's root   inside a bare user ns   inside one it made
    setns into the machine's mnt ns            0                     EPERM                  —
    setns into an mnt ns made inside it        —                       0                    0
    setns into a child user ns                 0                      —                     —
    setns into a sibling user ns               —                      EPERM                  —
    setns into a parent user ns                —                       —                   EPERM
    AF_PACKET (SOCK_DGRAM or SOCK_RAW)         0                     EPERM                  0
    ```

    The two rows that carry it are the second column's: the whole set held *in the caller's own
    namespace* is not the question, and the "already in it" no-op is not reached first either —
    the bare caller is refused joining the mount namespace it is standing in, which every other
    kind answers `0` for. The third row is the reason a namespace is a *grant* rather than a view:
    the machine's root joins a child user namespace it made, and a caller inside one joins a
    namespace made inside it, while a sibling and a parent are out of reach from below.
    **The fix is one table and one walk.** `vfs::ns::mint_owned` records whose each namespace is
    where it is *made* — a mount table's copy, a pid namespace, a uts namespace, a time namespace,
    a stack's number and a user namespace — and `Namespace::owner` reads it back, with `USER` its
    own owner, which is Linux's `ns_common::user_ns` and what separates the sibling row from the
    "made inside it" row. `userns::Caller::capable_in` is the walk Linux's `cap_capable` performs:
    the target itself is the caller's, and the answer is the set; the machine's own reached from
    below is not privileged over it; the caller *made* the next one down and holds every capability
    in it whether or not its own set carries the bit; otherwise up one rung. Both doors ask it
    through `sched::ns_capable_of`, which reads the three facts off the running task. Owner:
    `crates/userns` (the walk, its unit test, and `UserNs`'s new `parent`/`owner`), `src/vfs/ns.rs`
    (the table), `src/vfs/mount.rs` (`copy` takes its owner), the six mint sites, and the two doors
    in `src/syscall/linux/posix.rs` and `src/net/socket.rs`. Pin: `t-ns-setns`' new section — the
    child is `EPERM` on the no-op and `0` on the same no-op once the namespace is its own, and the
    machine's root is served joining a namespace its child user namespace owns (it *made* that
    namespace) — beside `t-ns-net`'s, where the same child holding the same set is `EPERM` on a
    packet socket in the machine's network namespace and served in one it made. **One thing this
    kernel cannot express yet, and §5.2 now names**: a *real* re-join, because a namespace
    descriptor here follows the process instead of pinning the namespace it named
    (`.workbuddy/probe_ns_fd_pin.c`: Linux answers the number the descriptor was opened in, this
    kernel answers the copy's). Gate: x86_64 `pass=181 fail=0 (181 built)`, `mpos-kunit 32 passed, 0
    failed` (11 staged out by `tests.toml`); the second machine's arm is `pass=180 fail=0`,
    `mpos-kunit 32 passed, 0 failed` (12 staged out). `t-ns-setns`' 43 `ok:` lines and `t-ns-net`'s
    66 are byte-identical across the two machines. The counterfactual is exact: with only the two
    doors put back to asking the caller's own set (two files, the owner table and every pin left in
    place) the sweep is `pass=179 fail=2` with `mpos-kunit` unmoved at 32 passed, 0 failed — the two
    red programs are the two pins and the two red claims are exactly the discriminating ones
    (`FAIL: and is EPERM joining the mount namespace it is standing in (the no-op every other kind
    serves)` and `FAIL: and a packet socket is EPERM while it stands in the machine's network
    namespace`), while the served halves and every control pass on both kernels.

## item 43 — the third raw door asks the network namespace's owner too

    The same question as item 42, in the *other* door that asks `CAP_NET_RAW`: `AF_INET`'s
    `SOCK_RAW` — the socket `ping` and a DHCP client open — asked the caller's own effective set
    (`Credentials::has_effective`), where Linux asks `ns_capable(sock_net(sk)->user_ns,
    CAP_NET_RAW)`, exactly as `AF_PACKET` does. The hole is the one item 42 closed on the other
    side of the same family: a caller inside a bare user namespace, holding the whole set *in it*,
    was served an `AF_INET` raw socket where Linux refuses one.
    **Measured whole on a current host first** (`.workbuddy/probe_rawip_ns.c`, Linux 7.2.9, a root
    caller in the same three positions item 42's probe used — the machine's own namespaces, a bare
    user namespace, and a user namespace that made a network namespace of its own):

    ```text
                                          the machine's root   inside a bare user ns   inside one with its own net ns
    socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)       0                     EPERM                          0
    socket(AF_PACKET, SOCK_DGRAM)                 0                     EPERM                          0
    ```

    The two rows are the point: the *same* three answers for the two doors, so the refusal is not
    about the capability the caller holds — the bare row holds the whole set *in its own*
    namespace — and not about what a user namespace may do, since the third column is served. It is
    asked of the owner, and the owner changes when the namespace does.
    **The fix is one helper and one call site.** `net::socket`'s new `net_ns_capable(cap)` reads
    the two facts the question needs — `vfs::ns::owner_of(current_net_ns)` and
    `sched::ns_capable_of`'s walk about it — and **both** raw doors now ask it, because a second
    copy of one question is a second place for the two to drift apart: the `AF_PACKET` door's
    inline pair of lines became the same call. The capability's *place* in the order does not move
    (it is still asked before the protocol is judged, `docs/uapi/` §4.1's row), and a root caller
    is privileged over the machine's own network namespace, so no answer that existed before this
    commit changed. Pin: `t-ns-net`'s raw-door section — the child that already walks a bare user
    namespace and then one of its own now asks both doors at both points, so one file pins the
    pair together (68 `ok:` lines, was 66). Gate: x86_64 `pass=181 fail=0 (181 built)`,
    `mpos-kunit 32 passed, 0 failed` (11 staged out by `tests.toml`); the second machine's arm is
    `pass=180 fail=0`, `mpos-kunit 32 passed, 0 failed` (12 staged out), and both programs' `ok:`
    lines are byte-identical across the two machines. The counterfactual is exact: with only this
    door put back to the caller's own set (one call site; the helper and the `AF_PACKET` door left
    as they are) the sweep is `pass=180 fail=1 (181 built)` with `mpos-kunit` unmoved at 32 passed,
    0 failed — the one red program is `t-ns-net` and its one red claim is exactly the
    discriminating one (`FAIL: and so is an AF_INET raw socket, the same question asked of the same
    owner`), while the served half (`and a raw IP socket too`) passes on both kernels, which is
    what keeps the claim on the namespace's owner rather than on the socket's type.

## item 44 — a namespace descriptor *is* the namespace, so it pins what it named

    §5.2's last named gap on the namespace rung, and the one whose absence was visible from the
    outside: `/proc/<pid>/ns/<kind>` opened here was the procfs *link*, so `setns` re-resolved
    `(pid, entry)` and joined whatever that process was in **then**. Linux hands back an `nsfs`
    inode whose private data is the namespace itself, so the descriptor names the namespace it was
    opened in for as long as it is held — which is what makes `setns(fd)` a join rather than a
    question about where the maker stands now, and what lets a namespace outlive the process that
    made it by descriptor alone.
    **Measured whole on a current host first**, in two probes, because the gap was two shapes.
    `.workbuddy/probe_ns_fd_stat.c` (Linux 7.2.9, run as root *and* as an unprivileged caller) for
    the file:

    ```text
    lstat /proc/self/ns/mnt        -> symlink 0777 size 0 ino 126330      (the procfs link's own number)
    stat  /proc/self/ns/mnt        -> regular 0444 size 0 ino 4026531832  (the namespace's own number)
    fstat of the descriptor        -> regular 0444 size 0 ino 4026531832
    read  -> EINVAL                 lseek -> ESPIPE
    open O_WRONLY / O_RDWR / O_RDONLY|O_TRUNC -> EPERM (root and unprivileged alike)
    open O_RDONLY|O_APPEND         -> served
    ```

    and `.workbuddy/probe_ns_fd_pin.c` for the object: open `/proc/self/ns/mnt` (the link reads
    `mnt:[4026531832]`), `unshare(CLONE_NEWNS)` (into a copy, `mnt:[4026532932]`), then `setns`
    through the descriptor — `0`, and the readlink is `mnt:[4026531832]` **again**. The identity is
    the object's own number, which is why `/proc/self/ns/mnt` and `/proc/1/ns/mnt` report one
    `st_ino` — `4026531832` in both rows above.
    **The fix is one file kind and one line of the walk.** `vfs::ns::NsFile` holds the resolved
    `Namespace` and is what the walk denotes for a followed namespace name (the substitution beside
    the descriptor link's, in `src/vfs/mount.rs`); it reports the namespace's own number, kind
    `File`, size 0 and mode `0444`, refuses a write-ish open with `EPERM` in the place
    `Inode::open_replacement` exists for, answers `EINVAL` for a read and `ESPIPE` for a seek, and
    hands `as_namespace()` the object it pinned — which is what `setns` and `ip link set … netns
    <fd>` read, unchanged. `lstat`/`readlink` are untouched: they stop at the link, which is the
    split Linux makes. Pin: `t-ns-setns`' descriptor section (9 claims: the `lstat` link, the
    followed file, the descriptor, the inode number, the write-ish refusal with `O_APPEND` as its
    control, and the three-readlink pin with the unshare and the join's own control), beside the
    host unit test `an_opened_namespace_descriptor_has_the_shape_a_host_measured`, which walks the
    table above with no machine under it. Gate: x86_64 `pass=181 fail=0 (181 built)`,
    `mpos-kunit 32 passed, 0 failed` (11 staged out by `tests.toml`); aarch64 `pass=180 fail=0`,
    `mpos-kunit 32 passed, 0 failed` (12 staged out). `t-ns-setns` is 52 claims now (was 43), and
    its `ok:` lines are byte-identical across the two machines. The counterfactual is exact: with
    only the walk's substitution put back (one file, `NsFile` and every pin left in place) the sweep
    is `pass=180 fail=1` with `mpos-kunit` unmoved — the one red program is `t-ns-setns` and its
    **five** red claims are exactly the discriminating ones (`and following it gives the namespace
    file, which is what stat reports`, `and the descriptor that open gave is that same regular file,
    0444 and no size`, `whose inode number is the namespace's own, the number the link's text
    carries`, `and a write-ish open of the name is EPERM, whichever word asked`, and `and lands in
    the namespace the descriptor named, not in the copy`) — while every control passes on both
    kernels: the `lstat` link, `O_APPEND`, the unshare moving the caller, the join being served, and
    the pre-existing claims of the whole file. One shape stays named in §5.2 with the same
    measurement: the mode is still a *report* rather than a gate (procfs keeps no modes), so the
    write-ish refusal here is the inode's own rule and not the bits'.

## item 45 — a mount point answers the mount, and a name a mount sits on may not be taken away

    §5.2's last named divergence in the filesystem's own resolution, and the one its own entry said
    was "not a two-line fix" because one helper answered two questions. Both halves are the same
    fact seen from two sides: a mount **covers** the name it sits on. **Measured whole on a current
    host first** (`.workbuddy/probe_mountpoint_doors.c`, Linux 7.2.9, root; transcript beside it),
    with a tmpfs over a `0755` directory and a file bind, so one mode word and one `st_dev` say
    which of the two inodes answered:

    * **the reading and altering doors cross the mount** - `lstat`, `stat`, `statx` with *and*
      without `AT_SYMLINK_NOFOLLOW`, `newfstatat`, `open`, `faccessat2`, `chmod` and `utimensat` all
      answer the mounted root (`dev 61 ino 1 mode 0705` against the covered `dev 52 ino ... 0755`),
      and `open(mount point, O_NOFOLLOW)` succeeds. The crossing is **not** what `LOOKUP_NOFOLLOW`
      turns off: the flag is about links, and a mount is stepped into whatever the flag says;
    * **a plain bind covers it up** - inside `mount --bind` of the parent (no `MS_REC` clones no
      submounts) the *same* name answers the covered directory again (`ino 9433 mode 0755` where the
      outer spelling answers the tmpfs `ino 1 mode 0705`), which is why the crossing has to come
      from the walk's own state rather than from a lookup in the table;
    * **the make-or-remove doors refuse the name** - `rmdir`, `unlinkat(AT_REMOVEDIR)` and `rename`
      are `EBUSY` (16); `mkdir`, `mknod` and `symlink` at the name are `EEXIST` (17) because it is
      taken;
    * **and the type and permission questions come first** - `rename(file, mounted dir)` is `EISDIR`
      (21) where `rename(dir, mounted dir)` is `EBUSY`, `rename(mounted dir, file)` is `ENOTDIR`
      (20) where `rename(mounted dir, dir)` is `EBUSY`, and an unprivileged caller in a `0555`
      directory holding a mount point reads **`EACCES` (13)** for all three of `rmdir`, `unlink` and
      `rename` - while `mkdir` of that taken name is still `EEXIST` (17), so the order is per-door
      rather than one rule.

    **The fix is one mode on the walk and one question in the refusal.** `resolve_walk` gains a
    trailing-link mode (`LastLink::Follow`/`NoFollow`, Linux's `LOOKUP_FOLLOW` against
    `LOOKUP_NOFOLLOW` for the last name only), and the no-follow resolver becomes that walk, so the
    crossing happens with the walk's own bind state and the payload-link substitution is skipped
    exactly as `lstat` and `readlink` need (`src/vfs/mount.rs`). The old parent-and-lookup body
    survives as `resolve_entry`/`resolve_entry_at` - "the entry this directory holds under the
    name" - for the three doors whose question is a *name* rather than a path: `mkdirat`'s,
    `mknod`'s and `mknodat`'s `EEXIST` probes, which reach it through the caller's table. The
    refusal is `MountTable::is_mount_point` (a fact about the **node**, which is Linux's own
    `d_mountpoint` rather than an approximation of it), asked at the *end* of `may_remove` - after
    the immutable/append-only and permission halves, which is the measured `EACCES`-before-`EBUSY`
    order - so `unlink`, `rmdir` and their callers refuse together; and `renameat2` gains the type
    pair the measurement names, before its existing `may_remove`/`may_create_in`, then the refusal
    on either end. Owner: `src/vfs/mount.rs` (the walk's mode, the entry door, the mount-point
    question), `src/vfs/mod.rs` (`may_remove`), `src/syscall/linux/fs/file.rs` (three probes moved
    to the entry door; `renameat2`). Pin: **`t-mountpoint`** - a new program that mounts its own
    tmpfs (whose root this kernel makes `01777`, so it needs no fixture from the boot), **37 claims**
    on both machines and green on this host first: the reading doors and their `st_dev`, the five
    refusals, the type pair twice (over a plain directory and over a mount point), the altering door
    on the mounted root with the covered directory's own stamp intact after the umount, the alias's
    covered answer beside the outer spelling's mounted one, the same name's `EBUSY` inside the alias,
    and four answers from a forked **stranger**. Beside it `t-proc-modes` claims `/proc` and `/sys`
    through **both** doors now (**20** claims, was 18), and three host unit tests hold the mechanism
    with no machine under it. Gate: x86_64 `pass=182 fail=0 (182 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 33 (182 staged, 11 left out by `tests.toml`); the second machine's arm is
    `pass=181 fail=0`, `mpos-kunit 32 passed, 0 failed` (181 staged, 12 left out). Both programs'
    `ok:` lines are byte-identical across the two machines (diffed, carriage returns aside).
    Counterfactual, one mechanism at a time: with the no-follow resolver put back to the entry
    resolver (one method, every pin left in place) the sweep is `pass=180 fail=2 (182 built)` with
    `mpos-kunit` unmoved - the two red programs are `t-mountpoint` and `t-proc-modes` and the
    **seven** red claims are exactly the crossing ones (`lstat of the mount point answers the mounted
    root`, `fstatat with AT_SYMLINK_NOFOLLOW answers the mounted root`, `statx with
    AT_SYMLINK_NOFOLLOW answers it too`, `while the outer spelling still answers the mounted root`,
    `and its own stamp, untouched by the mount that covered it` - the altering door had stamped the
    *covered* directory - and `/proc`/`/sys` `reports 0555 for a directory (got 0755)`), while `stat`,
    `open`, every refusal and both alias controls pass on both kernels. With the refusal taken back
    out (`may_remove`'s question and `renameat2`'s new-end one) the sweep is `pass=181 fail=1 (182
    built)`, one red program, and the first red claim is the one that matters: `rmdir of the mount
    point (ok)` - the name went, and the nine claims after it are the consequence: `rename of the
    mount point` is `ENOENT`, `mkdir` of the freed name **succeeds**, and the umount has no mount
    point to peel, so the tree a later `t-` program would walk is one a mount was lifted out of.
    A third run places the reverted method on the *other* half of the pair (`resolve_no_follow`, the
    machine-rooted walk, instead of the caller-shaped one the doors use) and shows the host stage
    catching it alone: `556 passed; 2 failed`, the two unit tests named for the crossing, before any
    QEMU boots.

    **Two findings from the probe stay named rather than served.** A plain bind's clone is a
    *different mount*, so Linux answers **`EXDEV` (18)** for a `rename` between a view and the outer
    tree (`rename(alias2/e, outer/...)` measured) - the check is `old_path.mnt != new_path.mnt`, and
    this kernel has no per-view mount identity to compare, so a rename across an alias is still
    served here. And a **file mount point** - the one case where `unlink`'s half of the refusal is
    reachable on Linux - cannot exist in this kernel at all, because every mount it can make lands
    on a directory (`bind` of a file is `EINVAL`, `mount_new` is `ENOTDIR`); the question is asked of
    the node anyway, because it is Linux's rule and it costs a comparison.

## item 46 — the three procfs names Linux makes links are links

    §5.2's third and last named consequence of item 37, and the shortest one to state: Linux answers a
    `0777` **symlink** for `/proc/self`, `/proc/thread-self` and `/proc/mounts`, where this kernel
    answered a directory (a `MirrorDir`, forwarding to the reading process's own directory) for the
    first two and a plain `0444` file for the third. Item 37 named it as a consequence rather than
    fixing it - "their word follows from the shape and stays as it is" - and `t-proc-modes`' header
    carried the same sentence from the other side, which is what made this the last of that pass'
    three.

    **Measured whole on a current host first** (`.workbuddy/probe_proc_self_links.c`, Linux 7.2.9,
    root for the namespace half), and the fourth question is the one a table cannot guess:

    * **the shape and the target** - `lstat` is `0777` for each of the three; `readlink` answers
      `95884`, `95884/task/95884` and `self/mounts`; `stat` *follows* them to a directory (`0555`), a
      directory and a regular file (`0444`); and a `/proc` listing labels all three `DT_LNK`;
    * **the number is the reader's, in the pid namespace the *procfs instance* belongs to** - a
      process whose `getpid()` is `1` inside a fresh pid namespace still reads `95886` from the
      *machine's* `/proc`, and reads `1` from a `/proc` it mounts itself inside that namespace. So
      the rule is `pid_nr_ns(pid, proc_pid_ns(inode))` rather than "the reader's own number";
    * **and the size follows how the name was made, not what it answers** - `st_size` is **0** for
      `/proc/self` and `/proc/thread-self` (registered with an empty target, whatever number they
      hand back) and **11** for `/proc/mounts`, the length of the text it was registered with.

    **The fix is a node that was already here.** `ProcLink` is "a link whose target is a fact rather
    than a stored string" - `exe`, `cwd`, `root`, `fd/<n>`, `ns/<entry>` - and its mode is `0777` for
    everything but a descriptor's second name, which is this measurement's answer. Two things were
    added to it: a `registered` length (the size Linux sets from the text a name was *made* with, so
    the two live names report 0 and `/proc/mounts` reports 11) and a `fixed_link(target)` helper for
    the one name whose target is a path. `/proc/self` is then `link(|| reader_number(current_pid()))`
    and `/proc/thread-self` is `link(|| "<pid>/task/<tid>")`, both through **the number this tree
    names the reader by** (the pid namespace's "the number is the reader's" rule) so the link and the
    directory it leads to cannot disagree; `MirrorDir` is deleted with its last two callers. Nothing
    else moved: the walk follows a plain link by splicing its text beside the link's own parent, so
    every name *through* the link (`/proc/self/status`, `/proc/self/fd/N`, `/proc/self/mountinfo`,
    `/dev/stdin`'s `/proc/self/fd/0`) reaches the same node it reached before, and `readlink`'s
    shortcut for `exe` and `fd/<n>` compares path *text* and is untouched.

    Pin: `t-proc-modes`, which owns the shape of every node in this tree and used to *exclude* these
    three by name - **15 new claims**, the kind and mode of each, each followed answer, the target
    text against the reader's own `getpid`/`gettid`, the registered `st_size`, and the `DT_LNK` a
    listing hands out (read through `libc::readdir`, since `d_type` is a different answer from
    `lstat`), beside two host unit tests for the node itself. 35 claims now (was 20), the same on both
    machines and green on this host first. The reader's *own* number is not the same on two machines,
    so the two claims that name it put the values in the label only when they disagree - a passing
    line has to stay diffable. Gate: x86_64 `pass=182 fail=0 (182 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 33 (182 staged, 11 left out by `tests.toml`) - run twice, the same line both times;
    the second machine's arm is `pass=181 fail=0`, `mpos-kunit 32 passed, 0 failed` (181 staged, 12
    left out). The counterfactual is exact: with only `src/vfs/procfs.rs` put back (the pin kept) the
    sweep is `pass=181 fail=1 (182 built)` with `mpos-kunit` unmoved - the one red program is
    `t-proc-modes`, and its **ten** red claims are the old shapes by name (`/proc/self reports 0777
    for a link (got 0555 for a directory)`, the same for `thread-self`, `/proc/mounts reports 0777 for
    a link (got 0444 for a file)`, all three `readlink`s `got None`, `/proc/mounts`'s `st_size` `got
    Some(0), want 11`, and the three listings `got Some(4)`/`Some(4)`/`Some(8)` - `DT_DIR`, `DT_DIR`,
    `DT_REG`) - while every control passes on both kernels: the two followed-door claims for the
    directories, the two `st_size == 0` claims (zero either way, which is why `/proc/mounts`'s 11 is
    the discriminating one), and every `/proc/self/...` claim the file already had.

## item 47 — a directory is not a byte stream: `EISDIR` from its read doors, and `EINVAL` from `sendfile`'s source

    §5.2's newest entry, and it began as one line of a boot log rather than as a plan: reading a
    *symlink to a directory* to see where it pointed printed `cat: read error: Not supported` beside
    the kernel's own `mpos: syscall 0 (fd 3) -> EOPNOTSUPP` and, for the `sendfile` busybox's `cat`
    tries first, `mpos: syscall 40 (fd 1) -> EOPNOTSUPP`. Linux's answer for the same two calls is
    measured, and it is two *different* words - which is the whole item, because `EOPNOTSUPP` tells
    a caller "this kind of file cannot be read that way" where Linux's tells it *what it named*.

    **Measured whole on a current host first** (`.workbuddy/probe_dir_io.c`, Linux 7.2.9, one line
    per door, because two cases were slices of a table):

    * **the read doors are `EISDIR` (21)** - `read(dir, buf, 16)`, `pread64(dir, buf, 16, 0)`,
      `readv(dir, iov)`, `preadv(dir, iov, 0)` - and so is `read(dir, buf, 0)`: the *kind* is asked
      before the count, where the same zero count on a regular file answers `0`. Linux's owner for
      it is the directory's own file operations (`read = generic_read_dir`, whose body is
      `return -EISDIR`), so the rule is the kind's rather than a door's;
    * **`sendfile(out, dir, NULL, 16)` is `EINVAL` (22)** - the copy goes through the *source's*
      splice path and a directory has none, so the caller hears "this source cannot be sent" rather
      than the read's "that is a directory" - while `sendfile(out, dir, NULL, 0)` is **`0`**: this
      call's own count is asked before its kind, which is why its answer is not the read's;
    * **the write half has no such word to give, and that is why it is a control rather than a
      fourth case**: a directory cannot be *opened* writable at all (`open(dir, O_RDWR)` and
      `open(dir, O_WRONLY)` are both `EISDIR`, measured, refused where the description is made), so
      the only write a caller can reach is a read-only description's `EBADF` (9). The same
      measurement is why `copy_file_range` and `splice` are untouched: both already ask the source's
      kind first and answer `EINVAL` (`copy_file_range` requires `kind == File`, `splice` a pipe on
      one end), which is Linux's answer for a directory beside `sendfile`'s.

    **The fix is two conditions on the two doors the probe named.** `File::read` asks the kind
    after the mode check and *before* the empty-buffer short-circuit - the measured order, and the
    one door `read`, `pread64`, `readv` and `preadv` all arrive at, which is why the answer is
    spelled once rather than in four callers; `handle_sendfile` asks its own kind question after its
    own count-0 return and before its loop, so the answer does not depend on how the chunking falls.
    Nothing else moved: the write half is unreachable by construction (above), and both copy doors
    were already right.

    Pin: `t-dir-io` (**14 claims**), green on a current host first and with no per-machine branch -
    each claim is Linux's answer, so a host run is the measurement and the sweep is the pin - beside
    the host unit test `a_directory_has_no_byte_reader`, which asks the kind question and its
    zero-length form against a directory and against a file. Gate: x86_64 `pass=183 fail=0 (183
    built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (183 staged, 11 left out by `tests.toml`);
    the second machine's arm is `pass=182 fail=0`, `mpos-kunit 32 passed, 0 failed` (182 staged, 12
    left out). **The counterfactual is two measurements, because the host stage is a pin too**: with
    both conditions removed and both pins kept, `cargo xtask test` stops at stage 1 (`560 passed; 1
    failed` - the unit test, by name) and the sweep never starts; with the two host assertions held
    aside so the sweep could reach the guest, the same tree is `pass=182 fail=1 (183 built)` with
    `FAIL t-dir-io` and **six** red claims by name - the five read doors and `sendfile`'s `EINVAL` -
    while every control passes on both kernels (`sendfile`'s count-0 `0`, the two open refusals, the
    read-only description's `EBADF` and all four claims against a regular file).

## item 48 — the device tree removes a directory it made: `rmdir` served, and `unlink` refuses one with `EISDIR`

    §5.2's entry, and it began as one line of a boot log: `mpos: syscall 84 (path
    /dev/dri/by-path) -> EOPNOTSUPP`, once per boot. That is udev doing to that directory what it
    does at every boot - remove it, then publish the `by-path` links again - and the boot went on to
    publish them anyway, leaving a directory that only ever grew. Linux's devtmpfs serves the pair;
    this tree served the `mkdir` (`DevDirNode`'s `create`) and had **no `rmdir` at all**, and its
    `unlink` was a plain map removal that took a *directory* as readily as a node - so the two doors
    were not a split, and the trait's default `Unsupported` answered the call udev makes.

    **Measured whole on a current host first** (`.workbuddy/probe_devfs_doors.c`, Linux 7.2.9, run as
    root on a machine whose `/dev` *is* a devtmpfs, so the answers are devtmpfs's rather than a
    tmpfs's over one):

    * **the make-and-remove pair is served at both levels** - `mkdir("/dev/probe-mkdir")` `0`, the
      same name again `EEXIST` (17), `mkdir` under a caller-made directory `0` **and under a driver's
      own `/dev/dri` `0`**; `rmdir` of the empty one `0` at either level, and of one that still holds
      something `ENOTEMPTY` (39);
    * **the two doors answer differently for every kind that is not a directory**: a character node
      (`/dev/null`), a regular file and a symlink to a directory are all `ENOTDIR` (20) from `rmdir` -
      it does not follow the last name, so the *link's* kind answers - where `unlink` serves all three
      (`0`);
    * **a directory is `EISDIR` (21) from `unlink`**, which is the word that makes the pair a split
      rather than two spellings of one removal;
    * **a name nothing ever made is `ENOENT` (2)** from either door, and the two cleanup controls
      answer `2` again - nothing above was left behind;
    * **the mount-shaped rows are the mount layer's**, measured here as controls: `rmdir("/dev")` and
      `rmdir("/dev/shm")` are `EBUSY` (16) and `unlink("/dev/shm")` is `EISDIR` (21), which is what
      `may_remove` already answers (item 45) before the filesystem is asked anything.

    **The fix is one helper and four methods.** `check_empty_dir` asks the two questions in Linux's
    order - kind (`ENOTDIR`), then emptiness (`ENOTEMPTY`) - and both `rmdir`s call it *before* the
    name goes, which is what makes a refused `rmdir` leave the directory where it was; the two
    `unlink`s gained the kind check they never had (`EISDIR`), the rest of their bodies unchanged, so
    a driver-registered *name* still goes (mdev's link script unlinks those; the driver keeps the
    device, the name was one way to reach it) while a directory cannot be taken through that door at
    all. Nothing in the mount layer moved: the two `EBUSY` rows above were already right.

    Pin: `t-devtmpfs`'s removal section - **14 new claims** (44 in the file, was 30), the six-per-node
    clock section it already owned untouched - beside two host unit tests over the same table
    (`a_made_directory_is_removable_and_unlink_refuses_one`: mkdir, `EEXIST`, the inode the name
    reaches, `unlink`'s `EISDIR` taking nothing away, `rmdir` served then `ENOENT`;
    `the_removal_doors_answer_for_the_other_kinds`: `ENOTEMPTY` leaving the name, the nested
    `DevDirNode` door, a device node's `ENOTDIR`, a symlink to a directory's `ENOTDIR` with `unlink`
    serving it, and `ENOENT`). The nested half is claimed on the guest only where a display driver
    published `/dev/dri` - else the program notes its absence rather than failing - because
    `/dev/dri` is the driver's directory and `DevDirNode`'s door is a different one from `DevRoot`'s,
    which is the half udev's pass lands on. All 44 claims are byte-identical across the two machines
    (diffed, carriage returns aside).

    Gate: x86_64 `pass=183 fail=0 (183 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (183 staged,
    11 left out by `tests.toml`); the second machine's arm is `pass=182 fail=0`, `mpos-kunit 32
    passed, 0 failed` (182 staged, 12 left out).

    **The boot it came from is quiet now, and that is a measurement of its own**: `cargo xtask run
    --mode terminal` on the installed image runs OpenRC's "Populating /dev with existing devices
    through uevents" and "Waiting for uevents to be processed", and afterwards `/dev/dri/by-path`
    holds both links (`pci-0000:00:04.0-card -> ../card0`, `...-render -> ../renderD128`) with **no
    `mpos: syscall` line anywhere in the console** - the pass that makes that directory and then
    removes it again ran, and neither half was refused.

    Counterfactual, two measurements because the host stage is a pin too: with the two `rmdir`s and
    the two `unlink`s put back to their old bodies (the pins kept - both host tests and the whole
    `t-devtmpfs` section), `cargo xtask test` stops at stage 1 with `561 passed; 2 failed`, the two
    unit tests named by name, and the sweep never starts; with those two held aside (`#[ignore]`) so
    the sweep could reach the guest, the same tree is `pass=182 fail=1 (183 built)` with `FAIL
    t-devtmpfs` and **nine** red claims, the first of them the root: `unlink of a directory is
    EISDIR` - the old body *took* the directory - and the eight after it are its cascade, which is
    what makes the split the item rather than the missing method: `... and the refused name is still
    there`, `mkdir of a child inside it is served` (the parent was gone), `rmdir of a directory that
    still holds something is ENOTEMPTY`, `... and that refusal left it where it was`, `rmdir of the
    empty child is served`, `and then rmdir of the directory itself is served`, `rmdir of a device
    node is ENOTDIR` (the trait's default), and `and rmdir of it, the pass udev runs
    (Err(Os { code: 95, kind: Unsupported, message: "Not supported" }))` - the boot's own
    `EOPNOTSUPP`, in the guest's words, on the exact claim udev's pass lands on. The other 35 claims
    - every clock claim, `mkdir` served, `EEXIST`, `so the name it removed is gone`, `ENOENT` and
    `rmdir of a name nothing made` - pass on both kernels, which is what keeps the reds on the
    removal doors rather than on the directory the test makes.

## item 49 — the `F_GETFL` word is the caller's own, minus what the open spends

    §5.2's entry, and the second half of one item 21 already half-closed: the *access mode* has been
    in the word since 2026-10-16, and what stayed missing was the rest of `filp->f_flags`. Linux
    answers the word the open recorded, so a caller that compares `F_GETFL` against what it opened
    with agrees; this kernel answered the mode plus `O_APPEND` and `O_NONBLOCK` and nothing else, so
    the comparison disagreed on every other flag the caller had named.

    **Measured whole on a current host first** (`.workbuddy/probe_getfl.c`, Linux 7.2.11, every call
    through `syscall(2)` so no libc's rewrite is mistaken for the kernel's rule):

    * **what the open keeps**: `open(O_RDONLY)` `0x8000`, `open(O_WRONLY)` `0x8001`,
      `open(O_RDWR)` `0x8002` - and `0x8000` is `O_LARGEFILE`, which that kernel *adds* to every
      path open (`0o100000` on x86_64, `0o20000` on arm64: the one member this kernel does not
      carry, and `t-open-flags` pins that it is ignored here on both machines);
    * **every flag the caller named comes back**: `open(dir, O_RDONLY|O_DIRECTORY)` `0x18000`,
      `open(O_RDONLY|O_NOFOLLOW)` `0x28000` - so the bit is *not* open-only, which `abi.rs`'s
      comment about it used to say it was - `open(O_PATH)` `0x200000`,
      `open(O_WRONLY|O_SYNC)` `0x109001` (both `O_SYNC` and its `O_DSYNC` half),
      `open(O_RDONLY|O_ASYNC)` `0xa000`, `open(O_RDWR|O_DIRECT)` `0xc002`,
      `open(O_RDONLY|O_NOATIME)` `0x48000`;
    * **what the open spends is not reported**: `open(O_WRONLY|O_CREAT|O_TRUNC)` answers `0x8001`
      and `open(O_WRONLY|O_CLOEXEC)` `0x8001` - the creation flags are answered by the open itself,
      and `O_CLOEXEC` is a *descriptor* flag (`F_GETFD`'s word);
    * **the two halves are separate**, which is the whole reason `F_SETFL` takes a mask: on one
      `O_WRONLY` description, `F_SETFL(0)` leaves `0x8001` (mode kept, status cleared),
      `F_SETFL(O_NONBLOCK)` gives `0x8801`, and `F_SETFL(0xFFFFFFFF)` gives `0x4cc01` - Linux's own
      `SETFL_MASK` is `O_APPEND|O_ASYNC|O_DIRECT|O_NOATIME|O_NONBLOCK`, so those four survive and
      the creation bits in the same word store nothing (`F_SETFL(O_CREAT|O_EXCL|O_NOCTTY|O_TRUNC)`
      answers `0x8001`);
    * **the doors that mint their own direction already agree**: `pipe()`'s ends `0` and `1`,
      `pipe2(O_NONBLOCK)`'s `0x800` and `0x801`, a socket/socketpair/`eventfd`/`timerfd`/
      `signalfd`/`epoll` `2`, `inotify_init1` `0`, and `memfd_create` `0x8002` - the last one only
      because that descriptor's own `O_LARGEFILE` is in the answer, so this kernel's `2` there is
      the same `O_LARGEFILE` gap and not a second finding.

    **The fix is one field and one call.** `File::born` keeps the word the open was asked for,
    minus `SPENT_AT_OPEN` - the access mode, the four creation flags and `O_CLOEXEC` -
    `File::open_flags` composes `access | status | born`, and the one path-`open` records it beside
    `set_status`. `F_SETFL` still reaches `status` alone, which is Linux's mask making the split
    rather than a rule about this kernel. Nothing else moved: the doors that mint a description
    without a flag word leave `born` at zero, which is what Linux reports for all of them.

    Pin: `t-fcntl`'s new **nine claims** (31 in the file, was 22) - `O_NOFOLLOW` reported, a status
    write beside it keeping both, the status clear leaving it standing, `O_DIRECTORY`,
    `O_SYNC|O_DSYNC`, `O_NOATIME`, `O_ASYNC`, and the two controls that say the word is not
    everything (`O_CLOEXEC` and `O_TRUNC` are not reported) - beside the host unit test
    `the_reported_word_holds_what_the_open_asked_for`, which walks the split with no machine under
    it. Every claim is Linux's answer, so `MPOS_ON_HOST=1` is the measurement: **31 of 31 on this
    host first**, and the sweep is the pin. The four flags whose *numbers* differ between the two
    machines are taken from `libc` in the program rather than written down, which is what lets one
    file pin both.

    Gate: x86_64 `pass=183 fail=0 (183 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (183
    staged, 11 left out by `tests.toml`); the second machine's arm is `pass=182 fail=0`,
    `mpos-kunit 32 passed, 0 failed` (182 staged, 12 left out), and the program's 31 `ok:` lines are
    byte-identical across the two machines (diffed, carriage returns aside).

    The counterfactual is sweep-only, and that is the shape of the change rather than a shortcut:
    the field, the composition and the unit test are the *mechanism*, and the mechanism is one line
    at the call site. With only that line put back (`file.set_born(flags as u32)` commented out; the
    field, the unit test and all 31 claims kept) stage 1 is untouched - the unit test calls the
    setter itself - and the sweep is `pass=182 fail=1 (183 built)` with `FAIL t-fcntl` and **seven**
    red claims by name: the five reported flags (`O_NOFOLLOW`, `O_DIRECTORY`, `O_SYNC`+`O_DSYNC`,
    `O_NOATIME`, `O_ASYNC`) and the two that only pass because one of them survives a status write
    (`and a status write beside it keeps both`, `... while clearing the status half leaves the
    open's flag standing`). The two controls - `O_CLOEXEC` and `O_TRUNC` not reported - pass on both
    kernels, which is what keeps the reds on the recording rather than on the doors.

## item 50 — `__WCLONE` narrows a wait to the kind of child `clone`'s low byte named

    §5.2's entry, and the last of the three Linux-only wait bits to be read rather than waved
    through. `clone`'s low byte is not a flag: it is the signal a child's death should report,
    and Linux reads that byte at the other end to split children into two kinds — born to report
    `SIGCHLD` ("non-clone") and born to report nothing or something else ("clone") — with
    `wait(2)`'s option word choosing which kind the caller may be answered about (`__WCLONE` the
    clone kind alone, `__WALL` both, neither the non-clone kind alone). This kernel accepted the
    bit and reported every child, so both doors disagreed with Linux about which children the
    caller has: the pid was a real child's, and Linux says `ECHILD`. The prerequisite the entry
    named — "the clone exit signal would have to survive on the task" — was the whole of the work.

    **Measured whole on a current host first** (`.workbuddy/probe_wclone.c` and
    `probe_wclone_block.c`, Linux 7.2.9, raw `clone` so the byte is chosen rather than inherited
    from a libc's `fork`):

    * **A — a `SIGCHLD` child is invisible to `__WCLONE`**: `waitid(P_ALL, 0, WEXITED|__WCLONE)`
      is `ECHILD`, and the same call with `__WALL` reports it (`si_code` 2, `si_pid` the child).
      The old `probe_waitid.c` measurement, confirmed on a second kernel;
    * **B — a `clone(0)` child is invisible to everything but `__WCLONE`**: `wait4(-1, WNOHANG)`
      `ECHILD`, `wait4(clone_pid, WNOHANG)` `ECHILD`, `waitid(P_PID, ..., WEXITED)` `ECHILD`, and
      `wait4(-1, __WCLONE)` collects it with its status word. `ECHILD`, not `WNOHANG`'s 0 — a
      child the call cannot see is not a child it is waiting for;
    * **C — a non-`SIGCHLD` signal makes a clone child too**: `clone(SIGTERM)` is `ECHILD` to a
      plain wait and collected by `__WCLONE`; and the same byte is *delivered* — `sigtimedwait`
      reads a `SIGTERM` with `si_code` 2 and the child's pid (the half below that is still named);
    * **D — the mixed set**: with a forked child and a live clone child, `wait4(-1, 0)` returns the
      *forked* one, and the next `WNOHANG` is `ECHILD` while the clone child is still alive — and
      after it dies too. `__WCLONE` then collects exactly it. This is the shape that tells a
      filter from a widening: a kernel that merely *preferred* `SIGCHLD` children would pass every
      single-child claim above and fail this one;
    * **E — two clone children, then nothing**: `__WCLONE` reaps both in turn, and the third call
      is `ECHILD`;
    * **F — `__WALL` wins when both bits are set**: a `SIGCHLD` child is reported by
      `__WALL|__WCLONE`, which `__WCLONE` alone refuses;
    * **G — the blocking shapes**: with only a live clone child, `wait4(-1, 0)` and
      `waitid(P_ALL, WEXITED)` answer `ECHILD` at once, and `__WALL` collects it once it exits.
      Without this half, a kernel that filtered only the *answers* would park a caller for ever.

    **The fix is a field, a type, and one eligibility rule.** `Task::exit_signal` keeps the byte
    `validate_clone_flags` had been splitting off and dropping; `Retired::exit_signal` carries it
    past the task's death, for the reason `pgid` is carried — a wildcard wait reads the record,
    and the task is gone. `sched::ChildKind` (`NonClone`/`Clone`/`Either`) is the option word's
    second dimension, and it rides in `trace::waitable` — the one rule every scan already asks —
    so the seven places that look for "my children" (`scan_child_event`, the live and retired
    halves of `has_matching_children`, `has_children`, `child_alive_for`, `unreported_stop`, and
    the by-id `child_event_in`) cannot disagree. A task some tracer holds is exempt: Linux draws
    the `__WCLONE` line for children, not for tracees, and `PTRACE_ATTACH` can be aimed at a task
    this caller never cloned.

    `fork` and `vfork` pass `SIGCHLD` explicitly — they *are* `clone(SIGCHLD)` — so every child
    this system already had is a non-clone child and every existing `wait` answers as before; a
    bare `CloneRequest` keeps 0, which is what `clone(0)` asked for. A `CLONE_THREAD` child is
    given `SIGCHLD` whatever its word said: Linux forces a thread's exit signal away (it never
    notifies), and this kernel's threads are not their creator's children either, which is the
    line `spawn_thread` already draws.

    Both doors decode the word through one function (`wait_child_kind`), because the bits mean the
    same thing in `wait4`'s and `waitid`'s words and two decodings could drift. `wait4`'s *named*
    form asks the kind before it looks at anything, because a record is read without a scan and
    would otherwise be handed to a caller Linux says has no such child.

    Pin: `t-wait4` — **11 new claims** (51 in the file) — and `t-waitid` — **5** (61 in the file):
    the forks, the two `ECHILD` shapes, the `__WCLONE` collection with its own exit code, the mixed
    set's four answers in order, and the blocking claim guarded by a `SIGALRM` that turns a park
    into a readable `EINTR` rather than a sweep that stops dead. Both binaries were run on a
    current host first (`MPOS_ON_HOST=1`, 51/51 and 61/61 green), so every claim is Linux's own
    answer and not this kernel's shape.

    **And the pin caught a bug beside the item, in the same word.** Both doors read `options` as the
    whole register. Linux declares it an `int` (`SYSCALL_DEFINE4(wait4, pid_t, upid, int __user *,
    stat_addr, int, options, ...)`), and that is not a formality here: `__WCLONE` is bit 31, so a C
    caller passes a *negative* `int`, the compiler sign-extends it into the 64-bit register, and a
    door that reads all 64 bits sees bits Linux never defined - so
    `waitid(P_ALL, 0, WEXITED|__WCLONE)` from a program was `EINVAL`, while the same call through a
    32-bit-clean word was `ECHILD`. Both doors narrow now (`as u32 as usize` for the option words,
    `as i32 as isize` for the `int`-shaped ids), and the test helper says why its own `options`
    argument is an `i32`: the two `__WCLONE` claims are the ones that failed first, and they are what
    keeps a door from reading a register where Linux passes an `int`.

    Gate: x86_64 `pass=183 fail=0 (183 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (183
    staged, 11 left out by `tests.toml`); the second machine's arm is `pass=182 fail=0`, `mpos-kunit
    32 passed, 0 failed` (182 staged, 12 left out), and both programs' `ok:` lines are byte-identical
    across the two machines (diffed, carriage returns aside).

    Counterfactual, and it is sweep-only because the rule is one predicate: with `ChildKind::sees`
    made to answer `true` - the old kernel, which stores the byte and never asks it, everything else
    kept - stage 1 is untouched (no host unit test asks the rule directly, which is why the rule is
    pinned where a caller can see it) and the sweep is `pass=181 fail=2 (183 built)` with `FAIL
    t-wait4` and `FAIL t-waitid`, **nine** red claims by name: `WNOHANG finds no child to report: a
    clone child is not one a plain wait sees`, `__WCLONE collects it, with its own exit code`, `and
    then answers ECHILD with the clone child still alive`, `which stays invisible after it has died
    too`, `a blocking wait4 with only a clone child answers ECHILD instead of parking`,
    `waitid(P_ALL, WEXITED|__WCLONE) is ECHILD for a SIGCHLD child`, `while __WALL reports it, with
    its code`, `waitid(P_ALL, WEXITED|WNOHANG) is ECHILD for a clone child` and `__WCLONE reports
    it, with its own code`. The blocking claim goes red as `EINTR` rather than as a hang, which is
    what the alarm is there for; and three claims *pass* on the old kernel for a reason worth
    naming - `the default wait returns the forked child, not the clone one` (only the forked child
    had exited yet), and the two that follow a claim which collected the fixture early (a cascade,
    not agreement).

## item 51 — a wait's two writes sit where Linux's sit: the wait, the rusage, then the record

    §5.2's entry, and the two corners item 34 measured and left unbuilt. Item 34 gave both wait
    doors the caller's `struct rusage`; what it left was *where* the writes sit — relative to the
    wait and to each other — and that is visible only to a caller that hands over a pointer the
    kernel cannot write to. Linux's `do_waitid` collects (or refuses) inside `kernel_waitid`, copies
    the `struct rusage` when — and only when — it has an event to report, and writes the six-word
    `siginfo_t` **last**, on every path, with that copy's own `EFAULT` outranking whatever the wait
    said. This door cleared the whole record up front, which produced exactly the two divergences
    item 34 named: a bad `rusage` left *zeros* where Linux leaves the caller's own bytes, and a bad
    record was `EFAULT` *before* the wait, so the child Linux collects was still here.

    **Measured whole on a current host first** (`.workbuddy/probe_waitid_record.c`, Linux 7.2.9, raw
    `SYS_wait4`/`SYS_waitid`, every scenario in its own forked child, so no reap leaks into the
    next):

    * **P — a bad `rusage` on a ready death**: `EFAULT`, the record **untouched** (all `0xAA`), and
      the child **collected** (the next wait is `ECHILD`);
    * **Q — a bad record with a good `rusage`**: `EFAULT`, the rusage **filled**, the child
      **collected** — the other half of the same order;
    * **R — both bad**: `EFAULT` (the rusage's, being first), child collected;
    * **S — the `EINVAL` for a word that asks for no event class**: the record's **six words
      zeroed**, while `__pad0` at 12 and everything from 28 on stay the caller's `0xAA`. This is
      what a 128-byte clear gets wrong, and what a caller can *see*;
    * **T, U, AD — the same refusal with an unwritable record**: **`EFAULT`, not `EINVAL`** — for the
      no-class word, for an unknown bit, and for an idtype this kernel does not serve;
    * **V — `ECHILD` with an unwritable record**: **`EFAULT`** as well; **AE** — `ECHILD` with a
      writable seeded record: `ECHILD`, six zeros written;
    * **W — a null record**: accepted, `0`, and the child collected — the caller that wants only the
      reap;
    * **AC, AF — `WNOHANG` with nothing to report**: the zero, with the rusage untouched (AC) but
      the record still written, so a bad one is `EFAULT` (AF). "Nothing yet" is not "nothing to
      write";
    * **AB — `EINVAL` with a bad rusage and a good record**: `EINVAL`, so the rusage is copied only
      when an event was reported;
    * **X — a record whose tail is unmapped** (64 bytes in the page, 64 past it): **`0`**, six words
      written, the bytes from 28 to 64 untouched. A whole-record range check answers `EFAULT`;
    * **AG — a record whose third word straddles the boundary**: `EFAULT` with `si_signo` 17 and
      `si_errno` 0 **already written** and `si_code` untouched. The check is per word;
    * **AH — a `rusage` straddling the boundary**: `EFAULT` with its mapped **head written** —
      Linux's copy is a range copy (`copy_to_user`), not a check-then-write;
    * **Z, AA — the `wait4` controls**: a bad `*status` is `EFAULT` with the rusage untouched and
      the child collected, and a bad rusage is `EFAULT` with `*status` already written. That door
      was already in this order, and the pair is what keeps the reds on the door that moved.

    **The fix is a split and three writers.** `waitid_collect` does the word, the selector and the
    collection and returns a `WaitIdAnswer` — an errno, plus the event when there is one — touching
    no caller pointer; `handle_waitid` runs the two writers afterwards in Linux's order and lets the
    record's `EFAULT` win over the answer, which is why `EINVAL` and `ECHILD` become `EFAULT` for a
    record that cannot be written. `abi::write_waitid_info` and `write_waitid_empty` write the same
    six words through one `put_siginfo_word`, which checks that word's own four bytes — that is what
    makes X and AG come out right, and what a 128-byte `buffer_ok` cannot express — and touch
    nothing else in the record. `write_rusage` builds its 144 bytes and hands them to
    `sched::write_user`, the page-by-page copy that *is* Linux's `copy_to_user`; the old
    range-check-then-write refused a straddling rusage whole, which was the same mistake one door
    over.

    Pins: `t-waitid` — **34 new claims** (102 in the file), every reachable one green on a current
    host first (`MPOS_ON_HOST=1`, 95 of them; the seven it cannot make move a child's ids, which
    needs a privilege, and none of the new ones does). Host unit tests: one per writer shape — the
    six words, the untouched `__pad0` and tail, and the no-event zeros.

    Gate: x86_64 `pass=183 fail=0 (183 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33; aarch64
    `pass=182 fail=0 (182 built)`, kunit 32/0; both machines' `ok:` lines byte-identical for the
    file (diffed, carriage returns aside).

    Counterfactual, and it is sweep-only for the same reason item 50's was: with **both** writers put
    back — the 128-byte clear and the whole-record `buffer_ok` in `handle_waitid`, and
    `write_rusage`'s range-check-then-write — stage 1 is untouched (`565 passed; 0 failed`: the unit
    tests pin the *writers'* shape, and that is not what moved), while the sweep is `pass=182 fail=1
    (183 built)` with `FAIL t-waitid` and **eleven** red claims by name: `and leaves the record as
    the caller wrote it, because the record is written last`, `with the rusage already filled,
    because it is written first`, `and the child collected before either refusal`, `while the pad at
    12 and the tail from 28 stay the caller's`, `a null record is accepted and answers 0`, `and it
    collected the child`, `a record whose tail is unmapped is still written, because every word it
    writes landed`, `with the child's own pid in it`, `with the words before it already written`,
    `with its mapped head already written, because the copy is a range`, and `and the child collected
    before that refusal too` — the last a cascade of the null-record refusal rather than a second
    corner. Worth naming what *stays green* there: every precedence claim (`an unwritable record
    outranks the refusal`, `and outranks ECHILD...`, and the `EINVAL`/`ECHILD` pairs) passes on the
    old kernel too, because refusing *before* the wait still answers `EFAULT` — only "what happened
    to the child" tells the two shapes apart, which is why those claims are the pin.

## item 52 — `fallocate` is a write: it costs the set-id bits and stamps the clocks, on every successful call

    §5.2's entry, and item 41's one named corner. Item 41 gave the write path Linux's
    `file_remove_privs` at four doors — `File::write`, `ftruncate`, `truncate` and an `open`'s
    `O_TRUNC` — and left `fallocate`'s resize named as "reaches `Inode::truncate` without going
    through either truncate door, and was not measured". Measuring it widened the question rather
    than closing it: Linux's `vfs_fallocate` ends **every** successful path in `file_modified`, and
    that is three facts at once — the set-id bits go unless `CAP_FSETID` spares them, `mtime` and
    `ctime` move, `IN_MODIFY` is reported — while the immutable flag refuses the call before
    anything is allocated. This door reached `Inode::truncate` directly and passed none of them,
    and the two paths that *grow nothing* (`KEEP_SIZE`, a range already inside the file) passed
    nothing at all, because they never reached the truncate.

    **Measured whole on a current host first** (`.workbuddy/probe_fallocate_mode.c`, Linux 7.2.9,
    raw `SYS_fallocate`, `mtime` pinned to `1000000000` by `utimensat` before each call so "moved"
    and "kept" are one comparison apart; run on ext4 under `/home` and on tmpfs under `/tmp`, which
    agree on every row):

    * **A — grow, `4755` file, mode 0**: `0`, size 4 → 8192, mode **`0755`**, `mtime` → now;
    * **B — `KEEP_SIZE`, `4755` file**: `0`, size stays **4**, mode **`0755`**, `mtime` → now. The
      length did not move and the bits went anyway;
    * **C, D — a range starting past the end, with and without `KEEP_SIZE`**: both `0755`, `mtime`
      → now, sizes 8192 and 4;
    * **E — a range already inside a 16-byte file, mode 0**: `0`, size stays 16, mode **`0755`**,
      `mtime` → now. Nothing was allocated and the call was still a write;
    * **F — a `2755` file**: stores `0755` — the set-group-id half goes by the same rule;
    * **H — the same grow as root**, which holds `CAP_FSETID`: mode stays **`4755`** and `mtime`
      still moves. Two rules, two questions — the bits are the capability's, the clock is not;
    * **J — a descriptor opened *before* `chattr +i`**: **`EPERM`**, size still 4, `mtime` still
      the pinned `1000000000`. The flag refuses this door, and a refusal is not a write. (G: after
      the flag, `open(O_RDWR)` is itself `EPERM`, which is why the descriptor has to exist first —
      the same shape `t-fsflags` uses);
    * **I, K — `chattr +a`**: served, both for a growing range and for one inside the file. So the
      flag question here is `crate::vfs::may_modify` and **not** `may_alter`, which would have
      refused a file Linux serves. The measurement is the whole reason the narrower predicate is
      the right one.

    Implemented as the flag question where Linux asks it — after the descriptor's write mode
    (`EBADF`) and before the kind — and one helper at **both** success arms:
    `fallocate_modified` runs `crate::vfs::remove_privs`, then `set_times(atime, now)` (the `ctime`
    rides with it, which is `set_times`' own promise), then the `IN_MODIFY` event. That is
    `file_modified`'s order, and putting it on both arms is what makes the `KEEP_SIZE` case a write
    rather than a no-op.

    Pins: **13 claims** — `t-fallocate` 9 (the clock pinned, an allocation that cannot change the
    length, `fstat` after it, the moved `mtime`, the length that did not move, the re-pin, the
    refused `EINVAL`, `fstat` after the refusal, the clock that stayed pinned), `t-caps` 2 (the
    set-id bits with `CAP_FSETID` dropped, beside the whole set's control — `t-caps`' row, because
    the sweep runs as root and the bits only go for a caller that dropped the capability) and
    `t-fsflags` 2 (the `EPERM` on an immutable file, and the size it left alone). Host: all nine
    `t-fallocate` claims green under `MPOS_ON_HOST=1`; the other four are the gated kind (a dropped
    capability, an immutable file), and both files already name that class and stop on an
    unprivileged host.

    Gate: x86_64 `pass=183 fail=0 (183 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33; aarch64 `pass=182 fail=0 (182 built)`, kunit 32/0; both machines' `ok:` lines for the three programs identical claim by claim (diffed, carriage returns aside).

    Counterfactual (both halves put back — no flag question, no `fallocate_modified`):
    stage 1 unchanged (`565 passed; 0 failed` - no unit test asks this question, and the writers are not what moved), and the sweep `pass=180 fail=3 (183 built)` with `FAIL t-caps`, `FAIL t-fallocate` and `FAIL t-fsflags`, **three** red claims by name: `FSETID dropped: an allocation costs the same two bits (got 4755)`, `an allocation moves mtime off the pinned second`, and `fallocate is EPERM (got errno 0, want 1)`. The other two new claims are controls and saying so is the point: `whole set: and so does an allocation` holds on the old kernel because root keeps the bits either way, and `and the file did not grow` holds because the request was `KEEP_SIZE`, so the size never depended on the fix.

    Still named, one shape over: `fallocate`'s **interior** operations (punch, collapse, insert,
    zero-range) are `EOPNOTSUPP` in this kernel, so the bits-and-clocks rule has no other arm to
    reach; if one is ever implemented it inherits this helper rather than the truncate's door. The
    mapping-write half of item 41's tail is untouched and remains Linux's own exception.

## item 53 — `wait4`'s `*status` is one `put_user`, and this door already answered that way

    Item 51's own trailing sentence, measured and closed rather than built. Item 51 put the two
    wait writes in Linux's order — the wait, the record, then the `siginfo_t` — and named one
    corner it had not looked at: `wait4`'s four-byte `*status` was (and is) checked with a
    whole-word `buffer_ok` before a single write, and the question was whether Linux's `put_user`
    there faults **atomically** or copies what it can reach the way `struct rusage` does (item 51's
    scenario AH, where the head of the record is already written when the tail faults).

    **Measured whole on a current host first** (`.workbuddy/probe_wait4_status.c`, Linux 7.2.9,
    fourteen scenarios A–N; every reachable byte seeded `0xAA` before the call and read back with
    `read_unaligned`, so a fixture never depends on the kernel having written anything):

    * **A — the control**: a word wholly inside a writable page answers the pid and `0x700` — the
      write the rest of the section refuses, which a pin made only of refusals would never show;
    * **B, C, D — the word straddling the page end**: two bytes in and two out, one in and three
      out, three in and one out — **all three `EFAULT`, both reachable bytes still `aa`**. Not one
      byte is written, which is `put_user`'s atomicity and not a range copy's;
    * **E — wholly inside `PROT_NONE`**: `EFAULT`;
    * **F — a null pointer**: a *report*, not an error — the child is reaped and the answer is its
      pid, the shape `rustix`'s `waitpid` passes;
    * **G — a refused word with a good `rusage`**: `EFAULT` and the record untouched, so the word
      is written *first*;
    * **H — a good word with a refused `rusage`**: `EFAULT` and the word **written** (`0x700`) —
      the mirror image, already pinned from item 51's side;
    * **I — both refused**: `EFAULT`, nothing written — and the record is never reached, because the
      word is asked first;
    * **J — the `WNOHANG` zero with a refused word**: `0`, not `EFAULT` — the zero is decided
      before the word is reached;
    * **K — `ECHILD` with a refused word**: `ECHILD`, same reason;
    * **L — a refused *stop***: `EFAULT`, and the next `WUNTRACED|WNOHANG` answers `0`. The event
      was taken before the word was written, so the stop is gone — Linux loses it exactly the same
      way, which is why the claim says so instead of calling it a leak;
    * **M — a refused death**: `EFAULT`, and the second `WNOHANG` wait is `ECHILD` — the child was
      collected before the refusal landed;
    * **N — a page the caller made read-only**: `EFAULT`, and the page still holds the seed
      afterwards. The question the check asks is the *write* permission, not the mapping.

    **Nothing in the kernel changed**: this door's whole-word `buffer_ok` followed by one
    `write_unaligned` *is* the atomic shape, and the measurement is what says so rather than the
    reading of the code. What the item buys is the claim — every one of the fourteen shapes is a
    claim in `t-wait4` now (the four a first draft of this entry left to the probe — A, C, D and I,
    the control, the other two splits and the both-refused shape — included), so a later "just copy
    four bytes" cannot land quietly.

    Pins: **33 new claims** in `t-wait4` (84 in the file, was 51), all green on a current host
    first (`MPOS_ON_HOST=1`; none needs a privilege). The two-page fixture is built inside the test
    with `mmap` + `mprotect` rather than read out of a `/proc` the host and the guest spell
    differently, and the pointer is handed to the syscall directly — a `&mut i32` cannot name a word
    that starts two bytes before a page end.

    Gate: x86_64 `pass=183 fail=0 (183 built)`, aarch64 `pass=182 fail=0`, `mpos-kunit 32 passed, 0
    failed` on both machines, exit 0; both machines' `t-wait4` `ok:` lines byte-identical (84 and
    84, `diff` empty), and equal to this host's own run but for the one named divergence line item
    34 recorded about `ru_maxrss` and the fault/switch counters.

    Counterfactual — the word written byte by byte, each byte passing its own `buffer_ok`, which is
    the range-copy shape `struct rusage` has: stage one is untouched (nothing here is host-compiled
    and no unit test asks this question), and the sweep is `pass=182 fail=1 (183 built)` with
    `FAIL t-wait4 exit=1` and **four** claims red by name — "and not one byte of it was written",
    "and that one reachable byte was not written either", "and all three reachable bytes were left
    alone" and "both pointers refused is EFAULT and touches neither". The `EFAULT` claim beside each
    of them stays green, because a range copy that wrote its head still refuses; that asymmetry is
    the item in one sentence: an error code alone cannot tell "refused before writing" from "refused
    after writing what it could reach". Only the bytes can.

## item 54 — `/proc/<pid>/smaps`: Linux's field set, and the three numbers this kernel cannot derive

    §5.2's entry said it in one line — "`maps` is served in Linux's exact format; the two
    siblings are unwritten" — and `smaps` is the one every memory-reporting tool reads:
    `smem`, `ps_mem`, and a compositor asking how much of a client's `wl_shm` buffer is
    resident. What such a reader depends on is two things at once, and the measurement
    below is about both: a record *is* the `maps` line for its mapping, byte for byte,
    followed by Linux's field lines in Linux's order.

    **Measured whole on a current host first** (`.workbuddy/probe_smaps.c` for the shape,
    `probe_smaps_shapes.c` for the accounting, Linux 7.2.11):

    * the field set is twenty-four `label: value` lines in this order — `Size`,
      `KernelPageSize`, `MMUPageSize`, `Rss`, `Pss`, `Pss_Dirty`, `Shared_Clean`,
      `Shared_Dirty`, `Private_Clean`, `Private_Dirty`, `Referenced`, `Anonymous`, `KSM`,
      `LazyFree`, `AnonHugePages`, `ShmemPmdMapped`, `FilePmdMapped`, `Shared_Hugetlb`,
      `Private_Hugetlb`, `Swap`, `SwapPss`, `Locked`, `THPeligible`, `ProtectionKey` —
      and then a `VmFlags` word list. The last three are the exceptions a copy from a
      table gets wrong: `THPeligible` and `ProtectionKey` carry no unit, and every
      `VmFlags` word is followed by a space, so that line ends in one (`VmFlags: rd mr mw
      me \n`);
    * **every value ends in column 24 whatever its width** — `8`, `136` and `1516` all do,
      which is `seq_printf`'s `%8lu` behind a sixteen-column label, and what makes the
      file readable by position as well as by name;
    * **every record's header is one of the lines `maps` prints, byte for byte.** Measured
      over a process whose mappings covered every shape the two probes could make (34 of
      34 records agreed), and the property a reader that maps one file onto the other
      depends on — which is why `maps` and `smaps` here are two renderings of *one*
      gatherer (`procfs::mapping_lines`);
    * `/proc/<pid>/smaps_rollup` is a different field set again — the same lines minus
      `THPeligible`/`ProtectionKey`/`VmFlags`, plus three this file does not have
      (`Pss_Anon`, `Pss_File`, `Pss_Shmem`) under a `[rollup]` header — and
      `/proc/<pid>/numa_maps` is a different format (`<start> default [file=…] [anon=N]
      [dirty=N] [mapped=N] [mapmax=N] [active=N] N0=N kernelpagesize_kB=4`, with spaces in
      a name escaped as `\040`). **Both stay named in §5.2**: they are the same
      accounting question in two other spellings, and neither is what a tool reaches for
      first.

    **What this kernel cannot derive, and the rule it answers with instead.** The probes
    are also what said the accounting numbers are not derivable here, and each has a
    reason in this kernel's model rather than in effort:

    * `Pss` is a page's size divided by how many address spaces map it, and **no mapcount
      exists here** — `Mm.frames` is a flat list of frames with no virtual key and no
      users, `MappedObject::same_as` compares identity rather than counting, and `fork`
      copies the VMA table without counting anything. Measured on the host: one page two
      address spaces share reads `Rss: 4 kB` with `Pss: 2 kB`, and a private read of the
      same page reads `Pss: 4 kB`;
    * `Shared_*`/`Private_Clean`/`Private_Dirty` need a dirty bit **and a page cache to be
      clean against**, and this kernel has neither: nothing records whether a page has
      been written (a user write goes straight to the frame), and a file is read *in*
      rather than paged, so there is no clean page to compare against. The host's own
      four buckets are its page cache's and its COW states': a written anonymous page
      reads `Private_Dirty` + `Anonymous`, a shared `memfd` page reads `Private_Dirty`
      with `Anonymous: 0`, a private read of a tmpfs file reads `Private_Dirty` with
      `Anonymous: 0`, and a page a `fork` shares reads `Shared_Dirty` with `Anonymous: 4`
      and a halved `Pss` — four shapes, and no rule that produces them from what is here;
    * `Referenced` is the hardware accessed bit, which nothing here reads or clears;
    * `Locked` is `mlock`'s bookkeeping, and `mlock` here is a no-op that takes no lock
      (its own entry in §5.1 says so), so zero is the *truth* rather than a placeholder.

    So the file answers Linux's lines with the one rule this kernel's model can state,
    and the rest of the fields are zero because the feature behind them does not exist:

    **every resident page is this mapping's own, and it is dirty.**

    That makes `Pss` and `Pss_Dirty` equal to `Rss`, `Private_Dirty` equal to `Rss` and
    `Shared_*`/`Private_Clean` zero; `Referenced` equal to `Rss`, because a page becomes
    resident here only by being faulted in, so every resident page has been accessed;
    `Anonymous` equal to `Rss` for a mapping with no object behind it and zero for one
    whose pages an object owns (`memfd`, dma-buf, the framebuffer, a DRM dumb buffer) —
    which is Linux's *own* answer for the two shared shapes measured, and the reason the
    flag comes from the mapping's kind rather than from a count; and `Locked`, `Swap`,
    `SwapPss`, `KSM`, `LazyFree`, `AnonHugePages`, `ShmemPmdMapped`, `FilePmdMapped`,
    `Shared_Hugetlb`, `Private_Hugetlb`, `THPeligible` and `ProtectionKey` zero because
    there is no lock, no swap, no KSM, no huge page, no pmd-mapped page, no pkey and no
    transparent-hugepage eligibility to report.

    `VmFlags` is the same grant `/proc/<pid>/maps`' permission column is (item 54's
    neighbour, `docs/notes/` §15.72): `rd`/`wr`/`ex` from the three permission bits,
    `sh`/`ms` from the sharing bit, and `mr`/`mw`/`me` unconditionally — Linux's
    `VM_MAY*`, which `mprotect` here grants without asking a may-question, so a
    `PROT_NONE` guard page prints `mr mw me` exactly as Linux's does. Two words Linux
    prints are deliberately **absent**: `ac` (overcommit accounting) and `sd`
    (soft-dirty), because printing either would claim a state nothing here keeps.

    **Three fixture lessons, because the gate charged for each.** Adjacent mappings with
    equal flags are **one VMA** — the first draft's two anonymous fixtures came back from
    the host as a single five-page record, so every fixture is now bounded by a
    `PROT_NONE` page on both sides and a record is found by *containing* an address rather
    than by starting at one. **Reading a file can move the heap**: `[heap]`'s own range
    changed between two reads of the two files, because the buffer the reader allocates is
    itself a mapping — so the header comparison is made over the records two `smaps` reads
    agree on, and the buffers are sized before the reads. And **a claim keyed on numbers a
    fixture happens to have is a claim about the whole address space**: the child's
    mapping was first found by "a record of 12 kB with 8 kB resident", which the child's
    own heap can satisfy by accident — it passed on the first machine and failed on the
    second. The child now reports the address it got down a pipe, and the parent asks
    about *that* mapping.

    Pins: `t-smaps`, **34 claims**, all green on a current host first (`MPOS_ON_HOST=1`,
    where the `ok:` lines come out byte-identical to the sweep's: the one divergence a
    host run has is the two extra `VmFlags` words, and the claim filters exactly those and
    prints the host's own list beside this kernel's when anything else differs). Four host unit tests hold the
    renderer: the whole record byte for byte against the host's own lines
    (`a_smaps_record_has_linuxs_lines_in_linuxs_order`), a wider value in the same column
    (`a_wider_value_keeps_the_column_linux_puts_it_in`), nothing resident being every
    number zero (`a_record_with_nothing_resident_is_all_zeroes`), and the flag words per
    shape (`the_flags_are_the_words_this_kernel_can_stand_behind`).

    Gate: x86_64 `all 184 user tests passed` (`pass=184 fail=0 (184 built)`), aarch64
    `pass=183 fail=0`, `mpos-kunit 32 passed, 0 failed` on both machines; both machines'
    `t-smaps` `ok:` lines byte-identical (34 and 34, `diff` empty).

    Counterfactual: `sched::resident_pages` answering the whole range as resident — the
    shape that would make the file a statement about the *description* rather than about
    the pages — is `pass=182 fail=2 (184 built)`: `FAIL t-smaps exit=1` with **nine**
    claims red by name, every one of them `Rss`'s or derived from it — `Rss counts the
    pages that faulted in`, `Pss is Rss`, `Pss_Dirty follows Pss`, `the four
    shared/private buckets`, `Referenced is the resident set`, `Anonymous is the resident
    set`, `an untouched mapping is Size without Rss`, `a guard page is mapped without a
    resident page`, and the child's own mapping — while the four renderer unit tests stay
    green, because they pin the *lines* and this moves the *number*. (The run's second red
    was `t-aslr`'s `the stack does not start at the same page twice`, which has nothing to
    do with the file: it passed on the immediate re-run, the randomization flake kind.)

    Owner: `src/vfs/procfs.rs` (`SmapsLine`, `format_smaps`, `mapping_lines`, the node),
    `src/sched/runtime/child_mm.rs` (`resident_pages`, the page-table walk a named pid's
    `Rss` needs), and the module's `sched` re-export. What stayed named here —
    `numa_maps` and `smaps_rollup`, and the three `Pss_*` lines the rollup has and
    `smaps` does not — is **served by item 62** (2026-10-19), over the same records
    this item's `mapping_lines` gathers.

## item 55 — a `poll` with no descriptors is a *wait*, and `pause()` is that call on arm64

    A zero-`nfds` `poll`/`ppoll` answered `0` at once, where Linux waits out the timeout
    and waits for ever when there is none. Measured whole on a current host first
    (`.workbuddy/probe_poll0.c`, Linux 7.2.11): `poll(NULL, 0, 100)` answers `0` after
    **100.1 ms**, `poll(NULL, 0, 0)` and `ppoll(NULL, 0, {0,0})` answer at once, and
    `ppoll(NULL, 0, NULL)` answers only when a signal arrives — that probe's `SIGALRM` at
    1000 ms, with the call answering `-1` and the errno of an interrupted wait.

    What makes it more than a corner: **`pause()` *is* that call on arm64.** Linux has no
    `pause` syscall number there, so musl builds it as `ppoll(NULL, 0, NULL, NULL, 0)` —
    measured by disassembling the toolchain's own archive (`llvm-objdump -d pause.lo` out
    of `aarch64-unknown-linux-musl`'s `libc.a`: syscall 73, with every argument register
    zero) — so a program that idles in `pause()` returned immediately instead of parking.
    It was found the hard way: `/proc/<pid>/smaps`' pin parks a child, the child exited
    with status 0 on the second machine where the same program parked on the first, and
    every `Rss` in the child's file read `0` because its frames had gone with it
    (`docs/uapi/` §5.1 item 54, `docs/notes/` §15.73).

    The fix is at the one place both calls are made of (`poll_wait`): the array is
    validated only when there is one — an `nfds` of zero *is* the null `fds` that spelling
    uses, and a zero-length range needs no page — and the wait then runs with an empty
    watch list, which the loop already handles: it parks to the deadline or, with none,
    until a signal ends it. Nothing else moved: a wait with descriptors takes the same
    path it did.

    Pin: `t-poll`'s **four new claims** (24 in the file) — the timeout is waited out
    (`≥ 90 ms` of a 100 ms wait), a zero timeout answers at once, and a forked child is
    still sitting in the no-timeout wait 200 ms later. All four green on a current host
    first, where Linux answers exactly that.

    Gate: x86_64 `pass=184 fail=0 (184 built)`, aarch64 `pass=183 fail=0`, `mpos-kunit 32
    passed, 0 failed` on both machines; both machines' `t-poll` `ok:` lines byte-identical
    (24 and 24, `diff` empty).

    Counterfactual: the early `return 0` put back for a zero `nfds` (the shape that was
    here) is `pass=183 fail=1 (184 built)` with `FAIL t-poll exit=1` and **two** claims red
    by name — `a poll with no descriptors waits out its timeout` and `and the no-timeout
    wait does not return on its own`. The zero-timeout control stays green either way: a
    *zero* timeout is answered at once by both shapes, and that is exactly what makes it a
    control rather than a claim. `t-smaps` stays green there too, because its child parks
    in a sleep loop and does not depend on this call - the dependency would have hidden
    which landing the failure belonged to.

## item 56 — a terminal's foreground group is the *session's*, and a reader is not its owner

    The divergence `docs/uapi/` §5.2 carried: Linux keeps a terminal's foreground
    group strictly as session state, while this kernel answered with the reader -
    `Tty::read` recorded the reading task's group as the terminal's foreground
    ("the same answer for a program that never asks"), so a program that read a
    tty it never claimed *became* its owner: the typed `SIGINT`/`SIGQUIT`/
    `SIGTSTP` and the resize `SIGWINCH` reached it, and `tcgetpgrp` named it. And
    the pair of doors refused nothing: a pair no session held answered `0` and
    accepted any group from anyone.

    Measured whole on a current host first (`.workbuddy/probe_tty_fg.c` and
    `probe_tty_fg2.c`, Linux 7.2.11, every scenario in a session of its own, the
    fork's own `setsid` isolated so one row cannot leak into the next):

    * **a pair nobody holds has no foreground group**: `TIOCGPGRP` on the slave is
      `ENOTTY`, `TIOCSPGRP` is `ENOTTY` on either end, and `TIOCSWINSZ` on the
      master succeeds with nobody to tell. The *master's* `TIOCGPGRP` answers `0`
      instead - Linux asks the controlling-terminal question of the slave end only
      (`tty == real_tty && current->signal->tty != real_tty`), because a supervisor
      holding nothing but the master has no other way to learn whether it may do
      job control at all;
    * **being a session leader is not holding it**: the same refusals after
      `setsid` until `TIOCSCTTY` lands, and then the group is the leader's own - on
      both ends, since a pair is one terminal;
    * **the refusals are ordered and distinguishable**, from the owning session:
      `EFAULT` for a bad pointer (asked *after* the session question, so an
      unowned terminal answers `ENOTTY` to a null pointer too), `EINVAL` for a
      negative number, `ESRCH` for one that names no process (zero included),
      `EPERM` for a group in another session - and the group it would not take is
      exactly where it was;
    * **reading is not claiming**: a process in *another* session reads the slave
      and the answer does not move. This is the row the old kernel failed, and the
      fixture puts the reader there for a second reason - a reader in a *background
      group of the terminal's own session* is stopped by `SIGTTIN` on Linux (the
      probe's first draft did exactly that and hung, `--- stopped by SIGTTIN ---`),
      which is a signal this kernel does not raise;
    * **the move that is allowed** is a group of the same session, and the terminal
      answers it afterwards.

    Implementation: the read-side claim is *deleted* (`Tty::read` no longer touches
    `foreground`, and its comment says why the session is the only writer); the
    `0x540F`/`0x5410` arm asks the two questions Linux asks - "is this the caller's
    controlling terminal" (`controlling_tty_of_current` against the pair's *slave*
    device id, which is the name `TIOCSCTTY` recorded) and "is the named group in
    the terminal's session" (`sched::session_of` beside `session_of_tty`). The
    master end skips the first question for `TIOCGPGRP` only. Error order is the
    measurement's, and `EINVAL` became `ESRCH` for a number that names nothing.

    Two shapes stay *named* rather than built, and both are measured: a background
    group that tries to change the terminal is `ENOTTY` when its group is orphaned
    (with `SIGTTOU` caught) and served when `SIGTTOU` is ignored - this kernel has
    no terminal-generated `SIGTTOU` to send, so it serves the call; and the
    background *read* above is `SIGTTIN`'s. `TIOCNOTTY` is measured (Linux hangs the
    session up with `SIGHUP`) and is not served here. The two scheduler hand-backs
    stay as well (`load.rs`'s fork hook, `child_exit.rs`'s return to the parent's
    group): they only fire when a group is *already* the foreground, so they move it
    within the session rather than minting one, which is what this item removed.

    Pin: `t-pty`'s two new sections, **17 claims** (54 in the file, was 37) - four on
    a pair nobody holds, ten inside the getty child (the claimed group, the four
    ordered refusals, that they moved nothing, and the one allowed move), and three
    on the outsider reader. All 49 printed claims green on a current host first.

    Gate: x86_64 `pass=184 fail=0 (184 built)`, `mpos-kunit 32 passed, 0 failed`,
    exit 0; aarch64 `pass=183 fail=0 (183 built)`, kunit 32/0. Both machines' `t-pty`
    `ok:` lines identical claim by claim (49 and 49, `diff` clean after the carriage
    returns).

    Counterfactual (the read-side claim and the old arm put back, the test kept):
    `pass=183 fail=1 (184 built)` with `FAIL t-pty` and **nine** claims red by name -
    `a pair nobody holds has no foreground group to report`, `and its master answers
    zero rather than refusing` (the parent's own earlier reads had made it the
    owner, so the master named its group), `handing a group to a pair nobody holds
    is ENOTTY`, `and the refused hand left no group behind`, the two `ESRCH` rows
    (the old arm said `EINVAL`), `a group in another session is EPERM`, `the four
    refusals moved nothing` (one of them *succeeded* and moved it to the parent's
    group), and `and reading it does not make the reader the foreground group`.

    One trap worth carrying: the record each child reports down a pipe was read with
    a bare `read`, and the child's exit delivers a signal to the reading process - so
    a parked read could come back interrupted, which the fixture read as fifteen
    zeroes. It looked like a kernel difference (green under the new kernel, red under
    the old one) and was the loop; `t-pty` now has one `read_record` that retries
    `EINTR`, and the counterfactual's red list is the one above rather than eleven.

## item 57 — giving the terminal back is the session's act, and only the session's

    `TIOCNOTTY` is the residual item 56 named and did not serve. Linux's rule is
    two facts about *who* asks, and both were measured on a current host before
    anything was written (`.workbuddy/probe_tty_notty.c`, Linux 7.2.11, six
    scenarios, each forked into a session of its own):

    * **the leader gives up the session's terminal.** Its call is `0`, and the
      terminal's **foreground group** hears a `SIGCONT` and a `SIGHUP` - in that
      order, measured twice, the second time with the group already in front so
      that no `TIOCSPGRP` could have sent the `SIGCONT`. A leader that is *not*
      in the foreground group itself hears nothing, which is what makes the
      recipient a claim about the group rather than about the caller. Afterwards
      the terminal has no foreground (`tcgetpgrp` is `ENOTTY`), `/dev/tty` is
      `ENXIO` for every task that still named it, a second call is `ENOTTY`, and
      the pair is **not** hung up: a `read` answers `EAGAIN` rather than `EIO`,
      and a `write` still lands;
    * **anybody else clears only its own record.** A member of the session that
      holds the same terminal gets `0` too, and after it its own `/dev/tty` is
      `ENXIO` while the leader's still opens and the terminal still answers
      `TIOCGPGRP` with the leader's group - the session kept the line;
    * **a descriptor that is not the held terminal is `ENOTTY`**: the same caller
      asking through a pair's *master* (a different terminal from the one
      `TIOCSCTTY` bound, even though both ends are one pair), a process with no
      controlling terminal, and a session leader that never claimed one. The
      master's refusal leaves the claim whole.

    Implementation: the `0x5422` arm of `src/syscall/linux/fs/ioctl.rs` asks
    Linux's two questions - is this descriptor the terminal the caller holds (the
    slave end, not a supervisor's master) and is the caller its session's leader
    - and `sched::release_controlling_tty(dev, session_wide)` does the writing:
    the leader's call clears every task still naming the device, everybody
    else's clears only the caller's. `sched::current_is_session_leader` is the
    question, and `signal::SIGHUP` (1) joins the vocabulary with the default
    action table's plain terminate behind it.

    Pin: `t-pty`'s fourth section, **18 claims** (the file's `r.check` count 54 ->
    72, its printed rows 49 -> 67) - three on the master's refusal and the claim
    it left, seven on the leader's call (its return, both sides' signals, the
    group it emptied, `/dev/tty`, the second call, and the write that says the
    pair is not hung up), four on the member's, two on the callers who hold
    nothing, and the move that put a job in front. All 67 printed claims green on
    a current host first.

    Gate: x86_64 `pass=184 fail=0 (184 built)`, `mpos-kunit 32 passed, 0 failed`,
    exit 0; aarch64 `pass=183 fail=0 (183 built)`, kunit 32/0, exit 0; both
    machines' `t-pty` `ok:` lines identical (67 and 67, `diff` clean after the
    carriage returns).

    Counterfactual (the arm returning `ENOTTY` and nothing else, the test kept):
    `pass=183 fail=1 (184 built)` with `FAIL t-pty` and **six** claims red by
    name - `a session leader's TIOCNOTTY is served`, `the foreground group hears
    the hangup and a SIGCONT with it`, `the terminal has no foreground group to
    report afterwards`, `/dev/tty no longer opens for the caller`, `and its
    TIOCNOTTY is served`, `the member's own /dev/tty stops opening`. The two
    refusal rows stay green, which is what they are for: an arm that does not
    exist refuses everything, and those two are the shapes it is *supposed* to
    refuse.

## item 58 — a socket's buffer size is Linux's number, and a datagram queue is the size it names

    `SO_SNDBUF`/`SO_RCVBUF` were the pair the ROADMAP called record-only, and both halves
    of that were wrong: the figure stored was the caller's (not Linux's), and the figure
    governed nothing. Measured whole on a current host first (`.workbuddy/probe_sockbuf.c`,
    Linux 7.2.11, `wmem_max`/`rmem_max` 4194304 and their defaults 212992):

    * `setsockopt` stores **twice** the ask, and `getsockopt` reports it: 2304 -> 4608,
      10000 -> 20000, 100000 -> 200000;
    * each option has its own floor, applied to the doubled figure: `SO_SNDBUF` 4608 (100,
      1 and 0 all read 4608) and `SO_RCVBUF` 2304 (100 reads 2304, while 2048 reads 4096 -
      the doubling again above half the floor);
    * the ceiling is the sysctl maximum applied to the *ask*, so ten million reads back
      8388608 (twice four mebibytes);
    * a **negative** ask is not an error: Linux reads the `int` as unsigned, the maximum
      caps it, and `setsockopt(-1)` answers 0 with 8388608 behind it;
    * the number governs the queue. With the dead-neighbour fill (`docs/notes/` §10.4's
      shape), 1400-byte datagrams stop after 2 at the floor, 4 at 9216, 9 at 20000 and 57 at
      131072 - and a 5600-byte datagram is accepted into a socket reading 4608, because the
      accounting compares what is *queued* before the new datagram is charged.

    Implementation: `net::sock_buffer_size` is the rule (cap the ask at
    `SOCK_MEM_MAX`, double, then clamp at the option's floor), `SockOpts::new(typ)` gives a
    fresh socket its family's default (212992/212992 for a datagram socket,
    16384/131072 for a stream), and the two `setsockopt` arms store the result, so
    `getsockopt` needs no change. The governing half is two questions of one number:
    `udp_send` asks it before `send_slice` and answers `EAGAIN` (the same answer the full
    ring gives, which is what makes non-blocking and blocking one rule), and
    `udp_room_for` asks it again so the park's predicate cannot come true while the
    enqueue would still refuse. Linux's grace is in both: an empty queue admits one
    datagram whatever its size. The datagram ring rose from 4 KiB to **16 KiB** and from 4
    packet slots to **16**, which is what lets the buffer be the smaller of the two
    numbers - a ring under 4608 could never be the one that binds.

    Pin: `t-sockbuf`, a new program, **12 claims** (the four transformations on both
    options, the two floors, the ceiling a negative ask names, the two family defaults and
    the doubling on a stream socket), all green on a current host first - the floors and
    the ceiling are the host's sysctls there and this kernel's constants here, which is why
    one program pins both. Plus `t-udpblock`'s `the fill stops where the send buffer's
    number says`: with the buffer set to its floor, three 1400-byte datagrams go in and the
    fourth is refused (`3 * 1400 <= 4608 < 4 * 1400`).

    Gate: x86_64 `pass=185 fail=0 (185 built)`, `mpos-kunit 32 passed, 0 failed`, exit 0;
    aarch64 `pass=185 fail=0 (185 built)`; both machines' `t-sockbuf` and `t-udpblock` `ok:`
    lines identical (12 and 16).

    Counterfactual (`sock_buffer_size` back to storing the ask unchanged, the tests kept):
    `FAIL t-sockbuf` and `FAIL t-udpblock` - the doubling claims, the floors and the fill's
    stop point all red by name.

    Still named, and measured: `SO_RCVBUF` is the number a caller reads and not a window
    this kernel resizes (the receive ring is a fixed 16 KiB, so a smaller ask does not
    shrink it - a stream's became its window in item 98; a datagram's ring is still fixed);
    the datagram-too-big line remains the ring's rather than Linux's 65507,
    because this stack has no IP fragmentation and a wider datagram could not leave the
    machine; and a floor buffer admits three datagrams here against Linux's two, this
    kernel charging the payload where Linux charges each skb's truesize.

## item 59 — a `clone` child's own exit signal is what its death reports (item 50's other half)

    §5.2's entry, and the half item 50 named and did not build. `clone`'s low byte is the
    signal a child's death should report; item 50 read it at the *waiting* end, this reads it
    at the *dying* end, where Linux's `do_notify_parent` sends that signal verbatim: a `fork`'s
    child reports `SIGCHLD`, a `clone(SIGTERM)` child reports a `SIGTERM`, and a `clone(0)`
    child reports **nothing at all**. This kernel raised `SIGCHLD` through every door whatever
    the byte said — the whole of what the entry left named — so a `clone(0)` death raised a
    spurious `SIGCHLD` and a `clone(SIGTERM)` death announced the wrong signal.

    **Measured whole on a current host first** (`.workbuddy/probe_exitsig.c`, Linux 7.2.11, the
    child's byte the only variable, each scenario in its own forked child and the parent's
    queue seeded first):

    * **A1–B1 — the byte is the signal**: a `clone(SIGTERM)` child's death is read by a blocked
      `rt_sigtimedwait` as `si_signo` `SIGTERM` (15), a `clone(SIGUSR2)` child's as `SIGUSR2`
      (12), each with `si_code` **`CLD_EXITED`** (1), the child's pid in `si_pid`, its real uid
      in `si_uid` and its exit code in `si_status` (A2 confirms the uid is the parent's own);
      beside it a plain `wait4` that is `ECHILD` and a `__WCLONE` that collects the corpse
      (item 50's halves, unmoved);
    * **C1 — `exit_signal == 0` delivers nothing**, whatever the mask: a `clone(0)` child with
      the parent's `SIGCHLD` pinned leaves the queue empty, while `__WCLONE` still collects it;
    * **E1, F1 — `SIG_IGN` asks first, and *blocking* brings the record back** for anything but
      `SIGCHLD`: an unblocked, ignored `SIGTERM` death delivers nothing, a blocked one **is**
      delivered — where K1's blocked, ignored `SIGCHLD` is still dropped, the one place "a
      blocked signal is never ignored" does not hold;
    * **G1, H1 — the `SIG_DFL` shape**: `clone(SIGURG)` delivers nothing unblocked and arrives
      (as `SIGURG`, 23) once blocked;
    * **I1 — the consequence that makes the byte have to be delivered**: a `SIG_DFL`, unblocked
      parent is **killed by the `SIGTERM`** its `clone(SIGTERM)` child's death sends;
    * **J — the default-ignored set**: as the exit byte at `SIG_DFL` unblocked, `SIGURG` (23)
      and `SIGWINCH` (28) are dropped while `SIGIO` (29), `SIGUSR1` (10) and `SIGHUP` (1) are
      delivered — so the set is `{SIGCHLD, SIGURG, SIGWINCH}`. **J cannot see `SIGCONT`**, whose
      own default is *resume* rather than a fatal no-op: a second probe
      (`.workbuddy/probe_exitsig_cont.c`, same kernel) stops the helper with `SIGSTOP` and asks
      whether the child's death resumes it — it does, so `SIGCONT` is **delivered** and is *not*
      in the set, whatever the J row's "helper survived" reads like.

    **The fix is a predicate, one constant and a widened record.** `sched::signal` gains
    `SIGURG` (23), `default_action`'s `Ignore` arm takes it beside `SIGCHLD`/`SIGWINCH` (Linux's
    `sig_kernel_ignore`), and `ignored_by_default` is exactly that arm — the three signals, not
    `SIGCONT`, whose default is a *state change* and which the second probe shows delivered.
    `SigInfo::child` takes the signal as its first argument, because the record's `si_signo` is
    the death's signal and not a constant `SIGCHLD`. `sched::notify_parent_of_death` asks the
    parent's disposition and mask — `0` never; `SIGCHLD` when `!SIG_IGN && (handler || blocked)`;
    any other signal when `SIG_IGN ⇒ blocked`, else `!default || blocked ||
    !ignored_by_default` — and `child_exit.rs` calls it with `t.task(id).exit_signal`. It uses
    the private `enqueue` rather than `queue_signal`, because `discarded_when_generated` would
    drop an ignored signal *before* F1's blocked-ignored rule could keep it; the namespace-init
    guard still applies. `hal::SignalInfo::chld_union` already keys on `si_code ∈ 1..=6`, so a
    non-`SIGCHLD` death record renders its fields with no further change.

    Pin: **`t-clone-exit`**, a new program, **29 claims** — the record's four fields for a
    `clone(SIGTERM)`; the plain-`wait4` `ECHILD` and the `__WCLONE` collection beside it;
    `clone(0)`'s silence under a blocked `SIGCHLD`; the `clone(SIGCHLD)` control; the `SIG_IGN`
    pair (unblocked dropped, blocked delivered — F1); the `SIGURG` pair (default dropped,
    blocked delivered as `SIGURG`, not `SIGCHLD`); the `SIGCONT` blocked claim (delivered, not
    an ignored-by-default signal); the `SIGCHLD` blocked-ignore control (K1); and a must-die
    helper that does not catch the `SIGTERM` a `clone(SIGTERM)` child's death sends a `SIG_DFL`,
    unblocked parent. Every claim is Linux's answer, so `MPOS_ON_HOST=1` is the measurement:
    29 of 29 on this host first, and the sweep is the pin. Plus the host unit test
    `only_the_three_sig_kernel_ignore_signals_are_ignored_by_default`, which walks the set.

    Gate: x86_64 `pass=186 fail=0 (186 built)`, `mpos-kunit 32 passed, 0 failed`, exit 0 (186
    staged, 11 left out by `tests.toml`); the second machine's arm is `pass=185 fail=0`,
    `mpos-kunit 32 passed, 0 failed`, exit 0 (185 staged, 12 left out); and the program's 29
    `ok:` lines are byte-identical across the two machines (diffed, carriage returns aside).

    Counterfactual, and it is sweep-only because the change is one byte at the call site: with
    the old kernel's byte put back (`let sig = signal::SIGCHLD;`, every pin kept) stage 1 is
    untouched - `571 passed; 0 failed` - and the sweep is `pass=185 fail=1 (186 built)` with
    `FAIL t-clone-exit` and **ten** red claims by name: `its death delivers a SIGTERM, not a
    SIGCHLD`, `with CLD_EXITED in si_code`, `the child's exit code in si_status`, `the child's
    pid in si_pid`, `the child's real uid in si_uid`, `clone(0) delivers nothing at all, even
    with SIGCHLD blocked`, `a blocked, ignored SIGTERM death **is** delivered`, `blocked, the
    death arrives as SIGURG - its own number, not SIGCHLD`, `SIGCONT is not ignored by default:
    blocked, the death arrives`, and `a SIG_DFL, unblocked parent is killed by its
    clone(SIGTERM) child's death`. The controls pass on both kernels, which is what keeps the
    reds on the delivery rather than on the wait: `a plain wait4 still does not see a clone
    child (ECHILD)`, `__WCLONE still collects it`, `clone(SIGCHLD) blocked still delivers
    SIGCHLD`, and `an ignored SIGCHLD death is dropped even when blocked`. The first five reds
    are one wait that timed out (only `SIGCHLD` was ever queued, so the `SIGTERM` read is
    `EAGAIN` and the five field claims read the zeroed record); the last five are the
    dispositions the old constant got wrong.

    Still named, and measured: `__WNOTHREAD` stays
    accepted-and-ignored (item 50's own), and the *state change* a death `SIGCONT` carries —
    Linux's sender-side resume of a stopped parent, measured — is the `kill` path's alone and
    is not performed for a death record. The third thing this paragraph used to name — the
    record's own byte as a *report*, `/proc/<pid>/stat`'s `exit_signal` — **landed 2026-10-19**
    (item **63**), and the file it named was wrong: it is `stat` and not `status`, since Linux's
    `status` has no such line at all (measured).

## item 60 — a read of a watched file reports `IN_ACCESS`, at the doors and not in the VFS

    §5.2's entry, and the divergence the inotify module's own header had carried since it
    landed: `IN_ACCESS` was never emitted, on a rationale that had gone stale — "the read path
    does not bump atime, so the event would be half a lie". The read path *does* bump atime now
    (the ext4 `relatime` bump, §3.3), and a read is an access whatever the stored time then
    says, so the honest thing was to emit it. *Where* it belongs took measuring, and the answer
    is not the VFS: Linux's `fsnotify_access` sits at the syscall boundary, because the kernel's
    own readers (`read_file`, the `execve` ELF load) reach `File::read` too and Linux's
    `kernel_read` reports nothing.

    **Measured whole on a current host first** (`.workbuddy/probe_inotify_access.c`, Linux
    7.2.11, each scenario in its own forked child with its own queue):

    * **A — one `IN_ACCESS` per successful read that moved bytes**, mask `0x1`, empty name:
      `read(4)` then `read(4)` report two, `pread` reports one, and a **read at EOF (0 bytes)**
      and a **zero-length read** report nothing; a read on an `O_WRONLY` descriptor (`EBADF`)
      and a directory read (`EISDIR`) report nothing; and a **`mmap` page fault reports
      nothing** (D: touching a mapped page queues only `OPEN` and `CLOSE_NOWRITE`);
    * **B, C — the life, in order**: `IN_OPEN` on the open, `IN_ACCESS` on each read,
      `IN_CLOSE_NOWRITE` on the close;
    * **L — a vectored read is *one* read**: a two-iovec `readv` that fills both reports
      **one** `IN_ACCESS`, and a two-iovec `preadv` one — the record belongs to the call, not
      the iovec;
    * **E, I, J, K — the copy doors count**: `sendfile` reports one whether its offset is
      `NULL` (the cursor, E) or a caller's word (I); `copy_file_range`'s source reports one
      (J); and a file→pipe `splice` reports one (K) — while a **pipe→file `splice` reports
      `IN_MODIFY` (0x2) and no `IN_ACCESS`** (M), because the file is written, not read;
    * **F — the identity is the inode**: two descriptions of one file each report their own
      read; and **the mask filters**: a watch that did not ask for `IN_ACCESS` is silent
      through it all.

    **The fix is a chokepoint at the doors, not in the VFS.** `vfs::inotify::emit_access` is
    the record (the shape of `emit_write`), and `syscall::linux::fs::file::notice_access(fd)`
    is the one call-site helper — it looks the description up and reports on its inode — asked
    by the three read-family doors (`handle_read`, `handle_pread64`, and `handle_readv`, whose
    body moved into `readv_inner` so the wrapper can report once for `readv`/`preadv`/
    `preadv2` together), and by the three copy doors (`handle_sendfile`,
    `handle_copy_file_range`, and `handle_splice` when its input is not a pipe). It sits there
    and not in `File::read` because `read_file`/`read_file_opt` — the loader's way in — reach
    `File::read` and Linux's `kernel_read` does not report. The inotify module's header drops
    the stale `IN_ACCESS` sentence and keeps the three divergences that remain.

    Pin: `t-inotify` grows **22 claims** (24 → 46): the one-record read, the wrong-mask
    control, EOF, zero-length, the two-iovec `readv`, `pread`, `sendfile`, the file→pipe
    `splice`, `copy_file_range`, the failed read and the directory read, beside the shape
    claims. Every one is Linux's answer, so `MPOS_ON_HOST=1` is the measurement: 46 of 46 on
    this host first, and the sweep is the pin. Plus the host unit test
    `emit_access_reports_a_read_only_to_a_watch_that_asked`, which pins the record's shape and
    the mask filter.

    Gate: x86_64 `pass=186 fail=0 (186 built)`, `mpos-kunit 32 passed, 0 failed`, exit 0 (186
    staged, 11 left out by `tests.toml`); aarch64 `pass=185 fail=0`, `mpos-kunit 32 passed, 0
    failed`, exit 0 (185 staged, 12 left out); and the program's 46 `ok:` lines are
    byte-identical across the two machines (diffed, carriage returns aside).

    Counterfactual, and it is sweep-only because stage 1 does not touch the doors: with
    `notice_access` made a no-op (every call site kept, the unit test still calling
    `emit_access` directly), stage 1 is untouched — `572 passed; 0 failed` — and the sweep is
    `pass=185 fail=1 (186 built)` with `FAIL t-inotify` and **six** red claims by name, one per
    door that reports: `a read reports one IN_ACCESS`, `a two-iovec readv reports one
    IN_ACCESS`, `a pread reports one IN_ACCESS`, `sendfile reports one IN_ACCESS`, `a
    file-to-pipe splice reports one IN_ACCESS`, and `a copy_file_range source reports one
    IN_ACCESS`. The controls — EOF, zero-length, the failed read, the directory read and the
    wrong-mask watch — stay green, which keeps the reds on the emit rather than on the read.

    Named, and measured rather than fixed: a *write* made by `copy_file_range`/`splice` is
    still invisible to `IN_MODIFY` (both write through `Inode::write_at`, below the
    `File::write` chokepoint that reports), where Linux reports `IN_MODIFY` on the output;
    `IN_UNMOUNT` and a watched directory's `IN_MOVE_SELF` remain the two named departures the
    module header keeps.

## item 61 — a write of a watched file reports `IN_MODIFY`, once per call

    Item 60's own named residual, and the write half of the same rule. The entry it closes: a
    `copy_file_range` or a pipe→file `splice` moved bytes through `Inode::write_at`, *below*
    `File::write` — the one chokepoint that reports `IN_MODIFY` — so a watcher on the
    destination heard nothing at all, and the `File::write` comment claiming every
    `write`/`writev`/`pwrite`/`splice`/`copy` handler reached it "so the event is said once" was
    false for two of them.

    **Measured whole on a current host first** (`.workbuddy/probe_inotify_modify.c`, Linux
    7.2.11, one scenario per forked child with its own queue). The rule turned out to be
    exactly the one `IN_ACCESS` already follows — *one `IN_MODIFY` per successful write-family
    or copy call, however many pieces the call wrote*:

    * **U — the scalar door**: `write` reports one, and one only;
    * **R, S, T — a vectored write is *one* write**: a two-iovec `writev`, `pwritev` and
      `pwritev2` each report **one** `IN_MODIFY`, not one per iovec;
    * **O — `sendfile` counts per *call*, not per chunk**: a 10 KB copy (three 4 KiB chunks)
      reports one on the destination, the same as an 8-byte one;
    * **N — `copy_file_range` likewise**: one on the destination at both sizes, and **Q** — the
      *source* of the copy, being read and not written, reports none;
    * **P — a pipe→file `splice`**: one on the output file, at both sizes.

    **The fix is one quiet write and one symmetric helper.** `File::write` splits into
    `write_reporting(buf, report)`; `write_quiet` is the `report = false` form — `remove_privs`
    still runs (a set-id file is still stripped), only the `IN_MODIFY` is held — and
    `vfs::write_quiet(fd, buf)` is its free-function form. The vectored and copy doors write
    their whole call through it and say the one event themselves, from
    `syscall::linux::fs::file::notice_modify(fd)` — the mirror of item 60's `notice_access`:
    it looks the description up and reports `emit_write` on its inode. The call sites are the
    `handle_writev` wrapper, whose body moved to `writev_inner` so `writev`/`pwritev`/
    `pwritev2` share one report, and `handle_sendfile`/`handle_copy_file_range`/`handle_splice`
    (the last only when its output is not a pipe — the input-pipe case is item 60's
    `IN_ACCESS`). The scalar `write`/`pwrite` doors are untouched: they reach `File::write`,
    which already reports once, and `notice_modify` is not called near them. The `File::write`
    comment is corrected, and the inotify header and `emit_write` doc now name the rule.

    Pin: `t-inotify` grows **21 claims** (46 → 67): the mask filter, the scalar write, a
    two-iovec `writev`/`pwritev`/`pwritev2` each reporting one, a zero-length write reporting
    nothing, and a 10 KB `sendfile`/`copy_file_range`/pipe→file-`splice` each reporting one on
    the destination. Every one is Linux's answer, so `MPOS_ON_HOST=1` is the measurement: 67 of
    67 on this host first, and the sweep is the pin. Plus the host unit test
    `write_quiet_moves_bytes_without_reporting_modify`, which pins that a quiet write still
    moves the bytes and advances the cursor and reports nothing.

    Gate: x86_64 `pass=186 fail=0 (186 built)`, `mpos-kunit 32 passed, 0 failed`, exit 0 (186
    staged, 11 left out by `tests.toml`); aarch64 `pass=185 fail=0`, `mpos-kunit 32 passed, 0
    failed`, exit 0 (185 staged, 12 left out); and the program's 67 `ok:` lines are
    byte-identical across the two machines (diffed, carriage returns aside).

    Counterfactual, sweep-only for the same reason as item 60's (the unit test calls
    `write_quiet` directly and never reaches a door): with `notice_modify` made a no-op (every
    call site kept, the unit test still calling `write_quiet`), stage 1 is untouched — `573
    passed; 0 failed` — and the sweep is `pass=185 fail=1 (186 built)` with `FAIL t-inotify`
    and **six** red claims by name, one per door that reports: `a two-iovec writev reports one
    IN_MODIFY`, `a two-iovec pwritev reports one IN_MODIFY`, `a two-iovec pwritev2 reports one
    IN_MODIFY`, `a 10 KB sendfile reports one IN_MODIFY`, `a 10 KB copy_file_range reports one
    IN_MODIFY on the destination`, and `a pipe-to-file splice reports one IN_MODIFY`. The
    controls — the scalar `a plain write reports one IN_MODIFY`, the zero-length write and the
    wrong-mask watch — stay green, which keeps the reds on the emit rather than on the write.

    Nothing new is left named for the write side; the module's other two departures
    (`IN_UNMOUNT`, a watched directory's `IN_MOVE_SELF`) are unchanged.


## item 62 — `numa_maps` and `smaps_rollup`: the two other spellings of `smaps`' accounting

    Item 54 gave `/proc/<pid>/smaps` Linux's shape and named the three numbers this
    kernel cannot derive. What it left were the two files `docs/uapi/` §5.2 had already
    measured and never written - the entry said so ("their formats are measured so the
    next landing does not have to re-derive them") and both stayed absent, so
    `numastat -p`, `numactl` and every reader that wants a total rather than a walk
    found no file at all.

    **Measured whole on a current host first** (`.workbuddy/probe_numamaps.c`,
    `probe_numamaps2.c`, `probe_rollup.c`, Linux 7.2.11, one node). Three of the
    measurements are the ones that decided the shape, and two of them are mistakes
    the first draft of the probe made:

    * **`/tmp` is tmpfs on this host**, so every "file" mapping made there is a shmem
      mapping whose folios are dirty from the moment the probe wrote them - `dirty=4`
      for a read-only mapping, meaning nothing. The files moved to ext4 and the answer
      changed shape.
    * **adjacent anonymous mappings merge**, so two shapes became one VMA and one of
      them looked absent. Every shape is fenced by `PROT_NONE` pages now, and the
      `maps` line beside every answer proves it.

    The rest is what Linux prints:

    * **the line is `<start> default` and then the name, and the address carries no
      `-end`** - unlike a `maps` line - with the policy word `default` for every mapping
      a task that never called `set_mempolicy` has;
    * **a range with no resident page prints the policy and no number at all**, not a
      count of zero (an untouched mapping, and a `PROT_NONE` guard beside it);
    * **`anon=` and `dirty=` are the whole resident count** for the two shapes this
      kernel has: `anon=1 dirty=1 active=0 N0=1 kernelpagesize_kB=4` for a written
      private anonymous page (and `heap anon=5 dirty=5 …` / `stack anon=11 dirty=11 …`
      for the two named ranges), and `dirty=1` with no `anon=` for a written `memfd`
      page, which is an object's and not the address space's;
    * **`dirty=` versus `mapped=` turned on one shape, and only that shape could say
      which**: a file mapping that is *writable but never written* prints `mapped=4`,
      and the same mapping written prints `dirty=4` - so it is dirtiness and not the
      mapping being writable. A read-only file mapping prints `mapped=4` too;
    * **reading an anonymous page gathers nothing on Linux** - a read fault is
      satisfied with the shared zero page, which is not a page it counts - and that is
      the one place this kernel's answer differs, since a read fault here materialises
      a real frame. Named rather than claimed;
    * **a path's space is `\040` and its tab `\011`** (`seq_file_path`'s octal escape,
      measured with a path carrying both; a backslash is left alone), and a name that
      is not a path - `[vdso]`, `[vvar_vclock]` - is not printed at all;
    * **`mapmax=` is a mapcount** (it appeared at 26, 104, 112, 115 across a real
      process) and **`mapped=` appears exactly where Linux's own `anon`+`file` counts
      do not add up to the total**, which is a clean file-backed range.

    `smaps_rollup` is one record for the whole address space: a header that is the
    *bounds* of the space - `<first start>-<last end> ---p 00000000 00:00 0`, with
    `[rollup]` in the same column 73 a `maps` line names its mapping - and then
    twenty-two field lines, which is `smaps`' twenty-five with three taken out and
    three put in: out go `Size`, `KernelPageSize`, `MMUPageSize` (one mapping's) and
    `THPeligible`, `ProtectionKey`, `VmFlags` (one mapping's flags); in come
    `Pss_Anon`, `Pss_File` and `Pss_Shmem`. The gate VMA is the one exception the
    bounds have, and it is Linux's own: `[vsyscall]` has a `maps` line and is in
    neither bound, because it is not a VMA of the address space. Its numbers are the
    sums of `smaps`' - `Rss` measured exactly (`smaps` 2096, rollup 2096) where `Pss`
    is not (278 against 281, because each record's is rounded before it is added).

    **The fix is two renderers over the records `maps` already gathers.**
    `procfs::format_numa_maps` and `procfs::format_smaps_rollup` are the ABI and are
    compiled for the host, like `format_smaps`; `resident_lines` is the one place the
    residency is counted, so the three files that are `maps` plus accounting cannot
    disagree; `push_escaped_path` is the octal escape. The two nodes sit beside
    `smaps` in `/proc/<pid>`.

    What is deliberately **not** Linux's, each named rather than approximated:
    `mapped=` and `mapmax=` are absent (no mapcount - the same fact `Pss` names), the
    `stack` word is absent (this kernel names the heap and not the stack), a
    *read-only* object-backed range answers `dirty=N` where Linux answers `mapped=N`
    (no dirty bit), and `Pss_Shmem` is zero (a `memfd`'s pages are an object's here,
    and Linux's shmem/file split is a split about its page cache). `Pss_Anon` and
    `Pss_File` *are* the split this kernel can make, because it is the question
    `Anonymous` already answers.

    Pin: **`t-numamaps`**, a new program, **20 claims** - the shape (a start and the
    policy, no `-end`, one line per mapping and in `maps`' order, the gate VMA
    excepted), an untouched range's policy with no number, a written anonymous range's
    three counts of three, `active=0`, the 4 kB frame, the absence of `mapped`/`mapmax`,
    the same residency `smaps` reports, an object's page as `dirty` and not `anon`,
    the heap's word, and a pid nothing answers to - and **`t-smaps`' ten new claims**
    (33 → 43): the `[rollup]` name in column 73, the bounds read out of `maps`, the
    `---p 00000000 00:00 0` word, Linux's twenty-two lines in Linux's order, `Rss` and
    `Anonymous` as the sums of the same fields over every record, the features that
    answer zero in the total, and a pid nothing answers to. Every claim was run against
    a current host first (`MPOS_ON_HOST=1`: 20 of 20 and 43 of 43, three times each,
    because a total and a sum only agree while the address space is still), plus two
    host unit tests (`a_numa_maps_line_is_the_start_the_policy_and_the_resident_count`,
    `the_rollup_is_one_record_for_the_whole_space`) and the escaping's own
    (`a_numa_maps_path_escapes_what_would_break_the_line`), which is where a claim
    needing a filesystem that can hold a space belongs.

    Gate: x86_64 `pass=187 fail=0 (198 built)`, `mpos-kunit 32 passed, 0 failed`, exit 0
    (11 left out by `tests.toml`); aarch64 `pass=186 fail=0`, `mpos-kunit 32 passed, 0
    failed`, exit 0 (12 left out); and both programs' `ok:` lines are byte-identical
    across the two machines (20 and 43, diffed, carriage returns aside).

    Counterfactual: with the two nodes unregistered - the kernel this item started
    from, everything else kept - the sweep is `pass=185 fail=2 (187 built)` with
    `FAIL t-numamaps` and `FAIL t-smaps` and **sixteen** red claims by name, eight in
    each program: the file's own existence and every shape claim in one, and the
    rollup's header, bounds, field list and both sums in the other. The two
    "a pid nothing answers to" claims stay green, which is what keeps the reds on the
    files rather than on the reading.

    Nothing new is left named for either file; the four divergences above are the
    list, and each is a consequence of a fact `smaps` already carries.

## item 63 — `/proc/<pid>/stat`'s `exit_signal` is the task's own byte, and a thread's is `-1`

    The last named thing on the `clone` byte's ladder, and the one both item 50 and item 59
    left standing in the same words: "a rendering nothing pins". `/proc/<pid>/stat`'s field
    38 answered the constant **`17`** through every record, so a supervisor reading a
    `clone(0)` child's record, or a thread's, was told a `SIGCHLD` no death of theirs would
    ever deliver.

    **Measured whole on a current host first** (`.workbuddy/probe_exit_signal_field.c`, Linux
    7.2.11), the `clone` byte the only variable and every record read from a **live** child:

    * **a `fork`'s child is `17`** — `SIGCHLD`, the byte `fork` is;
    * **a raw `clone(0)` child is `0`** and **a `clone(SIGTERM)` child `15`** — its own
      number, which is what makes the field a report rather than a constant;
    * **a thread is `-1`** — a number no signal byte can carry, because Linux's
      `copy_process` *rewrites the byte away* for a task created with `CLONE_THREAD` (a
      thread never notifies anyone of its death) and then prints what it stored. So the
      field is a **signed** word, and the same `-1` comes back from `/proc/<pid>/task/<tid>/
      stat` **and** from the tid's own `/proc/<tid>/stat`;
    * **the *leader* of a threaded process keeps its own byte** (`17`), which is what keeps
      the `-1` on the task's *shape* rather than on the process having threads;
    * **a zombie keeps the byte it died with** — the field is the task's stored byte, not
      something the exit path recomputes;
    * and **`/proc/<pid>/status` has no such line at all**, which the same probe checked: the
      two paragraphs that named this field said "`status`'s `exit_signal` line", and the file
      is `stat`. The naming is corrected here, in `docs/uapi/` §5.2 and in `docs/ROADMAP.md`.

    **The fix is one field, one predicate and one rendering.** `sched::TaskSnapshot` carries
    the byte (`exit_signal`, filled from `Task::exit_signal`), and
    `vfs::procfs::exit_signal_field(byte, is_thread)` is the rule. The report is *derived*
    from the task's shape rather than read off the stored byte, and that is deliberate: this
    kernel's threads **store** `SIGCHLD`, because the one place a thread does leave a record
    is its group's `Retired` and a default `wait` that named that slot must not read it as a
    "clone" child's (the creation site's own comment says so in full) - where Linux stores
    `-1` because nothing there reads its byte at all. So printing the stored byte would have
    reported a thread as a `SIGCHLD` child: what has to match Linux is the *answer*, and the
    task's own shape is the fact both kernels agree on (`p->pid != p->tgid`). Owner:
    `src/sched/runtime/procfs.rs`'s
    `TaskSnapshot`/`snapshot_of` and `src/vfs/procfs.rs`'s `exit_signal_field` and field 38.

    Pin: **`t-clone-exit`'s 12 new claims** (29 → 41) — the five records (each from a live
    child), the leader's control, the thread's `-1`, and the zombie's state-and-byte pair -
    every one green on a current host first (`MPOS_ON_HOST=1`, 41 of 41, run three times
    because a claim read out of a live task's file is read while the machine moves), beside
    the host unit test `a_tasks_exit_signal_field_is_its_byte_or_minus_one_for_a_thread`,
    which walks the table with no machine under it. Every claim is Linux's answer, so a host
    run is the measurement and the sweep is the pin.

    Gate: x86_64 `pass=187 fail=0 (187 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33
    (187 staged, 11 left out by `tests.toml`); aarch64 `pass=186 fail=0`, `mpos-kunit 32
    passed, 0 failed`, exit 0 (186 staged, 12 left out); and the program's 41 `ok:` lines are
    byte-identical across the two machines (diffed, carriage returns aside).

    Counterfactual (`Cow::Borrowed("17")` put back for field 38 - the kernel this item
    started from, the field, the predicate, the unit test and all 41 claims kept): stage 1 is
    **untouched** (`577 passed; 0 failed`), because the unit test calls `exit_signal_field`
    itself and never reaches the rendering, and the sweep is `pass=186 fail=1 (187 built)`
    with `FAIL t-clone-exit` and **three** red claims by name - `a clone(0) child's is 0 - the
    byte it was born with, not a constant`, `a clone(SIGTERM) child's is its own number` and
    `a thread's stat field 38 is -1, the number no byte can carry`. The other nine new claims
    pass on both kernels *by construction*, and saying which is the point: the `fork` child, the
    leader and the zombie all report `17` either way, and the four "was started" claims plus the
    zombie's state are preconditions rather than pins. It is the two `clone` bytes and the `-1`
    that carry the item, and each of them is a value the constant cannot produce.

    One shape stayed named rather than served, with its measurement in the same probe: a
    thread had **no `/proc/<tid>` directory** here, where Linux serves one - the field and
    everything beside it were reachable through `<pid>/task/<tid>/stat` alone, because this
    tree's `/proc` directory is a *leader*'s (`pid_exists` asks `snapshot(pid).pid == pid`).
    It was closed the next day by **item 64**, the item below, which serves the tid's own
    directory; the field is now claimed through both spellings.

    The pin's own first draft failed in a way worth recording, because the failure *is* item
    59's rule: the clean-up killed the `clone(SIGTERM)` child with `SIGKILL`, and that child's
    death delivered a `SIGTERM` to the test process - which had `SIGTERM` at `SIG_DFL` by then
    - so the program was killed by its own test subject after 39 of its 41 claims. The kill
    now happens with `SIGTERM` blocked and the record it queues is taken rather than
    delivered, and the comment says why.


## item 64 — a thread's tid names a `/proc` directory, and a dead thread names nothing

    The gap item 63's pin found and §5.2 carried: this tree's `/proc` directory was a
    *leader*'s - `pid_exists` asked `snapshot(pid).pid == pid` - so `/proc/<tid>` was `ENOENT`
    for every thread, and a thread's record was reachable through `<pid>/task/<tid>/stat`
    alone. Item 63's own entry named the fix as "the *lookup* and everything it lets through
    rather than one field", and §5.2 named the question to settle first: **which of the two
    names a program uses**.

    **That question is measured now**, on a current host under `strace`
    (`.workbuddy/probe_tid_dir.c`, Linux 7.2.11): `ps -eLf` walks the group - **306** opens of
    the *relative* name `task`, beside not one absolute `/proc/<pid>/task/<tid>` path - while
    `gdb -p <tid>` opens **`/proc/<tid>/status`** outright. Both spellings have a program
    behind them; the `task/` one is `procps`'s and the tid's own is a debugger's, which is also
    the shape `/proc/thread-self` hands out (its target text is `<pid>/task/<tid>`, and what it
    *means* is the tid's directory).

    **Measured whole in the same probe**, a live thread of a live group:

    * **the two spellings are different file sets, and it is Linux's own split.**
      `/proc/<tid>` carries the **process** set - its names are the leader's exactly, **54
      against 54** - while `/proc/<pid>/task/<tid>` is the narrower **thread** set (**47**):
      `children` in, and `autogroup`, `coredump_filter`, `map_files`, `mountstats`, `task`,
      `timens_offsets`, `timers`, `timerslack_ns` out. That is `base_stuff` against
      `tid_base_stuff`, and it is why `/proc/<tid>/children` is `ENOENT` where the `task/`
      spelling answers it;
    * **`readdir /proc` lists no tid**, with its leader listed: a listing carries processes and
      a tid is looked *up* (367 entries, the tid absent);
    * **`stat`'s field 1 is the tid** and **field 38 is `-1`**, where the leader's own record
      says the pid and its own byte - item 63's rule, reached through the name it could not use;
    * **`status` is the pair, not one number twice**: `Tgid 16084 / Pid 16085 / NStgid 16084 /
      NSpid 16085 / NSpgid 16082 / NSsid 16075`;
    * **the *task's own* structures are what the tid's directory reads** - the half a
      leader-shaped answer gets wrong. A thread that `unshare(CLONE_FS)`s and `chdir`s reports
      its own `cwd` through `/proc/<tid>/cwd` (`/tmp`) while its leader still reports the one the
      group kept, and a thread that `unshare(CLONE_FILES)`s lists its new descriptor under
      `/proc/<tid>/fd` alone (`6` entries against the leader's `5`);
    * and **a thread that has exited takes its tid with it**: it is out of `/proc/<pid>/task` and
      `/proc/<tid>` is `ENOENT` in the same instant, joined or not - Linux's
      `__unhash_process` unhooks a task at exit **unless it leads its group**, which is exactly
      why the zombie *process* of item 63's pin keeps its record. Two probes settle it rather
      than one reading: the tid's own directory is `ENOENT` after the thread returned, and a
      `/proc/<pid>/task` listing holds the leader alone.

    **The clause that came with it is the same measurement's.** A lookup has to be Linux's gate
    and not merely a wider one, and this kernel's table holds a dead thread's **slot** until the
    slot is reused (the thread's retired record is what holds it) - so `/proc/<tid>` would have
    answered for a task Linux had already unhooked. It is
    `vfs::procfs::tid_has_a_directory(state, tid, pid)`, one clause (`Z` **and** `tid != pid`),
    asked at the two funnels that *name* a task: `process_dir` and `thread_ids`. The question is
    asked of the *state letter* because that is what a snapshot carries - `snapshot_of` writes
    `Z` for `exited` and nothing else does.

    **The fix is the lookup plus the task-shaped fields, and nothing else moved.**
    `process_dir(slot)` is built for the **task** the name resolved to: `stat`'s field 1 and
    `status`'s `Pid`/`NSpid` are the tid's while `Tgid`/`NStgid` stay the group's, and the names
    whose Linux owner is the *thread group* are asked of the leader - `task` (one listing for the
    group), `limits` and `oom_score_adj` (Linux keeps both in the group's `signal_struct`).
    Every other file is the task's and is asked of the slot, which is what Linux does even where
    the two agree by construction: `maps`, `fd` and `ns` name the task's own structures, and the
    split-structure pair above is the case that tells them apart. The `task/` spelling keeps its
    own narrower set, which is why the tid's directory is *not* built by `thread_dir`. Owner:
    `src/vfs/procfs.rs`'s `process_dir`/`thread_dir`, `thread_ids`/`tid_has_a_directory`,
    `status_of`'s id pair and `Processes`' two halves.

    Pin: **`t-tid-dir`**, a new program, **26 claims** - the directory and its word, the name
    set, the listing, the two `stat` fields beside their two controls, the four `status` id
    lines, the group-shaped names reached through the tid, the split-structure pair, and the two
    dead-thread readings - every one green on a current host first (`MPOS_ON_HOST=1`, 26 of 26,
    run **five** times with identical `ok:` lines, because a claim is read out of a live task's
    file while the machine moves). `t-clone-exit` gains one more claim (**41 → 42**): item 63's
    `-1` is now read through **both** spellings, which is the whole of what this item closes.
    Beside the host unit test `a_dead_threads_tid_names_nothing_and_a_zombie_leaders_does`, which
    walks the rule with no machine under it (this kernel's dead thread *is* still in the table,
    which is why the rule is worth one).

    Gate: x86_64 `pass=188 fail=0 (188 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (11 of
    199 left out by `tests.toml`); aarch64 `pass=187 fail=0`, `mpos-kunit 32 passed, 0 failed`
    (12 left out); stage 1 `578 passed; 0 failed` (577 before, the new unit test), and the new
    program's **26 `ok:` lines are byte-identical across the two machines** (diffed, carriage
    returns aside) **and equal to this host's own run**. fmt clean, clippy's warning set
    unmoved (245), `cargo xtask depgraph` within the ratchet.

    Counterfactual (the lookup put back to `pid_exists` and the `thread_ids` filter dropped -
    the kernel this item started from, with the rule, its unit test, the new program and every
    claim kept): stage 1 is **untouched** (`578 passed; 0 failed`, because the unit test calls
    `tid_has_a_directory` itself and never reaches the lookup), and the sweep is
    `pass=186 fail=2 (188 built)` with `FAIL t-tid-dir` and **17** red claims by name and
    `FAIL t-clone-exit` with **one** - `and the tid's own directory reads the same record`. The
    nine that stay green say what the item is: *where the leader's own record still names the
    process*, *and the leader's is not `-1`*, *readdir /proc holds the leader*, *and does not
    hold the thread*, the two leader-side halves of the split pair, `the thread under test
    started`, and - the one worth a sentence - **`and its tid names no directory at all`**, which
    passes on the old kernel too because the tid never named one. It is the *live* readings that
    carry the item, and the claim about a tid naming nothing is a control rather than a pin.

    One thing stays named rather than served, and it is where the two spellings disagree about
    *ownership*: **`children` is the task's on Linux and the process's here.** Measured on a
    current host: a child forked by a non-leader thread appears under **that thread's**
    `/proc/<pid>/task/<tid>/children` alone, while this kernel's `children_of` ignores the tid
    and `child_ids` filters by pid - so a thread that forked is answered the group's list. Owner:
    `src/vfs/procfs.rs`'s `children_of`/`child_ids`.

    The pin's own first draft failed in a way worth recording, because it is a property of the
    file rather than a slip: **`read_dir` of a process's *own* `/proc/<pid>/fd` holds a
    descriptor open while it lists**, so the number it adds to the listing is one that process
    has - and the claim "the leader's fd does not list the thread's new descriptor" read the
    leader's own directory from the leader and found it there. The two descriptor claims are
    taken from a **forked child** now, where the transient descriptor lives in the child, and
    the helper says why.

## item 65 — a task's `children` are its own, and the parent a record names is its group

    Item 64's residual, and the last word on *which task* a record points at. Two things were
    wrong and one probe settled both, because they are the two answers one link gives:
    `children` was the *process's* list through every tid's file (`child_ids` filtered the task
    table by `ppid` and `children_of` ignored the tid it was handed), and the parent a record
    *names* was the forking task's own slot - so a child forked by a non-leader thread was told
    its parent was that thread, where Linux names the **group**.

    **Measured whole on a current host first** (`.workbuddy/probe_children_owner.c`, Linux
    7.2.11, a leader forking one child beside a thread forking another, both alive, three
    identical runs):

    * **the file is the `task/` set's, and only its.** `/proc/<pid>/children` and
      `/proc/<tid>/children` are both `ENOENT` **even for a task that has children** - so
      `children` is one of the names Linux's `tid_base_stuff` carries and `base_stuff` does not
      (which is item 64's 54-against-47 measured from the other side), and a reader reaches it
      through the group and no other way.
    * **the list is one task's.** With the leader `42204` forking `42205` and `42206` while the
      thread `42207` forked `42208`: `/proc/42204/task/42204/children` was `"42205 42206 "` and
      `/proc/42204/task/42207/children` was `"42208 "` - neither file held the other's child,
      and a `pthread_create`d thread appears on neither (Linux's `copy_process` puts a
      `CLONE_THREAD` task on no `children` list at all).
    * **the bytes are Linux's, and the empty case is the one a guess gets wrong**: each id
      followed by a space and no newline, and a task that has forked nobody answers a
      **zero-byte** file (`""`, 0 bytes) rather than a blank line - measured on the thread's own
      child, which had none.
    * **a corpse stays until it is reaped.** An exited child that has not been waited for was
      still on its parent's list (`"42205 42206 42209 "`), and the reading *after* the `waitpid`
      no longer carried it - so the list is the reap's, not the exit's.
    * **and the parent a record names is the group.** The child of the *thread* read `getppid`,
      `status`'s `PPid` and `stat`'s field 4 as **`42204`** - the group's leader - where the
      thread that forked it was `42207`, while the child of the *leader* read `42205`'s group,
      which is the same number as its forking task's. Three doors, one rule: Linux's
      `task_tgid_nr_ns(real_parent)`.

    The fix is one link, one key and one rendering. `sched::TaskSnapshot` gains `parent:
    Option<usize>` (the scheduler's own `real_parent`, which only `children` reads) beside a
    `ppid` that is now the parent's **group** (`sched::runtime::procfs::parent_group`, Linux's
    `task_tgid_nr_ns` read where the link is kept); `vfs::procfs::child_slots` is the list rule
    - keyed on the link, leaders only, a zombie included - and `child_ids` is that rule over the
    live table, so the file's own gate can ask the *task's* directory clause
    (`tid_has_a_directory`, item 64's) rather than the leader's `pid_exists`, which is deleted
    with this landing because nothing asks a leader's question any more; `children_of` writes
    each id through `reader_number` and Linux's own separator; and `syscall::linux::proc`'s
    `getppid` asks the new `sched::parent_group_of`, so the three doors cannot disagree.

    **The pin's own fixture then found two kernel defects, and both are Linux's answer rather
    than a test-side shape.** (a) **`fork` from a non-leader thread was `ENOMEM`**:
    `spawn_copy` asked the *calling task's* `space`, and a thread has none - the group's
    address space is the leader's slot - so a program that forked on a worker thread was
    refused a call Linux serves, which is exactly the fixture's shape (a thread forking a child
    is what tells the two `children` files apart). The fix is the lookup the kernel already
    makes in two other places, `Table::space_owner` - the same one `resolve_cow_fault` and
    `/proc` ask - used for the page walk, the `parent_has_space` test and the COW hardening of
    the parent's writable pages. (b) **A reaped child lingered in the table**: Linux's
    `release_task` runs *inside* `do_wait`, so a child leaves `/proc` and its parent's
    `children` list the moment a `wait` answers for it, where this kernel took the record but
    left the slot for the scheduler's next `reap()` - so a reading taken in between still saw
    the corpse (the leader's file carried an extra pid, and "the reap is what takes it off"
    failed). Both doors that collect (`take_event`, and `take_retired_current` for the blocking
    `wait4(pid)` path) now reap after taking the record.

    Pin: **`t-tid-dir`'s 11 new claims** (26 → 37) - the two spellings that carry no file, the
    two per-task lists, the zero-byte empty case, the group read through three doors, the
    leader-side control, and the corpse pair - every one green on a current host first
    (`MPOS_ON_HOST=1`, 37 of 37, three runs), beside the host unit test
    `a_children_file_lists_the_children_the_task_itself_forked`, which walks the rule (a
    thread's child, a corpse, a task with none) with no machine under it.

    The counterfactuals are one per half, each with every pin kept, and each moves the
    *machine-side* wiring rather than the rule the unit test calls: the `ppid` half put back to
    the forking task's own slot turns **two** claims red by name - `a child forked by a thread
    names the group in stat's field 4` and `and the same group in status's PPid` - while the
    `children` claims stay green; keying the thread's file on the group instead turns exactly
    the ownership claim red, `and the thread's is the child the thread forked, not the leader's`.

    One thing stayed named rather than served, and it was the link's *other* end: a **thread's**
    own parent. Linux's `copy_process` gives a `CLONE_THREAD` task its creator's parent ("threads
    share the parent", so `CLONE_THREAD` implies `CLONE_PARENT`) - measured in the same probe, the
    forking thread read `42203`, the group's *parent*, through all three doors while its own tid
    was `42207`. This kernel's thread carried **no link at all** (`parent: None`), so all three
    doors read `0` - not its creator, as this item's own first draft said: that sentence was read
    from the `fork` path and had never been measured against the tree. It is not a rendering,
    which is why this item stopped at the record: moving the link moves three things that need
    their own measurement first, and all three are measured.

    A thread's death must stop notifying anybody - a `fork` child's death queues `SIGCHLD` with
    the signal blocked while a thread's death queues nothing, where a thread with a link would
    reach the death path's `notify_parent_of_death` for its group's parent (the path itself is
    unconditional; the missing link is what spared it). A task's exit must hand its children to
    an *alive thread of its group* before init - the thread's child moved to the leader's list
    the moment the thread exited, its own three doors unmoved - where this kernel reparents to
    `INIT_PID`. And **`wait` asks a different question again**: Linux's `do_wait` walks every
    thread of the caller's group (`while (tsk != current->group_leader)`), so with the forking
    thread still alive the *leader's* `waitpid` answered `0` from `WNOHANG` and then collected
    the child outright (pid and status `7`), while this kernel's one eligibility rule asks
    `task.parent == Some(me)` - the calling task - so the same call is `ECHILD`. All three are
    named in `docs/uapi/` §5.2 with those readings, and all three land as **item 66**.

## item 66 — a thread is its group's child: the link, and the three questions that read it

Item 65's residual, and the other end of the same link. Linux's `kernel_clone` turns
`CLONE_THREAD` into `CLONE_PARENT`, so a thread's `real_parent` is the parent of the group it
joined - "threads share the parent" - and every door that reads a parent sees that one number.
This kernel's `spawn_thread` gave a thread **no link at all** (`parent: None`), so `getppid`,
`status`'s `PPid` and `stat`'s field 4 all read `0` for a live worker.

**Measured whole on a current host first** (`.workbuddy/probe_children_owner.c`, Linux 7.2.11):
the leader `87096`, whose own parent was `87094`, started the worker `87099`; the worker read
`87094` - its *group's* parent, not the tid that created it - through all three doors, while its
own tid was the one it ran as. Three behaviours move with that link, and the same session
measured all three:

* **a thread's death notifies nobody.** With `SIGCHLD` blocked and `sigpending(2)` polled, a
  `fork` child's death queued one record and a thread's queued none - Linux's `do_notify_parent`
  is gated on `thread_group_leader`.
* **a dying task hands its children to an alive task of its group before init.** The thread's
  child moved onto the leader's `children` list the moment the thread exited, its own three
  doors unmoved. The mirror needed its own probe, which item 65 had not taken:
  `.workbuddy/probe_leader_exit.c` has the *leader* leave on `pthread_exit` while its worker
  still runs, and the child moves onto the **worker's** list - its `PPid` still reading the
  group, because that is the worker's `tgid`. So the candidate is "any alive task of the group,
  the leader preferred", not "init unless the dying task is a thread".
* **`wait` walks every thread of the caller's group.** With the forking thread still alive the
  leader's `waitpid(child, WNOHANG)` answered `0` (not `ECHILD`), and a blocking wait collected
  the child outright - pid and status `7` - which is `while (tsk != current->group_leader)` read
  from the other end.

The implementation is four changes and one predicate.

* `spawn_thread` stores the creator's **parent** (Linux's `CLONE_PARENT`), not the creator.
* `trace::waitable` asks the caller's **group** (`forked_in_group`: a child forked on a sibling
  thread is the leader's to collect) and admits only a **group leader** as a child (a thread is
  on no `children` list, and its death is nobody's), with a tracer the one exception.
  `child_alive_for` asks the same two questions, so the live half of a wait and the record half
  cannot disagree; the scan sites thread the table and the task's own id through, because
  leadership is a fact about the slot (`task.tgid == id`).
* `exit` announces and hands the console back only when `tgid == id` - the leader gate that
  keeps the new link from turning every worker's exit into a `SIGCHLD` - and the reparent walk
  (the file's two walks consolidated into one, no `!exited` filter, so a zombie's link moves
  too) picks `alive_thread_of(t, tgid, id)`: the leader first, a live sibling next, init only
  when the group has no live task.
* `Task::reap` folds a child's CPU into its parent only for a **leader**: `cpu_all` is the
  group's, so a collected thread would otherwise charge the whole group's work to the parent
  once per thread. `trace::park` gates its parent wake the same way - a thread's stop is its
  group's, and Linux reports one from the leader.

Pin: **`t-tid-dir`'s 8 new claims** (37 → 45) - the thread's own three parent doors, the
leader's `WNOHANG` over a sibling's child, the adopted child on the leader's list, the leader's
wait collecting that death with the code it exited by, the list losing it, and a two-fork-deep
notification fixture: the observer blocks `SIGCHLD`, forks the group, the group starts and joins
a thread and stays alive, so the only record the observer could hold is the thread's. Every one
green on a current host first (`MPOS_ON_HOST=1`, 45 of 45, repeated), beside item 65's host unit
test which this item leaves untouched (the wait rule has no host-reachable table).

Gate: x86_64 `pass=188 fail=0 (188 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (11 of 199
left out by `tests.toml`); aarch64 `pass=187 fail=0`, `mpos-kunit 32 passed, 0 failed`, exit 33
(12 left out); stage 1 `579 passed; 0 failed`; both machines' 45 `ok:` lines byte-identical
(diffed, carriage returns aside). Static: `fmt --check` clean; clippy's warning set unmoved (77
on `x86_64-unknown-none`, 74 on `aarch64-unknown-none`, 3 in `mpos-xtask`); `cargo xtask
depgraph` within the ratchet (8/8 mutual pairs, no new edges).

**Four counterfactuals, one per half, each with every pin kept**, and each red exactly where it
should be:

| reverted | red claims by name |
|---|---|
| the thread's link (`parent: None`) | the three parent doors: `a thread's own getppid(2) is its group's parent`, `and the tid's status PPid is the same number`, `and its stat field 4 agrees with the other two doors` |
| the wait rule (the caller's slot, not its group) | `the leader may wait for a child its thread forked: WNOHANG answers 0, not ECHILD` |
| the reparent rule (always `INIT_PID`) | `so the child it forked is on the leader's children file now`, `and the leader's wait collects it, with the code it exited by` |
| the leader gate on the death path | `a thread's death queues no SIGCHLD for its group's parent` |

The last fixture is the item's own lesson: its first draft observed the *group* and stayed green
on the broken kernel, because a thread's parent link names its **group's** parent - the observer
has to be that grandparent, which is why the fixture is two forks deep.

**What stays named** is the shape this item deliberately does not move: a non-leader's
`exit_group(2)` still records nothing for the group and announces nothing to its parent. This
kernel's `exit_group` marks the siblings exited and runs the caller's exit path, and the leader
gate now says so on purpose; Linux reports the group's leader. It is a shape this item neither
introduced nor closed, and the one place a multithreaded program's parent can be told `ECHILD`
where Linux hands it the leader's status. (Item 67 below closes it.)

## item 67 — the group's last thread is the process's death, and the parent hears the leader

    Item 66's named residual, and the answer to *when* a process exists to be waited for. Linux's
    `group_dead` is the group's **last** thread leaving: everything a parent can see hangs on that
    moment - the zombie, the record, the signal - and the process it names is its **leader**. This
    kernel had two shapes wrong, and the same probe measured both. `exit_group` from a worker
    marked the siblings exited and ran the caller's own exit path, where item 66's leader gate then
    recorded and announced nothing - so a multithreaded program's parent was told `ECHILD` where
    Linux hands it the leader's status. And a leader that left first (on the thread door) was
    announced and made reapable at once, while a live worker still ran.

    **Measured whole on a current host first** (`.workbuddy/probe_exit_group_thread.c`, Linux
    7.2.11; five scenarios in one run, repeated):

    * **A. a worker's `exit_group(7)` while the leader pauses**: the parent's `waitpid(leader)`
      returned the **leader's** pid with status `0x700`; one `SIGCHLD` record carried
      `si_pid` = the leader, `si_status` 7; the worker's own tid answered `ECHILD`.
    * **B. the control**: the leader's `exit_group(3)` with a worker alive returned the leader
      and 3.
    * **C. the leader leaves first on `pthread_exit`**: `/proc/<leader>/stat` said `Z` while the
      worker ran, `waitpid(leader, WNOHANG)` answered **0** (not `ECHILD`), and only the group's
      death answered - with the **last thread's** code, `7` from the worker's `exit_group(7)`.
    * **C2. the last thread leaves on the plain thread door** (`SYS_exit(5)` while the leader is
      already a zombie): the wait answered the leader with **5**. The status is the dying
      thread's `exit_code`, not the leader's own.
    * **D. the exit byte is the leader's**: a group born through `clone(SIGUSR1)`, ended by a
      worker's `exit_group(7)`, was invisible to a plain `wait4` (`ECHILD`) and collected by
      `__WCLONE` as the leader with 7, and the parent held a pending **SIGUSR1** - not a
      `SIGCHLD`. Linux stores `-1` on a thread's own byte precisely so its death announces
      nothing; the process's byte is the one it was forked with.
    * **E. a thread's child when the whole group ends**: the child (forked by a worker) named the
      group leader while it lived, and a live task outside the group the moment the group died -
      the nearest **child subreaper** on that host; this kernel, which has none, names init. The
      pin makes the same claim for a child forked by a **sibling** of the thread that ends the
      group: that sibling never runs the exit path, so only `exit_group`'s own walk can hand the
      child on (counterfactual 4 panics without it).

    The implementation turns on one fact and reads every field from the task that owns it.
    `exit` computes `group_dead` after marking the dying task exited, and only then records and
    announces the process's death - as the **leader's** id, with the dying thread's `code`, core-
    dump classification and real uid (`si_status`/`si_uid` are `tsk`'s), the **leader's** `pgid`,
    `exit_signal` and tracer, and the group's CPU (`si_utime`/`si_stime` are
    `tsk->utime + tsk->signal->utime`). A thread that is not last records nothing and notifies
    nobody. `trace::group_has_live_task` is the client side of the same fact: `trace::is_child`
    and `child_alive_for` count an exited **leader** whose group still has a live task as a child
    a wait may park on, so `WNOHANG` answers 0 and a blocking wait sleeps until the record
    appears. And `exit_group` hands every ended sibling's **children** to init before the group's
    last task leaves: a thread ended there never runs the exit path, so its children would
    otherwise keep a link to a slot the group is about to release - and a child whose parent slot
    is gone is collected by no `wait`, because the parentage test reads through that slot.

    Pin: **`t-exit-group`'s 18 new claims** (4 -> 22) - the worker-ended group reaped as the
    leader with its code, the worker's tid `ECHILD`, the zombie leader whose `WNOHANG` is 0 and
    whose group's death answers with the last thread's code, the `clone(SIGUSR1)` group's kind
    (invisible to a plain wait, collected by `__WCLONE`) and its pending `SIGUSR1`/absent
    `SIGCHLD`, and the thread's child and the sibling's child each naming a live task outside the
    dead group - every one green on a current host first (`MPOS_ON_HOST=1`, 22 of 22, repeated).

    **`t-leader-outlived`'s premise was corrected with it, and the first gate run is why.** That
    program expected the shell to collect a compositor's pid *while its threads still ran* - the
    shape this kernel served until now. Linux does not: measured in the same probe (C/C2), the
    leader is a zombie and `waitpid(leader, WNOHANG)` answers 0 until the group's last thread
    leaves, so the old `waitpid(pid, 0)` blocks for ever. The gate hung exactly there. The test now
    pins Linux's halves - the threads running while the leader is a dead, unreapable zombie, the
    counter still moving on the space the leader owns, and the group's end collected as the leader
    with the last thread's code - and its claim count went 2 → 6.

    **Four counterfactuals, one per rule, each with every pin kept**:

    | reverted | red claims by name |
    |---|---|
    | the record's id (the dying task's, not the leader's) | `pass=186 fail=2`; **eleven** red - `t-exit-group`'s nine (the leader is not reaped, its code, the worker's tid, the zombie's answer, both last-thread codes, `__WCLONE`'s leader and code, the child fixture) and `t-leader-outlived`'s two (the group's end, its code) |
    | the zombie's admission (`is_child`/`child_alive_for` back to `!exited`) | `pass=186 fail=2`; **six** red - `t-exit-group`'s `but not reapable yet`, `the group's death then answers for it` and `with the last thread's code`, and `t-leader-outlived`'s same three |
    | the exit byte (the dying thread's, not the leader's) | `pass=187 fail=1`; **two** red - `SIGUSR1 is pending` and `not SIGCHLD` (a thread is born with `SIGCHLD` here) |
    | `exit_group`'s reparent walk removed | the sibling's-child claim reds and the machine **panics** at `src/sched/runtime/task.rs:1174` with `there is no task 9` (QEMU 35, no summary) |

    Gate: stage 1 `579 passed; 0 failed`, `xtask` 122; x86_64 `pass=188 fail=0 (188 built)`,
    `mpos-kunit 32 passed, 0 failed`, exit 33 (11 of 199 left out by `tests.toml`); aarch64
    `pass=187 fail=0`, `mpos-kunit 32 passed, 0 failed`, `=== END` (12 left out); both machines'
    per-claim lines byte-identical - `t-exit-group` 22, `t-leader-outlived` 6.

## item 68 — a framebuffer is its description's object, and it has no properties

    §7.4's "framebuffer object's own properties" was one note with two premises, and a
    current host says both are false. The card had answered `OBJ_GETPROPERTIES` for an
    fb with an empty list, because Linux was believed to publish `width`, `height`,
    `format` and `modifier` on one; the reading came from the fields of
    `struct drm_framebuffer`, which are not properties at all.

    **Measured whole on a current host first** (`.workbuddy/probe_drm_fb_owner.c`,
    `.workbuddy/probe_fb_props.c`, Linux 7.2.11):

    * **the list is the open file description's.** `GETRESOURCES`'s
      `count_fbs`/`fb_id_ptr` walked `file_priv->fbs` in Linux: a child that made fb 464
      read `1`/`464`, the parent on a second description read `0`, and after the parent
      made fb 465 its own list read `1`/`465` while the child's stayed `464`. The
      crtc/connector/encoder lists beside it are the card's (with the lease filter).
    * **an fb has no property list.** `OBJ_GETPROPERTIES` on the description's own fb,
      on another description's, and on the same id under `OBJECT_ANY` all answered
      **`EINVAL`** (errno 22). `GETFB2` is the request that reports an fb's geometry and
      format.

    The niri failure the empty list was written for was the first fact, not the second:
    it walks every id `GETRESOURCES` gives it, and the device-wide list handed it
    **kmscube's** fb 100 - another client's object, which its own list would not contain
    on Linux. Answering for it kept the walk alive over a name that should never have
    been there.

    `get_resources` filters `st.fbs` on `Framebuffer::owner` (the open file description
    `ADDFB`/`ADDFB2` already record and the release path already prunes), and the
    `(OBJECT_FB, _)` arm is gone from `obj_get_properties`: the match falls to
    `_ => None`, which is `EINVAL`. `object_kind` still resolves an fb id, so
    `OBJECT_ANY` reaches the same refusal Linux gives.

    Pin: **`t-drm`'s four new claims** - `GETRESOURCES answers the fb list`, `this
    description sees the framebuffer it made`, `and it does not see another
    description's framebuffer`, and `OBJ_GETPROPERTIES on a framebuffer is EINVAL, as
    Linux's is` - green on both machines (`ok:` lines byte-identical).

    **Two counterfactuals, one per rule, each with every other pin kept**:

    | reverted | result |
    |---|---|
    | the owner filter (the list back to `st.fbs` device-wide) | `pass=187 fail=1`; red: `and it does not see another description's framebuffer` |
    | the `(OBJECT_FB, _)` empty-list arm restored | `pass=187 fail=1`; red: `OBJ_GETPROPERTIES on a framebuffer is EINVAL, as Linux's is (got 0, want -22)` |

    Gate: x86_64 `pass=188 fail=0 (188 built)`, `mpos-kunit 32 passed, 0 failed`, exit
    33; aarch64 `pass=187 fail=0`, `mpos-kunit 32 passed, 0 failed`, `=== END`; both
    machines' `t-drm` blocks byte-identical.

## item 69 — a function's configuration space is as long as its device says

    §7.6a closed with a size rather than a smell: the ECAM window reaches 4 KiB per
    function and every reader still stopped at 256 (`crate::pci::CFG_SIZE`). The doubt
    the note recorded — that a `/sys/.../config` file's size would start depending on
    which mechanism the firmware named — was answered by the measurement: the size is
    the *device's*, and the probe that asks it uses the mechanism in use, so a
    port-only machine answers 256 for every function exactly as it should.

    **Measured whole on a current host first** (`.workbuddy/probe_pci_ext.c`, Linux
    7.2.11, as root):

    * `/sys/bus/pci/devices/*/config` is **4096** for every PCIe function and every
      host bridge (class `0x0600`), and **256** for the IOMMU, the SMBus controller
      and the LPC bridge — the three that are neither;
    * a 4096-byte file answers at 0x100, at 0xfff (1 byte), and at 0xff8 (8 of 16
      bytes); 0x1000 is EOF; a 256-byte file is EOF at 0x100;
    * Linux's rule is `pci_cfg_space_size`: only a PCIe function, a host bridge and a
      PCI-X mode 2 bridge are asked; the dword at 0x100 must not be all ones, and the
      extended space must not be the standard header repeated at every 0x100 boundary
      (`pci_ext_cfg_is_aliased`); everything else is 256 without a question.

    The same probe found a second rule, **named and not copied**: without
    `CAP_SYS_ADMIN` in the initial user namespace Linux clamps every config file to its
    first **64** bytes (`pci_read_config`). `t-pci` states it and does not claim it.

    **Served** by `crate::pci::cfg_space_size` (the rule, host-tested over synthetic
    probes including the alias), `crate::pci::find_capability` (the standard list, 48
    nodes), `Device::cfg_size` (probed once at enumeration, Linux's `dev->cfg_size`),
    the six accessors widened to twelve-bit offsets, and `config_read`/`config_write`
    taking the function's size. Both `/sys` and `/proc` nodes carry it. With no window,
    an access above 0xff answers the absent-function value rather than aliasing the low
    256 bytes.

    **Two traps the gate found, both ours.** First, the alignment masks in `read32`
    and `read16` were still `0xfc`/`0xfe` — eight-bit literals that compile after the
    offset is widened and clear bits 8..15, folding every extended read onto 0x000; the
    0x100 probe then read the vendor/device dword and the rule called it an alias. They
    are `!0x3`/`!0x1`. Second, q35's own fourteen functions are all conventional PCI
    (none carries capability id 0x10), and a bare `pcie-root-port` aliases its 0x100
    in QEMU — so the test machine now carries one `nvme,serial=...` controller (no
    `drive=`, so no namespace), whose AER capability at 0x100 is a real extended
    answer. It is added last, and an installed disk does not get it.

    Pin: **`t-pci`**, seven claims on the gate's machine — the aggregate length rule,
    a conventional file's EOF at 0x100, a PCIe function's 4096, the extended space
    being real rather than an alias, 0xfff/0x1000/0xff8, the extended capability
    chain, and `/proc` publishing the same length and bytes. On aarch64 it skips, and
    names why: that machine's config files have no read path (`mpos_pci_config` is
    x86_64's), which is a pre-existing limit.

    **Three counterfactuals, each with every other pin kept**:

    | reverted | result |
    |---|---|
    | `Device::cfg_size` back to the constant 256 | `pass=188 fail=1`; red: `15 config attributes are as long as their device says, 256 or 4096` |
    | the eight-bit masks `0xfc`/`0xfe` restored | `pass=188 fail=1`; red: `15 config attributes are as long as their device says, 256 or 4096` |
    | `/proc`'s node size back to the constant 256 | `pass=188 fail=1`; red: `/proc/bus/pci publishes the same length and the same extended bytes` |

    Gate: x86_64 `pass=189 fail=0 (189 built)`, `mpos-kunit 32 passed, 0 failed`, exit
    33; aarch64 `pass=188 fail=0`, `mpos-kunit 32 passed, 0 failed`, `=== END`; host
    tests `mpos-pci` 19 → 22; `cargo fmt --check` clean; clippy 77/82 (the standing
    baseline); `depgraph` within the ratchet.

## item 70 — the second machine's configuration space, and the skip that was a claim

    §7.6b named one skip: aarch64's `t-pci` stopped at "this machine's configuration
    space is not readable through this path" because `mpos_pci_config` was x86_64's.
    The doubt was whether the *bus* was reachable there. The boot log answered it
    before this item was written: `pci: the tree names ECAM at 0x4010000000, buses
    0..=255, 256 MiB` and `pci: ECAM is this machine's only mechanism and the walk read
    2 functions through it`. Enumeration, the `Device` model, `cfg_size` and both trees
    were already the second machine's; only the two `Inode` methods were compiled out,
    and the trait's default body answered `EOPNOTSUPP` (67, `pread64`, in the
    counterfactual's own kernel line).

    **Served** by declaring `--cfg mpos_pci_config` for `aarch64-unknown-none` in
    `.cargo/config.toml`. The mechanism was already there and stays the machine's: the
    device tree's `pci-host-ecam-generic` window on aarch64, the port pair or the
    firmware's window on x86_64, and the absent-function answer where neither serves.
    The comment that said the second machine has "no PCI on the MMIO path this port
    drives" was true before `discover_ecam_dt` and is replaced rather than kept; §15's
    capability table loses its fourth missing entry.

    **One device for the pin.** The second machine's own functions were a host bridge
    (asked by the rule, empty extended space) and a conventional `ich9-ahci`; neither
    can pin "a PCIe function publishes 4096 bytes". The test boot now attaches
    `nvme,serial=mpos-pci-ext` with no `drive=` - the same device and reason as the
    first machine's test ESP - through a `test_machine` parameter on
    `virt_machine_args`/`boot_args`; a session's boot keeps the installed machine's
    device list and does not get it, and the parameter has its own host test.

    **The skip was the wrong shape for a machine that publishes a bus.** `t-pci` used
    to return early when the first `config` file could not be read. It makes that a
    claim now - `every published function's configuration space is readable through
    this path` - and the only remaining skip is the host's `CAP_SYS_ADMIN` clamp,
    guarded by `host_run()`. A machine that offers a tree it cannot serve is red, not
    quiet.

    Pin: **`t-pci`**, eight claims on both gates - the readability claim added to item
    69's seven. aarch64's boot enumerates **3** functions and the NVMe's AER is the
    4096-byte answer; x86_64's 15 are unchanged.

    **Counterfactual, every other pin kept**:

    | reverted | result |
    |---|---|
    | `--cfg mpos_pci_config` removed from aarch64 | `pass=187 fail=1`, `=== FAIL t-pci`; red: `every published function's configuration space is readable through this path` and `3 config attributes are as long as their device says, 256 or 4096`; the kernel's own line is `mpos: syscall 67 (fd 3) -> EOPNOTSUPP` |

    Gate: aarch64 `pass=188 fail=0`, `mpos-kunit 32 passed, 0 failed`, `=== END`;
    x86_64 `pass=189 fail=0 (189 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33;
    host tests 579, `mpos-xtask` 123; `cargo fmt --check` clean; clippy 77/80 (aarch64
    two below §7.6b's 82 because the config pair became live code); `depgraph` within
    the ratchet.

## item 71 — a device node made by `mknod` carries the number `mknod` was given

    §5.2's entry said this kernel ignores `mknod`'s `dev` argument, so a device node
    made here reports `st_rdev` `0:0` whatever number the caller named, and charged
    closing it to a prerequisite it does not need: a number-to-driver registry, "the
    same prerequisite `devfs`'s birth modes want". The number can be kept without a
    registry. What a registry would buy is a *driver behind* the number, and that
    absence already has an answer Linux wrote: `ENXIO`, for a device nobody owns.

    **Measured whole on a current host first** (`.workbuddy/probe_mknod_rdev.c`, Linux
    7.2.11, every row read through `stat` and again through `statx`, on a tmpfs and on
    an ext4 - which agree, and separate the answer from one filesystem's `mknod`
    method):

    | asked of Linux | the answer |
    |---|---|
    | `mknod(p, S_IFCHR\|0666, makedev(1, 3))` | `st_rdev` `0x00000103`, decoded back to `1:3` |
    | `makedev(136, 300)` — a minor above eight bits | `0x0010882c`, decoded back to `136:300` |
    | `makedev(226, 128)` — virtio-gpu's render node | `0x0000e280` |
    | `makedev(4095, 1048575)` — the whole field | `0xffffffff`, decoded back unchanged |
    | a **block** node, `makedev(254, 70000)` | `0x1110fe70`, and `stat` calls it a block node |
    | a `dev` wider than 32 bits (`0x10300000103`) | `1:3` — only the low word is the number |
    | `0666` under `umask 027` | mode `0640`, the type field untouched |
    | `statx` beside `stat` on the same node | `stx_rdev_major` 136, `stx_rdev_minor` 300, mode `0644` |

    **Three things were wrong here, and the middle one is not about the number.**
    `write_stat` packed a pair as `(major << 8) | (minor & 0xff)`, so a node asked for
    `136:300` would have reported `136:44` — *another* device's number, as every pts
    above 255 and every wide minor is; `RamDev` reported `NodeKind::CharDev` whatever
    kind `mknod` was handed, so a name made from `S_IFBLK` answered `stat` about a
    device nobody asked for - the kind, and not the number, being the half a caller
    reads first; and it opened, read end-of-file and swallowed writes, which is what
    `/dev/null` does exactly - a caller could not tell the device that is not there
    from the one that is.

    **Served** by carrying the pair through the one door that can make such a node.
    `Inode::create` takes it beside the kind, because a device node reached through any
    other door is a name answering `stat` about a device nobody made; both `mknod` doors
    unpack the argument's low 32 bits with `abi::new_decode_dev` — Linux's own inverse,
    whose four rows ride in that function's unit test next to the six above — and
    `abi::new_encode_dev` is what `st_rdev` is written with now, so `struct stat` and
    `statx`'s two unencoded halves cannot disagree. `RamDev` keeps the pair, the kind,
    the caller's mode and its ids, and answers `VfsError::NoDevice` — Linux's `ENXIO`,
    and therefore the answer for **every** number this kernel can be given, `0:0`'s
    whiteout included, since no driver stands behind any of them. Three filesystems
    that cannot keep a number (`ext4`, `9p`, `devfs`) refuse a device node rather than
    inventing one, which is what they already did for a kind they cannot publish.

    **Two answers stay named rather than claimed in the pin**, because each belongs to
    the machine reading them and not to the rule: **what `open(2)` of such a node
    answers** — on a `nodev` mount Linux answers `EACCES` for every device node before
    it consults the number (this host's `/tmp` is one), while off one the number is what
    answers: `makedev(1, 3)` opens the **real** `/dev/null`, `136:300` answers `EIO`
    from a pts driver with no line behind it, `4095:4095` answers `ENXIO`, and `O_PATH`
    succeeds in every case, being the one door that asks no driver for anything — and
    **whether the number reaches a driver at all**. It still does not. What the device
    would need is §5.2's registry, and this kernel's devices remain objects a driver
    published under a name; a node made by `mknod` is a name with a number now, where it
    used to be a name pretending to be the one device that is.

    **Pin**: `t-mknod`'s five new claims, made by a caller holding `CAP_MKNOD` — the
    node carries the number it was made with (`1:3` is `0x103`), a minor that does not
    fit in eight bits survives (`136:300` is `0x0010882c`), a block node keeps a
    twenty-bit minor (`254:70000` is `0x1110fe70`) *and* is called a block device, and
    the whiteout's zero is `0:0` through the same field — beside two host unit tests in
    `vfs::ramfs` (`a_device_node_keeps_the_number_its_kind_and_the_callers_mode`,
    `a_device_node_no_driver_answers_for_refuses_every_file_door`) which pin the pair,
    the two kinds and the `ENXIO` at the layer that makes them. Every row was green on
    this host's Linux first — the whole program run as root through `syscall(2)`, which
    is also why no claim here is about the inode when the host does not write it.

    **The depgraph ratchet caught one thing worth keeping**: the first draft of those
    unit tests encoded the pair with `abi::new_encode_dev` to prove the word matches,
    and `xtask`'s Phase-11 ratchet refused it — `vfs names crate::syscall:: 2 times, up
    from the recorded 0`, and a ninth mutual pair. The edge was real and the ratchet was
    right: the encoding is a question about the ABI and `abi`'s own tests answer it, so
    the filesystem's claim stops at the pair it keeps.

    **Counterfactual, every other pin kept**:

    | reverted | result |
    |---|---|
    | the two `mknod` doors' argument, replaced by a literal `0` (`src/syscall/linux/fs/file.rs`; `RamDev` untouched) | `pass=188 fail=1 (189 built)`, and the one red program is **`t-mknod`** - reproducibly, two runs - while every control passes on both kernels and the host unit tests stay 582 green, because the node still holds its pair and it is the door that stopped handing it one. Putting the argument back gives `pass=189 fail=0 (189 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33, command exit 0 |

    Gate: x86_64 `pass=189 fail=0 (189 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (11 of 200 left out by `tests.toml`); aarch64 `pass=188 fail=0`, `mpos-kunit 32 passed, 0 failed`, reaching `=== END` (12 left out); host unit tests 582 passed (up 3), `mpos-xtask` 123; `cargo fmt --check` clean; clippy's warning set unmoved by this change on both bare-metal targets; `cargo xtask depgraph` back within the ratchet

## item 72 — `MSG_PEEK` looks without taking, and `MSG_TRUNC` names the message's own length

    The last flag-word item on the IP receive path, and the one §5.3 had parked behind a
    prerequisite that turned out never to exist: the stack has had `tcp::Socket::peek` and
    `udp::Socket::peek` all along, and what was missing was the *reading* of the two flags.
    Both doors refused `MSG_PEEK` with `EOPNOTSUPP` - the honest answer while a peek could
    not be preserved - and `t-sock-flags` pinned that refusal. The contract below was
    measured whole on a current host first (`.workbuddy/probe_msg_peek{,2,3,4}.c`, Linux
    7.2.x), one scenario per answer, and each of the four probes corrects the one before it:

    * **a datagram peek does not consume.** Peeking 16 bytes of a six-byte datagram answers
      6 and a following peek answers 6 again; a three-byte peek copies three and a full peek
      afterwards still reads all six - the *short* peek does not cut the message. `MSG_TRUNC`
      on the same call answers the datagram's **own** length (6) and still copies only what
      fitted (3). A zero-length peek answers 0, with `MSG_TRUNC` answers 6, and takes
      nothing either way. A **non-peek** zero-length receive is the opposite: it answers 0
      and **consumes** the datagram, which the queue's own `FIONREAD` shows.
    * **a short non-peek datagram is Linux's truncation, not `EMSGSIZE`.** `recv(3)` on a
      six-byte datagram answers 3, copies the prefix and drops the rest; `recv(3)|MSG_TRUNC`
      answers 6 and copies 3; `recvmsg`'s `msg_flags` carries `MSG_TRUNC` in both cases
      (even without the input flag), because the count alone cannot tell "exactly filled"
      from "cut off". This kernel answered `EMSGSIZE` and dropped the datagram - smoltcp's
      `recv_slice` truncates by refusing - which is the same call's other half and is fixed
      by the same copy.
    * **a stream's `MSG_TRUNC` is a different rule, and the probe had to be believed over
      the man page.** A stream peek of three bytes of ten copies `012`; the same call with
      `MSG_TRUNC` answers 3 and copies **nothing**; a hundred-byte one answers 10 and copies
      nothing (measured twice, with a seeded buffer); and a *non-peek* `recv(3)|MSG_TRUNC`
      copies none of them but **consumes** the three it counted (`peek 100` afterwards
      answers the remaining seven). `recvmsg`'s `msg_flags` stay clear for a stream: there
      is no message boundary to cut.
    * **a zero-length stream receive is Linux's readiness question with a zero-byte
      answer.** With data queued it answers 0 at once and takes nothing; with nothing queued
      it waits, and a signal that ends that wait answers **0** - not `EINTR` (measured with
      a non-restarting `SIGALRM`; `recv(fd, buf, 1, MSG_PEEK)` in the same state answers
      `EINTR`). At end of stream it answers 0, and the wait is over.
    * **the empty queue and the wait are the ones every other door gives**: `EAGAIN` under
      `MSG_DONTWAIT`, a park without it, `EINTR` from a catchable signal - except in the
      zero-length stream shape above. A listener answers `ENOTCONN` to both doors, peek or
      not.

    **The fix is two lengths and the flag read in one place.** `RecvAnswer { copied, full,
    endpoint }` keeps what went into the caller's buffer apart from the message's own
    length, which is exactly the pair `MSG_TRUNC` and `msg_flags` need; `recv_common` takes
    `peek` and `truncate` beside `nonblock` and passes them down, and the raw family's
    `EOPNOTSUPP` moves inside it (asked before the dispatch, so a raw peek still answers
    the refusal it always did while a stream or datagram one is served). `tcp_recv` reads
    with `peek` - not `peek_slice`, which skips smoltcp's own receive-state check and would
    read a reset as an empty buffer - copies unless `truncate`, and dequeues the count
    unless `peek`; a zero-length request asks `can_recv`/`peek(1)` instead of the empty
    slice, which is what separates "0 because data is there" from "0 because the stream
    ended" from "wait", and the wait's two endings both answer 0. `udp_recv` peeks (or
    receives, for the consuming shape), copies `min(buffer, payload)` by hand and lets the
    caller's door decide whether to return `full` - which is also what removes the old
    `EMSGSIZE`. `recvfrom` returns `full` when the caller asked for `MSG_TRUNC` and
    `copied` otherwise; `recvmsg` does the same and writes `MSG_TRUNC` into `msg_flags`
    whenever `full > copied`. The source name is written on a peek like any other answer.

    **The pin's first guest run found two defects, and both were worth the run.**
    First, `Finished` was folded into the "nothing yet" arm: the stream path turned
    smoltcp's end-of-stream into a zero-length chunk and then into `Wait`, so a *peek* at EOF
    parked - and, because every stream read now goes through that same copy, so did an
    ordinary blocking `recv`. `t-msg-peek`'s own EOF claim went red, and the sweep then hung
    in `t-tcp-listen`, whose "the server reads end-of-stream" is exactly that blocking read.
    The two answers are an `Option` now: `None` is the report (`0`, the stream is over) and
    `Some(0)` is the wait. Second, the source-name claim compared the sender
    `recvfrom`/`recvmsg` named against `getsockname`'s answer for the sending socket, and the
    guest read `0.0.0.0:0`: the implicit bind on the first *send* (and on the first
    *receive*) set the `bound` flag and never wrote the `local` record `getsockname` answers
    from, so a socket whose datagrams carried a real port reported none. Both implicit-bind
    sites record the endpoint now, and that claim is the regression pin - Linux's own
    answer, so the host run had passed it all along.

    **And the aarch64 arm found a third thing that was not the kernel's.** `t-ns-net`'s
    descriptor handover connected to the child's pathname socket *before* waiting for the
    child's `ready` line - the wait sat inside the same block, after the connect - so the
    rendezvous was really a race against the child's scheduling. It had always been won;
    a changed socket timing lost it on aarch64, which failed the handover and parked the
    child in `accept` (its own claim then hung the sweep). The wait now comes first, which
    is what the rest of that block already assumed.

    Pin: **`t-msg-peek`**, a new program, **47 claims**, green on a current host first
    (`MPOS_ON_HOST=1`, 47 of 47) and built from the measured table rather than from the man
    page - the two `MSG_TRUNC` readings (a datagram's length versus a stream's no-copy
    count) are the ones a single sentence would have got wrong. `t-sock-flags` is
    rewritten around what is still its subject: its two peek-refusal claims are the one
    thing a served call makes false, so the file now pins the composition instead - a peek
    on an empty queue with `MSG_DONTWAIT` is `EAGAIN`, with `MSG_TRUNC` beside it too, and
    `MSG_DONTWAIT` alone is `EAGAIN` - 4 claims, green on the host. The old refusal was a
    true statement about the kernel it was written against; the pin moved with the answer
    rather than being deleted.

    **Three shapes stay named rather than claimed.** A **raw** socket's peek: the host this
    contract was measured on holds no `CAP_NET_RAW`, so the probe could not answer for that
    family and the honest `EOPNOTSUPP` stays (named in §5.3). The other families' peeks
    (`AF_UNIX` stream and datagram, NETLINK, PACKET, vsock) are untouched by this pass and
    keep their refusals. And `recvmsg` with a **zero-length data room** still answers
    `EINVAL` - a divergence older than this item, whose Linux answer is a consume (or, on a
    stream, a 0), and which belongs to the message-door item rather than to the flag's.

    Gate: x86_64 `pass=190 fail=0 (190 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33
    (11 of 201 left out by `tests.toml`); aarch64 `pass=189 fail=0`, `mpos-kunit 32 passed, 0
    failed`, reaching `=== END` (12 left out), and both programs' `ok:` lines are byte-identical
    across the two machines. Host unit suite 582 passed, `xtask` 123; `cargo fmt --check` clean on
    the touched files; clippy's warning set is the committed tree's, unmoved. The counterfactual is
    the flag itself, one line and every pin kept: with `recv_common` put back to refusing **every**
    peek (`if peek { return Err(err::EOPNOTSUPP) }` where the raw-only test now stands) the sweep is
    `pass=188 fail=2 (190 built)`, and the two red programs are exactly `t-msg-peek` (its first
    `expect` panics on the refusal, exit 134) and `t-sock-flags` (its two EAGAIN claims), every
    other program green on both kernels.

## item 73 - a `recvmsg`'s data room is the whole chain, and a stream's fault is not a datagram's

    Every receive door but one takes a single buffer, so the message door was the only place a
    *chain* of iovecs could appear - and it read the first non-empty segment and stopped there
    (`first_iovec`, beside this code since the door was written). A caller with a `4+4+2` chain
    got four bytes of a ten-byte message, answered 4, and never learned the rest was gone; and a
    zero-length data room was `EINVAL` for every family, which item 72 named as that item's own
    and left here. Both are served now, and the contract was measured whole on a current host
    first (`.workbuddy/probe_recvmsg_iov.c`, Linux 7.2.x), one scenario per answer:

    | room | datagram (`UDP`, `AF_UNIX`) | stream (`TCP`, `AF_UNIX`) |
    |---|---|---|
    | `12` into `5+5+5` | 12, `msg_flags` clear, every segment filled | 12, flags clear |
    | `12` into `4+4+2` | 10, prefix scattered, **`MSG_TRUNC` set without being asked** | 10, flags clear, the other 2 stay queued |
    | the same, `MSG_TRUNC` asked for | **12** returned, copy still 10 | 10 returned, copies **none**, consumes the 10 it counted |
    | `12` into `0+0`, and no iovec at all | 0, `MSG_TRUNC`, **consumes** | 0, takes nothing |
    | one segment, zero length, `MSG_TRUNC` | the datagram's own length | - |
    | `4+unwritable` | `EFAULT`, **and the datagram is already gone** | `EFAULT`, **bytes still queued** |
    | iovec array unreadable | `EFAULT` | `EFAULT` |
    | `1025` iovecs | `EMSGSIZE` | `EMSGSIZE` (`sendmsg` too) |
    | `MSG_PEEK` into `5+5` | 10, `MSG_TRUNC`, message whole | 10, message whole |

    Three of those rows are the ones a single sentence would have got wrong. The datagram's
    `MSG_TRUNC` is a *report* - the flag in `msg_flags` is set whether or not the caller asked,
    exactly as `recvfrom` already did after item 72 - while the return changes only when the caller
    asks. The stream's `MSG_TRUNC` is the opposite of a truncation: it counts the bytes it would
    have copied and copies none of them, so the caller's segments keep every byte they came with.
    And the two families answer the *fault* differently because they hold the message differently:
    a datagram has already left the queue by the time its segments are written, so a bad segment
    loses it (measured: `FIONREAD` 0 afterwards, the next receive `EAGAIN`), while a stream's
    bytes are still in its buffer, so a bad segment leaves them (measured: the next receive still
    reads all 12). `AF_UNIX` follows the same split - its datagram is taken, its stream is not.

    **The fix is one object and one extra length.** `DataRoom` reads the iovec *array* once (an
    unreadable array is `EFAULT`, and one longer than `IOV_MAX` - Linux's `UIO_MAXIOV`, 1024 - is
    `EMSGSIZE` before anything else is looked at), then `validate`s the segments where the
    family's rule says the check goes, `receive`s into the caller's own segment when the chain has
    exactly one non-empty one (every `read`/`recv`/`recvfrom` and the ordinary `recvmsg`, so no
    copy is added to the hot path) and into a scratch when it has more, and `scatter`s the answer
    across the chain once the count is known. `RecvAnswer` grew a third length: `copied` is what
    the call counts, `full` is the message's own length, and `written` is what actually reached
    the caller's buffer - 0 in the one shape whose count is not a copy. `sendmsg`'s gather moved
    to the same cap and the same errno. The unix channel's zero-length receive is the readiness
    question item 72 measured on `AF_INET`, answered on `AF_UNIX` with `EINTR` instead of 0, and
    it now parks on **either** readiness - a byte or a descriptor handover - because a
    control-only reader whose peer sent an fd and no bytes would otherwise wait for ever.

    **The pin's first guest run found two things, and one of them was the test's.**
    `msg_name`'s sender claim read the client's port before the first send and the guest answered
    `0.0.0.0:0`: a Linux `connect` on a `SOCK_DGRAM` socket binds it, and this kernel's does not -
    the port appears on the first send's implicit bind, which item 72 had just taught to record
    the endpoint. The claim now reads the port after the send, which is the same fix `t-msg-peek`
    already carried for the same reason (and the read is what found it: the host run passed
    either way, because on Linux there is a port to read). The other was real: a zero-length
    `AF_UNIX` receive **with** room for control data did not park here (`wants_control` skipped
    the wait so a queued handover could be picked up), while the probe's host parks exactly as it
    does without one, and the queued-handover case is already answered by `empty_handover` and by
    the readiness predicate now.

    **Pin**: `t-recvmsg-iov`, a new program, **56 claims**, green on a current host first
    (`MPOS_ON_HOST=1`, 56 of 56) and `ok:` for `ok:` identical on both machines. The calls go
    through the raw `recvmsg` rather than a wrapper, because two of the claims are about pointers
    no wrapper will build: a chain whose later segment points into a `PROT_NONE` page, and a chain
    *array* that is itself unreadable.

    **Counterfactual, every other pin kept** - two lines that make `DataRoom` keep only the first
    non-empty segment, which is exactly what the door did before:

    | reverted | result |
    |---|---|
    | `parse` keeps the **first** non-empty segment as the whole room (`if single.is_none()` + `if non_empty >= 1`) | `pass=190 fail=1 (191 built)`, and the one red program is **`t-recvmsg-iov`** with **21 of its 56 claims** red - every chain claim and the peek through a chain among them, while its single-segment, zero-length and `EMSGSIZE` claims stay green. `mpos-kunit 32 passed, 0 failed`. Restoring the two lines gives `pass=191 fail=0`, exit 33 |

    **Two shapes stay named rather than claimed.** A **raw** socket's zero-length receive keeps
    the `EINVAL` it has always given - the host this contract was measured on holds no
    `CAP_NET_RAW`, so the probe could not answer for it (§5.2 says so beside the raw peek) - and
    the **vsock** branch still reads its chain's first segment only, because nothing here could
    measure that family either; both are named in §5.2 rather than guessed.

    Gate: x86_64 `pass=191 fail=0 (191 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33 (11 of
    202 left out by `tests.toml`); aarch64 `pass=190 fail=0`, `mpos-kunit 32 passed, 0 failed`,
    reaching `=== END` (12 left out). Host unit suite 582 passed, `xtask` 123; `cargo fmt --check`
    clean on the touched files; clippy's warning set is one *below* the committed tree's - the new scratch's zero-fill is `alloc::vec!` rather than a `Vec::new` + `resize` - and nothing new appears.

## item 74 - a raw socket's receive is a datagram's, and its hop count is read off the packet

    Item 73 left this family's two receive shapes named rather than claimed, and both were the
    *host's* fault rather than the kernel's: the machine that contract was measured on holds no
    `CAP_NET_RAW`, so no probe could open the door at all. This item makes the capability instead
    of waiting for it - `unshare(CLONE_NEWUSER|CLONE_NEWNET)` hands the caller the whole set
    inside a user namespace of its own, and a network namespace whose owner is that user
    namespace is one `ns_capable(CAP_NET_RAW)` answers for (item 43's rule, from the other side).
    The probe (`.workbuddy/probe_raw_recvmsg.c`, Linux 7.2.11) brings `lo` up there and sends
    itself ICMP echo *replies* - type 0, the one the kernel's own ICMP input does not answer, so
    one send is exactly one packet on the queue - and the answers are a datagram family's, every
    one of them the ones `t-recvmsg-iov` had just pinned for UDP:

    | room | answer |
    |---|---|
    | zero-length, packet queued | 0, `MSG_TRUNC`, **consumed** |
    | zero-length, `MSG_DONTWAIT`, empty | `EAGAIN` |
    | zero-length, blocking, empty | parks; a signal ends it `EINTR` |
    | no iovec at all (`msg_iovlen = 0`) | 0, consumed |
    | 4 bytes of a 36-byte packet | 4, `MSG_TRUNC` **without being asked** |
    | the same, `MSG_TRUNC` asked | **36** back, copy still 4 |
    | `MSG_PEEK`, full segment | 36, packet still queued; again 36 |
    | `MSG_PEEK`, zero-length room | 0, `MSG_TRUNC`, packet still queued |
    | zero-length + `MSG_TRUNC` | 36, consumed |
    | `MSG_PEEK`+`MSG_TRUNC`, zero-length | 36, still queued |
    | chain 4+4+28 | 36, scattered segment by segment |
    | chain 4 + `PROT_NONE` | `EFAULT`, **and the packet is gone** |
    | 1025 segments | `EMSGSIZE`, packet survives, still queued |
    | `msg_name` | the header's sender, `127.0.0.1:0`, namelen 16 |

    The first row is one of the two shapes item 73 named (the `EINVAL` a raw zero-length room had
    always given is gone) and the seventh is the other (the `EOPNOTSUPP` its `MSG_PEEK` had
    always given). The fifth and sixth close the sub-gap §4.5's door named: a raw socket carries
    `MSG_TRUNC` now - set whether or not the caller asked, and only the caller that asked gets
    the packet's own length back.

    **The hop count is read off the packet, not off the copy.** `IP_RECVTTL` attaches even when
    the caller's buffer cut the IP header short (a 4-byte segment against a 36-byte packet
    answered `controllen=24 cmsg_len=20 level=0 type=2 ttl=64`), so the record cannot be read
    back out of what was copied. `RecvAnswer` grew a `ttl` the family fills from the packet's own
    header, and the gate that decides whether the record rides along is the **packet's** length
    rather than the copy's - gating on the copy answered `controllen 0` for exactly the
    short-buffer shape the option is most useful in. The same measurement corrected two words
    this kernel had been writing: `cmsg_len` is the record's own unpadded `CMSG_LEN(4)` (20), not
    the space it occupies (`CMSG_SPACE(4)`, 24 - what `msg_controllen` reports), and the
    record's `cmsg_type` is `IP_TTL` (2), Linux's `ip_cmsg_recv_ttl` word, not the option number
    `IP_RECVTTL` (12) that asks for it. `t-rawicmp`'s control-data claim had read 24 and
    `IP_RECVTTL`; both are the host's own answers, and it reads 20 and `IP_TTL` now.

    **The landing** is one answer shape and four call sites: `raw_recv` and `raw_wildcard_recv`
    hand back a whole `RecvAnswer` (the copy is what the caller's room holds, the length is the
    packet's own, the peek looks without taking - the wildcard queue's copy and its pop now
    happen under the one lock, so a second reader on the same descriptor cannot pop the packet
    this one copied), the raw arms of `handle_recvmsg` and `recv_common` no longer refuse, and
    the `IP_RECVTTL` writer takes its length from the answer.

    **Pin**: `t-raw-recvmsg`, a new program, **50 claims** (the 51st `r.check` is the
    socket-open failure branch a green run never reaches), green on a current host first
    (`MPOS_ON_HOST=1`, 50 of 50 - the host run makes its own door exactly as the probe does) and
    `ok:` for `ok:` identical on both machines. The guest's fabric is slirp's (`eth0`, never the
    loopback), so every shape starts from one packet queued from beyond slirp, and the run
    measures the reply's own length rather than hardcoding it.

    **The pin's first guest run found that the internet is not a fixture.** One gate run lost the
    third echo beyond slirp; the receive that should have answered 0 parked instead, the guard
    expired, and a claim about the *door's* answer went red on a packet that never came.
    `queue_one` now drains the queue, sends, and confirms with a **peek** - a look leaves the
    packet where the claim expects to find it - three attempts at most, past which the claim
    that asked fails loudly rather than quietly measuring nothing. Those claims are named "an
    echo is queued" now, because that is what the helper promises.

    **Counterfactual, every other pin kept** - the two refusals restored, the way the kernel
    answered before this item:

    | reverted | result |
    |---|---|
    | the two refusals put back (`room.is_empty() && typ == SOCK_RAW` answers `EINVAL` again, and a raw `MSG_PEEK` answers `EOPNOTSUPP` again) | `pass=197 fail=1 (198 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33, and the one red program is **`t-raw-recvmsg`** with **29 of its 50 claims** red - all thirteen "an echo is queued" (the helper's confirmation is itself a peek, which the refusal answers first), every zero-length-room claim (`answers 0`, `MSG_TRUNC`, the empty-queue `EAGAIN`, the park's `EINTR`, `no iovec at all`, the `MSG_TRUNC` return) and every peek claim, while its single-segment truncation, chain, `EFAULT`, `EMSGSIZE` and `IP_RECVTTL` claims stay green and **`t-rawicmp` stays green** - the cmsg correction is the other half of this item and the counterfactual does not touch it |

    Gate: x86_64 `pass=198 fail=0` (198 staged, 5 left out - the payload rides an initramfs
    here), `mpos-kunit 32 passed, 0 failed`, exit 0; aarch64 `pass=197 fail=0` (197 staged, 6
    left out - the payload rides the sweep disk here, and the sixth left out is the machine's own
    gap), `mpos-kunit 32 passed, 0 failed`, reaching `=== END`. Both machines' `ok:` lines
    byte-identical (50/50, diff empty), and `t-rawicmp` passes on both with its corrected
    control-data words. Host unit suite 582 passed, `xtask` 123; `cargo fmt --check` clean on the
    touched files; clippy's warning set unchanged; the depgraph ratchet holds. The **vsock**
    branch still reads its chain's first segment only - nothing here could measure that family
    either - so it stays named in §5.2.

## item 75 — the x86_64 signal frame carries the interrupted floating-point image, and a null one resets it

    The first machine's half of the record the signal frame owes a handler: an x86_64
    `rt_sigframe` was 440 bytes, its `sigcontext.fpstate` was left **null**, and `rt_sigreturn`
    restored only the general registers — so a handler that touched an `xmm` register (which any
    ordinary function may: all sixteen are caller-saved) returned the interrupted code with the
    handler's values. That is not an absent feature a caller can fall back from but a frame that
    names the interrupted state and does not carry it; the second machine has written and
    restored its `fpsimd_context` since §13.2's Stage 3b.

    **Measured whole on a current host first** (`.workbuddy/probe_signal_fp.c`,
    `probe_signal_fp2.c`, Linux 7.2.11): `offsetof(struct ucontext, uc_mcontext)` is 40 and
    `uc_mcontext.fpregs` is 224 inside the `ucontext` (184 inside the `sigcontext`), so a handler
    reads the pointer at `ucontext + 224`; on a real frame it is non-null. The 512 bytes it names
    **are an `fxsave` image**: compared byte for byte against an `fxsave` issued immediately
    before the trapped `tgkill`, the two differ in ten bytes, all at offset 464 —
    `_fpx_sw_bytes`, the XSAVE extension-record header, which a full Linux fills and this machine
    has no extended state to describe. The legacy area is read regardless: a handler that clears
    the whole 96-byte padding region (where those ten bytes live) and returns still gets its
    `xmm` registers back, so a kernel without XSAVE may leave the region zero. The third run
    (`NULL_FPREGS=1` / `NO_RESTORE=1`) is the one that shapes the design: a frame whose `fpregs`
    is null is **accepted**, and `rt_sigreturn` answers it by **resetting** the floating-point
    state — `xmm8..xmm11` come back zero and `mxcsr` is `0x1f80`, the architectural init value —
    rather than leaving the handler's registers live. An all-zero image is not that state,
    because `mxcsr` zero unmasks every SSE exception.

    **The landing** keeps the image as the machine's `fxsave` block: `SIGFRAME_BYTES` 440 → 952,
    with `_fpstate` at frame offset 440 (immediately after the 128-byte `siginfo_t`) and
    `sigcontext.fpstate` at `mcontext + 184` pointing at it. The frame's base is 8 mod 16 and
    440 is 8 mod 16, so the image lands 16-aligned — what `fxrstor` requires (Linux aligns its
    area to 64 because it is an XSAVE area; there is no XSAVE state here to align for).
    `hal::SignalDelivery` grew `fpu: Option<[u8; FPU_BYTES]>` — filled under
    `#[cfg(mpos_fpu_trap)]` from `sched::trace_fpu(current)`, the lazy scheme's "make the record
    current" call, and `None` on the machine whose trap frame already holds the registers
    (aarch64) — and `hal::SignalFrame::exit` now answers `SignalFrameExit { blocked_wire, fpu }`
    instead of the mask alone. `proc.rs` hands the image back through `sched::set_signal_fpu`,
    which reloads the live registers when `fpu_live` says they are authoritative; a frame whose
    pointer is zero is answered with `FpuState::reset().to_bytes()` rather than with zeros, the
    same definition `Context::empty` already gives a task that has never executed a
    floating-point instruction.

    **Pin**: `t-signal-frame`, 17 → 24 claims on x86_64 (aarch64's 21 unchanged, but its
    "registers came back" claim is a counterfactual now too, because the handler destroys every
    vector register after reading the frame): the handler fills all sixteen xmm registers with
    ones after reading the frame, so they can only return if the frame carried them. The x86_64 half
    reads `fpstate` at `uc_mcontext + 184`, checks it is non-null and 16-byte aligned, and walks
    the sixteen `xmm_space` slots at offset 160 for the stamp; a third delivery nulls the word
    in the handler and checks that the vectors come back **cleared** while the integer file is
    intact. Green on a current host first (23 `ok:`; the one red is the pre-existing
    `raise(SIGSEGV)` artifact of a Rust `std` host child, where the C probe dies of signal 11 as
    expected).

    **Counterfactual** — the six kernel files reverted to what they were, the pin kept: x86_64
    `pass=190 fail=1 (191 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33, and the one red
    program is `t-signal-frame` with exactly five claims red — both "stamped registers came back"
    claims (`xmm0..xmm15` all `0xffffffffffffffff`, the clobber), "the frame names a
    floating-point image (fpstate 0x0)", "its `xmm_space` holds the interrupted xmm registers (0
    did)", and "the frame's missing image resets the vector registers" — while "and it is
    16-byte aligned" passes **vacuously** on the null pointer (`0 & 15 == 0`), which is why the
    non-null claim sits beside it.

    **Gate**: x86_64 `pass=191 fail=0 (191 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33;
    aarch64 `pass=190 fail=0` (203 built, 13 left out — the second machine's `gaps` group holds
    the one the first machine serves), `mpos-kunit 32 passed, 0 failed`, reaching `=== END`;
    both machines' `ok:` lines byte-identical for the claims they share. Host unit suite 582,
    `xtask` 123; `cargo fmt --check` and clippy clean on the touched files; the depgraph ratchet
    holds — the new call is `syscall → sched`, an edge that already exists, and no lower layer
    reaches into `syscall`.

## item 76 — AF_UNIX's receive doors look without taking, and a datagram's `MSG_TRUNC` is its own length

    The last two flag words on this family's receive path, and both used to be refused
    `EOPNOTSUPP` on every unix door: `recvfrom`/`recv` and `recvmsg`, stream, datagram and
    sequenced-packet alike. The refusal was honest while a pipe could not look at its front
    without taking it, but the flags are what a large part of userland composes with - a
    supervisor that wants to see whether a whole message has arrived peeks first, and a
    caller that wants the message's length without room for it asks for `MSG_TRUNC`.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_peek{,2,3,4,5,6,7}.c`,
    Linux 7.2.x), and two of the shapes are not what the flags suggest:

    * **a stream peek copies and takes nothing.** A 4-byte look at ten queued bytes answers 4,
      fills the caller's buffer with `abcd`, and the next read still returns all ten; the look
      is repeatable, a zero-length look answers 0 at once, and a look at end-of-stream answers
      0. A four-byte look at a ten-byte stream with `MSG_TRUNC` is **the same call**: it answers
      4, copies the four bytes and leaves all ten.
    * **a stream's `MSG_TRUNC` is not a datagram's.** A four-byte read answers 4, copies the
      four and consumes them - indistinguishable from the read without the flag. (This
      kernel's *TCP* receive is the count-only rule; the families differ, which is why the pin
      lives here.)
    * **a datagram peek is a full copy of the queue's front.** A short look answers 4 and
      leaves the message whole; with `MSG_TRUNC` the *return* is the message's own length (10)
      while the copy stays 4; `recvmsg` sets `MSG_TRUNC` in `msg_flags` whether or not the
      caller asked. A zero-length peek answers 0 without consuming, a zero-length **non-peek**
      receive consumes, and a `recv`/`recvfrom`/`recvmsg` with `MSG_TRUNC` answers the
      message's length while consuming it.
    * **`SCM_RIGHTS` survives a peek.** A peek that has control room installs the descriptors
      the front send carried and leaves the record queued, so the read that consumes its bytes
      installs a second set - both live. A *zero-byte* handover is not readable at all: `recv`
      and `recvfrom` answer `EAGAIN` until a byte arrives (`probe_unix_peek6.c`).
    * **a datagram peek names its sender**, exactly as the consuming receive does.

    **The landing** is three pieces. `PipeEnd::peek_once` copies the ring's front without
    moving the cursor, and `PipeEnd::peek_scm` clones the front handover rather than taking it
    (so the peek's copies share the queued files' descriptions, Linux's `get_file` shape);
    `DuplexEnd` forwards both. `UnixSock::peek_dgram` clones the front `Dgram` and leaves the
    queue alone, and both `take_dgram` and `peek_dgram` answer a `RecvDgram` carrying the
    message's **whole length**, which is what `MSG_TRUNC` reports. Both receive doors route
    `MSG_PEEK` to the look and `MSG_TRUNC` to the datagram return; the stream arms deliberately
    ignore `MSG_TRUNC`, because on this family it changes nothing. A channel peek parks on the
    same readiness a read does (`poll_ready`, the hangupless "bytes or EOF" question) and
    reports `EINTR` when a signal ends the wait, but a queued zero-byte handover is **not**
    readiness here: Linux answers `EAGAIN` and only `recvmsg` with control room is the door
    that receives it, which is what `probe_unix_peek6.c` measured.

    **Pin**: **`t-unix-peek`**, a new program, **63 claims** - stream, datagram and seqpacket
    peeks, both `MSG_TRUNC` shapes on each, `recvmsg`'s `msg_flags`, the sender name through an
    abstract address, and the handover that a peek installs twice - green on a current host
    first (63 of 63) and all 63 green on both machines. Beside it, two host unit tests
    in `vfs::pipe`: a peek is repeatable and leaves the read's bytes, and a peeked handover is
    delivered again to the read.

    **Counterfactual** - the four peek refusals restored (the way the kernel answered before
    this item), everything else kept: x86_64 `pass=191 fail=1 (192 built)`,
    `mpos-kunit 32 passed, 0 failed`, exit 33, and the one red program is **`t-unix-peek`**
    with **18 of its 63 claims** red - every peek claim, the `MSG_TRUNC` peek returns included
    - while the ordinary reads, the zero-length consuming receive and the sender-name half
    stay green.

    Gate: x86_64 `pass=192 fail=0 (192 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33;
    aarch64 `pass=191 fail=0` (13 left out), `mpos-kunit 32 passed, 0 failed`, reaching
    `=== END`. Host unit suite 584 (was 582), `xtask` 123; `cargo fmt --check` and clippy
    clean on the touched files; the depgraph ratchet holds - the change adds no edge.
    Named rather than served: the **other families'** peeks (`NETLINK`, `AF_PACKET`, vsock) are
    still refused; the vsock `recvmsg` still reads its chain's first segment; and a **zero-byte
    `SCM_RIGHTS` handover on a stream** is receivable through `recvmsg` here, where Linux
    answers `EAGAIN` on every door until bytes arrive - the existing `empty_handover` path
    keeps that deliberate trade (the deadlock it avoids was the datagram udev channel's), and
    this item's measurement is the first thing that charges it.

## item 77 — Netlink's and `AF_PACKET`'s receive doors look without taking, and a truncated answer names its own length

    Item 76's named residual, and the same two flag words on the two families that were not
    listening to them. `NETLINK_ROUTE`'s `recvfrom`/`recvmsg` **popped the front answer whatever
    `MSG_PEEK` said** and returned the copy whatever `MSG_TRUNC` said; the uevent socket's
    `recvmsg` ignored both too; and every `AF_PACKET` door refused `MSG_PEEK` with `EOPNOTSUPP`
    and ignored `MSG_TRUNC` (its `recvmsg` did set the bit in `msg_flags`, but the return was
    still the copy). A peek that takes is worse than one that is refused: the caller gets a
    number and loses the message it asked about.

    **Measured whole on a current host first**, one probe per family, and the rows are the
    claims:

    * **netlink** (`.workbuddy/probe_netlink_peek.c`, Linux 7.2.11, over a `RTM_GETLINK` dump -
      several answers queued, so "was the front taken?" is answered by *which* interface the
      next answer names rather than by an empty queue): a peek answers the front's length and
      leaves it, and the plain receive after it returns the same bytes; the look is repeatable;
      a short receive through 8 bytes answers **8** and consumes the answer; `MSG_TRUNC` turns
      that same short receive into the answer's **own length** (3020 through the 8-byte room)
      while the copy stays 8, and consumes it when it was not a peek; `msg_flags` carries
      `MSG_TRUNC` whenever a copy was cut, **whether or not the caller asked**; a
      zero-length room answers 0, consumes without the peek and leaves the message with it, and
      reports the cut either way; `MSG_DONTWAIT` on a drained queue is `EAGAIN`, peek or not.
    * **`AF_PACKET`** (`.workbuddy/probe_packet_peek.c`, Linux 7.2.11, inside
      `unshare(CLONE_NEWUSER | CLONE_NEWNET)` with `lo` up and one frame the socket sends to
      itself): the same rule on one frame per call - a 16-byte frame through a 2048-byte
      buffer peeks as 16 and the plain receive after it is the same 16 bytes; 8 bytes of it
      with `MSG_TRUNC` peeks as 16 and without the flag answers 8 and consumes; a zero-length
      room answers 0 with the cut in `msg_flags`, consuming without the peek and leaving the
      frame with it.
    * **vsock** (`.workbuddy/probe_vsock_recv.c`, Linux 7.2.11, over `VMADDR_CID_LOCAL` after
      `modprobe vsock_loopback`) - **measured and not served**, and the measurement is why: a
      peek *is* served there (ten bytes peek as 10 and the read after it is the same ten),
      `MSG_TRUNC` is **ignored** (4 of 10 answers 4, copies and consumes the four; peeking the
      same way also answers 4, not 10), the **whole iovec chain** is used (4+4+2 answers 10),
      a zero-length room (or no iovec at all) answers 0 and **takes nothing**, a short room
      leaves the rest queued, and `MSG_WAITALL` does not lengthen the copy. Two of those are
      this kernel's own named shapes now (`docs/uapi/` §5.2): the peek needs a look primitive
      at the device seam - the virtio socket's ring is the crate's
      (`VsockConnectionManager::recv` drains it and the connection's buffer is private), so
      serving it means either a device-side `peek` or a socket-side stash - and the sweep's
      machine has no vsock peer to pin it against. The rest (chain, zero-length room,
      `MSG_TRUNC` ignored) is the next item's work, not this one's.

    **The landing keeps each look at its own layer.** `crate::net::rtnl::peek` clones the
    queue's front (the answer stays where it is, and `recv_blocking(handle, look)` gives the
    look the same park the receive has - `MSG_PEEK` must not turn a blocking read into
    `EAGAIN`); `crate::link::Frames::peek` is the packet ring's front, which `PacketSock` clones
    out (`try_peek`/`peek_blocking`) because the ring sits behind a `critical_section` cell and
    a frame is one MTU at most; and `crate::uevent::peek` clones a `Message`, whose bytes were
    already an `Arc` the send shared. The doors then read the flag word instead of ignoring it:
    `handle_recvfrom`'s two netlink and packet branches, `handle_recvmsg`'s netlink, packet and
    uevent arms, and `recvmsg_netlink`, which now returns the block's own length when
    `MSG_TRUNC` asked while the scatter wrapper still gets the copy's count for its own
    bookkeeping, and reports the cut in `msg_flags` either way.
    A zero-length room is a receive at every one of them, as it was.

    **Pins**: `t-netlink`'s **14 new claims** (the two doors' peeks, repeatability, the short
    receive's consume and its `msg_flags`, `MSG_TRUNC`'s return, both zero-length shapes and the
    drained-queue `EAGAIN`), `t-uevent`'s **5** (the block's own length through 8 bytes, the cut
    in `msg_flags`, the room that fits, a zero-length peek, and a zero-length receive that
    consumes) plus its new fixture claim, `t-dhcp`'s **4** on the external group (a
    `recvfrom` and a `recvmsg` peek each naming the frame's length and each leaving it for the
    read that follows) - green when the external group is run (`pass=199 fail=0`, 199 built) -
    and one host unit test in `mpos-link`
    (`a_peek_shows_the_front_and_leaves_the_queue_alone`).

    **Counterfactual**, every `MSG_TRUNC` return put back to the copy's length and the peeks
    kept, every pin kept: x86_64 `pass=190 fail=2` - `t-netlink` and `t-uevent` red at the two
    length claims by name, while the ordinary receives and the fixtures beside them stay green.
    Putting the netlink/uevent peek back is not a countable arm: the first look consumes the
    answer it was asked about, and the next blocking receive then parks on a queue the old take
    had already drained (observed: the named repeatability claims red, then the sweep held at
    that queue until killed).

    Gate: x86_64 `pass=192 fail=0 (192 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33;
    aarch64 `pass=191 fail=0`, `mpos-kunit 32 passed, 0 failed`; host unit suite 584 (unchanged;
    the new test is `mpos-link`'s 13th), `xtask` 123. Named rather than served: the **vsock**
    family's peek and its whole-chain room (measured above, with the prerequisite each needs),
    and the vsock `recvmsg` still reads its chain's first segment.

## item 78 — A named `SOCK_SEQPACKET` connection keeps its message boundaries, and its messages carry their descriptors

    The last divergence in the AF_UNIX family (`docs/notes/` §4.1): a listener reached by a
    path or an abstract name handed `accept` a `DuplexEnd` byte channel, so the type whose own
    description is "message boundaries over a stream" carried a byte stream for exactly the
    connections that had a name. A `socketpair(AF_UNIX, SOCK_SEQPACKET, ...)` half already had
    a message queue; the two shapes disagreed about the same type name, and `t-unix-dgram`
    recorded the difference as "a named one is served as the stream this family has".

    **Measured whole on a current host first**, one probe per question
    (`.workbuddy/probe_unix_seqpacket{,2,3}.c`, Linux 7.2.11, abstract and path names):

    * **One send is one message.** 3 then 5 bytes read back as 3 then 5; a `sendmsg` chain of
      4+4+2 is one message of 10; the peer sees no second message behind it.
    * **A short read truncates that message.** Ten bytes through a four-byte room answer 4,
      discard the remaining six, leave nothing queued, and the next read is `EAGAIN` - the
      boundary belongs to the message, not to a byte stream.
    * **The flags are the message family's.** `recvmsg(MSG_TRUNC, iov 4)` answers the message's
      own length (10) with `MSG_TRUNC` in `msg_flags` and consumes it; `MSG_PEEK` answers 10 and
      leaves it for the read that follows; a zero-length room answers 0, reports the cut and
      **consumes** (with `MSG_PEEK`, leaves).
    * **A zero-length message is a message.** `write(fd, "", 0)` queues one; the peer's read
      answers 0 and the queue is empty afterwards - not EOF, which only `shutdown` sends.
    * **`read(2)` with a zero-length buffer is not a receive.** It answers 0 and leaves the
      front message where it is (drained queue included, where a nonblocking `read` also
      answers 0 rather than `EAGAIN`); `recvfrom`/`recvmsg` with the same empty room are the
      receives that consume. The distinction is `vfs_read`'s short-circuit, not the family's.
    * **`SCM_RIGHTS` rides the message.** The receive that takes it installs the descriptors;
      a peek installs its own set and leaves the record, so the take after it installs a second,
      distinct descriptor and both stay live. A zero-length data room still receives the
      control chain and consumes the message.
    * **The two ends must agree on their type.** Through a path name, a stream dialing a
      seqpacket listener and a seqpacket dialing a stream one both answer `EPROTOTYPE` (91);
      an abstract name misses its type-keyed lookup and answers `ECONNREFUSED` (111).

    **The landing reuses the message machinery rather than adding a second one.** The
    listener's queue now holds a `Pending` - `Stream(Arc<DuplexEnd>, UCred)` for the byte shape,
    `Messages(Arc<UnixSock>)` for the sequenced-packet one - and `connect_messages` mints the
    server socket, points both ends at each other as `dgram_peer`s and records the credentials
    before queuing it. The server socket is allocated and an unqueued one dropped *outside* the
    listener's critical section: registering takes the registry's own section, and
    `UnixSock::drop` takes it again. `accept` returns the queued socket as it stands for the
    message shape, and wraps a channel for the stream shape. From there both ends use the
    `recvq`/`push_dgram` path a `socketpair` half already had.

    Three door-level details the measurement forced. `read_at` answers 0 for a zero-length
    buffer before touching the queue (it used to take a message and answer 0 - consuming a
    zero-length message and answering `EAGAIN` on an empty nonblocking queue). `Dgram`/`RecvDgram`
    now carry the message's `SCM_RIGHTS` descriptors (`Vec<Arc<File>>`); the message `sendmsg`
    collects them and the `recvmsg` arm installs them through the same `emit_unix_control` the
    stream uses, with `MSG_CMSG_CLOEXEC` honored, while `recvfrom` (no control buffer) drops
    them - the alternative was silently losing the handover udev's control socket can use the
    moment the connection stopped being a `DuplexEnd`. `connect_to` gained a `WrongType` arm so
    a path-name mismatch answers `EPROTOTYPE`; the abstract table never reaches it.

    **Pins**: **`t-unix-seqpacket`, 58 claims** - the fixture (a named listener, a connect and
    an accept), boundaries through `write`/`read` and a 4+4+2 `sendmsg`, the short read's
    discard, `MSG_TRUNC`'s length and `msg_flags`, `MSG_PEEK` leaving the message, a
    zero-length message, the zero-length `read` no-op versus the consuming `recvfrom`/`recvmsg`,
    the SCM handover through a peek/take pair and a zero-length room, and both type-mismatch
    directions - green on a current host first, then on both machines. `t-unix-dgram`'s claim
    "a named SOCK_SEQPACKET socket is served as a stream: listen succeeds" became "…listens",
    which is all it was measuring, and its module note and `t-udev-worker`'s point at the new
    file. No host unit test: the module is `target_os = "none"`.

    Gate: x86_64 `pass=193 fail=0 (193 built)`, `mpos-kunit 32 passed, 0 failed`, exit 33;
    aarch64 `pass=192 fail=0`, `mpos-kunit 32 passed, 0 failed`; host unit suite 584 (unchanged),
    `xtask` 123. Named at landing and served by item 79: `sendto` with a destination on an
    already-connected seqpacket socket answered `EISCONN` where Linux sends - and the same
    divergence held for a connected datagram socket, where Linux takes the address.

## item 79 — A send on a connected AF_UNIX socket takes its destination where Linux takes it: dropped by a seqpacket connection, honoured by a datagram one

    Item 78's named residual - and it was two rules, not one. Both message families
    answered `EISCONN` whenever the caller gave a destination on an already-connected
    socket; Linux decides by the socket's type. **Measured whole on a current host
    first**, one probe per question (`.workbuddy/probe_unix_sendto_connected{,2}.c`,
    `.workbuddy/probe_unix_sendto_family_only.c`,
    `.workbuddy/probe_unix_sendto_long_name.c`,
    `.workbuddy/probe_unix_sendto_wrongtype.c` and
    `.workbuddy/probe_unix_sendto_huge_name.c`, Linux 7.2.11, path and abstract names):

    * **A connected seqpacket socket drops the destination.** The peer is the whole
      answer, and the name is not resolved or validated: naming the listener itself,
      another live socket, a path nobody holds, a dead abstract name, a
      `SOCK_STREAM` listener, a family-only `sockaddr_un`, and an address 110 or 128
      bytes long all send to the peer. Only the syscall layer's own copy is visible to
      a caller: a bad pointer with a non-zero `addrlen` is `EFAULT` (measured), and
      `addrlen` beyond `sockaddr_storage` (128) is `EINVAL`.
    * **A connected datagram socket honours the destination.** The name is resolved and
      validated exactly as an unconnected send's is: a different live socket receives
      the message, a dead path is `ENOENT` (2), a dead abstract name and a regular file
      are `ECONNREFUSED` (111), a live `SOCK_STREAM` listener is `EPROTOTYPE` (91), a
      family-only address is `EINVAL` (22), and the pinned peer only decides where a
      send *without* a name goes.
    * **A connected stream pair still answers `EISCONN`** (106) - the channel door's
      rule, which `t-sockmsg` pins and this item did not touch.
    * **An unconnected seqpacket socket's named send is `ENOTCONN`** (107), as is a
      listening one's. Linux drops the name first and then finds no peer. Named rather
      than served: this family's message doors are reached only by a socket that
      already carries messages (a datagram, or a connected seqpacket), so an
      unconnected seqpacket socket did not enter them and its answer was unchanged
      (`ENOTSOCK`). **Served by item 81.**

    Served by `UnixSock::ignores_send_address` (connected sequenced-packet, and nothing
    else), by the two send doors routing an address through a new `copy_send_address`
    - the syscall layer's bounds and readability, no interpretation - when that answers
    true, and by `send_dgram`: the `EISCONN` refusal is gone, a seqpacket send targets
    its peer whatever `dest` carries, and a datagram send resolves and validates the
    name exactly as an unconnected one does.

    **Pins**: `t-unix-seqpacket` gains **eight claims** (58 -> 66): three sends that
    each name something the socket must ignore - the listener's own live name, a path
    nobody holds, and a family-only address - each read back as one whole message, the
    `EINVAL` a 129-byte address still earns from the syscall layer, and the empty queue
    behind them. `t-unix-dgram` gains **six** and loses its one divergence claim: the
    named send lands at the connected peer, reaches another live socket the name picks,
    leaves the pinned peer's queue alone, and a dead name is `ENOENT`. Both green on a
    current host first (`MPOS_ON_HOST=1`). `t-unix-peek`'s
    helper note changed with the rule it cited (its sender is still unconnected, for
    the different reason that a `socketpair` end is unnamed), and two stale notes -
    `net::socket`'s `SOCK_SEQPACKET` paragraph and `net::unix`'s `carries_messages` doc,
    both still saying a *named* seqpacket socket was served as a byte stream - were
    corrected in place.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32 passed, 0 failed on
    both; host unit suite 584 (unchanged), `xtask` 123. Counterfactual (the old rule restored -
    the doors' `EISCONN` and `send_dgram`'s refusal): `t-unix-dgram`'s first named-send claim goes
    red with `EISCONN` (106), and the pin's next read - a blocking receive for a message the old
    kernel never sent - parks there, so the arm was observed and stopped rather than counted (the
    shape item 77's peek counterfactual had). Named: the unconnected seqpacket send above (served
    by item 81), and the datagram-to-*stream*-listener `ECONNREFUSED` where Linux answers
    `EPROTOTYPE` (the existing `t-unix-dgram` claim, untouched - served by item 80).

## item 80 — A datagram aimed at a path naming another type is `EPROTOTYPE`; an abstract name of another type stays `ECONNREFUSED`

    Item 79's named residual. A datagram `sendto` and a datagram `connect` answered
    `ECONNREFUSED` for any live socket that carried no messages, so a path naming a
    `SOCK_STREAM` (or `SOCK_SEQPACKET`) listener was indistinguishable from a dead
    name. Item 79's own measurement had recorded only the abstract half, where the
    answer *is* `ECONNREFUSED`; measuring the path half split the question.
    **Measured whole on a current host first** (`.workbuddy/probe_unix_dgram_wrongtype.c`,
    Linux 7.2.11):

    * a **path** naming a live socket of another type is `EPROTOTYPE` (91) for a datagram
      `sendto` and for a datagram `connect`, whether the listener is a stream or a
      sequenced-packet socket, and whether the sender is connected or not;
    * an **abstract** name of another type is `ECONNREFUSED` (111) in every one of
      those shapes: the abstract table is keyed by the socket's type, so the lookup
      misses before any type comparison happens.

    Served by `send_dgram`'s path branch - the node's live socket is compared with the
    sender's type after the liveness upgrade - and by `connect_to`'s datagram arm, which
    answers `ConnectRefused::WrongType` instead of `Refused` when the resolved listener is
    not a datagram socket. The abstract half needed no change and is now pinned, so a
    lookup that stopped being type-keyed would be caught.

    **Pins**: `t-unix-dgram`, 49 -> 57 claims - the two divergence claims became
    `EPROTOTYPE`, and four new claims (plus the four setup claims that name the two
    listeners) pin the seqpacket-listener path and the abstract-name distinction, send
    and connect each. All 57 green on a current host first (`MPOS_ON_HOST=1`).

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32 passed, 0
    failed on both. Counterfactual (the old rule restored - `connect_to`'s datagram arm
    and `send_dgram`'s path branch both answering `ECONNREFUSED`): x86_64
    `pass=192 fail=1`, `t-unix-dgram` red on exactly the four path claims
    (`got Connection refused (os error 111), want Protocol wrong type for socket (os
    error 91)`), while the abstract claims stay green - the distinction the pin is for.
    The sweep completed because both doors refuse immediately rather than parking.

## item 81 — A seqpacket socket with no peer sends nothing, whatever the name says: `ENOTCONN`

    Item 79's other named residual. A `SOCK_SEQPACKET` socket that is unconnected
    or listening has message doors but no peer; the send doors recognised only
    sockets that already carried messages, so this shape fell through to
    `socket_handle_of_fd` and answered `ENOTSOCK` (88) - including for a named
    `sendto`, which is the shape item 79 had measured on Linux. **Measured whole
    on a current host first** (`.workbuddy/probe_unix_seqpacket_unconnected.c` and
    `..._msg.c`, Linux 7.2.11, unconnected and bound+listening alike):

    * `write`, `sendto` with no name, and `sendto` naming a live same-type
      listener, a dead path, a regular file, a family-only address or a live
      abstract name all answer `ENOTCONN` (107): the destination is not looked at,
      let alone resolved;
    * a NULL address pointer is "no name" whatever the length says: `sendto`
      with NULL/16 and `sendmsg` with NULL/16 both answer `ENOTCONN`, not
      `EFAULT`;
    * `sendto` still runs the syscall layer's copy first, so a 129-byte address
      is `EINVAL` (22) and a bad pointer with a non-zero length is `EFAULT` (14);
    * `sendmsg` imports the header, the name bytes, the iovec array and the
      control buffer, and an unreadable one is `EFAULT` - but it does not bound
      `msg_namelen` to `sockaddr_storage` (129 bytes still answers `ENOTCONN`)
      and does not touch the buffers the iovec entries point at (a bad iovec
      base is `ENOTCONN`, not `EFAULT`).

    Served by `UnixSock::unconnected_seqpacket` (a sequenced-packet socket that
    `carries_messages` does not: no peer, whatever its name) and a branch in each
    send door: `sendto` routes the address through `copy_send_address` (the
    syscall layer's bounds/readability, no interpretation) and then answers
    `ENOTCONN`; `sendmsg` performs the generic layer's imports and then the same
    answer. The byte doors already said `ENOTCONN` through `write_at`, which is
    why only the message doors changed.

    **Pins**: `t-unix-seqpacket`, 69 -> 88 host-run claims (19 new): the byte door,
    the named, unnamed and NULL-with-length `sendto` shapes, the two syscall-layer
    refusals, the seven `sendmsg` import shapes plus its NULL-with-length form,
    and the listening socket's two sends. All green on a current host first
    (`MPOS_ON_HOST=1`).

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. Counterfactual (the branch removed): x86_64
    `pass=192 fail=1`, exactly the 17 message-door claims red with `ENOTSOCK` (88),
    while `write` stays green at `ENOTCONN` - the byte door was never the gap. The
    sweep completes because every claim refuses immediately.

## item 82 — A `sockaddr_un` path ends at its first NUL, and the bytes after it are storage

    §4.1.1's last named divergence, and the only one in the AF_UNIX family that was
    an *error* rather than a gap: the parser answered `EINVAL` for a path carrying
    bytes behind its terminator, which Linux accepts. Linux's `unix_mkname` is
    `strnlen(sun_path, len)` over the caller's `addrlen`, so a path ends at the
    first NUL and whatever follows is the caller's own storage.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_path_tail.c`,
    Linux 7.2.11, one fresh name per scenario):

    * `"abc"` + NUL + `"XYZ"` under an `addrlen` covering all of it: `bind` 0, and
      `getsockname` answers `family + "abc" + NUL` with `addrlen` 6;
    * a 108-byte field with **no NUL anywhere** binds whole, and the reported
      `addrlen` is **111** - one past the field - because `move_addr_to_user` copies
      `min(len, ulen)` bytes and reports the used length regardless;
    * `addrlen` 2+109 (past the struct) is `EINVAL`, which is the parser's own bound
      and not part of this rule;
    * a `connect` spelled with the same trailing bytes reaches the listener, and
      `"abc"` alone answers `ECONNREFUSED`: the key is the prefix, and nothing was
      listening there;
    * an **abstract** name is length-delimited, so `"\0abc\0XYZ"` has interior NULs
      that *are* its name and echoes all eight bytes - the half this rule must not
      touch.

    Served by deleting the interior-NUL refusal in `net::unix::sockaddr_un_addr`.
    Everything below the parser already agreed with Linux and needed no change:
    `UnixAddr::Path` stores the prefix, `sockaddr_un_bytes` writes the prefix plus
    its own terminator, and `bind_path` keys the table on the resolved *node*, so
    the `EADDRINUSE` arbitration and the `getsockname` echo followed for free.

    **Pins**: `t-unix`, 74 -> 80 claims - the tail bind, its short echo, the node
    being the prefix's (and *not* `…sockXYZ`), a second bind of the bare prefix
    answering `EADDRINUSE`, a connection spelled with the same trailing bytes
    accepted, and the full 108-byte field's whole-field echo with its 111-byte
    `addrlen`. All six green on a current host first (`MPOS_ON_HOST=1`, 80 of 80),
    and the same 80 `ok:` lines on both machines. The first draft claimed the
    reported terminator was *written* into the caller's field and failed on the
    host - which is what a host run is for.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. Counterfactual (the refusal restored, every pin kept):
    the six new claims go red, the bind one first with `EINVAL` (22), and the
    short-circuited accept keeps the sweep moving.

    **What stays named**: the path half is still a `String`, so a non-UTF-8 *path*
    is refused here where Linux takes any bytes. That is a type change
    (`UnixAddr::Path(Vec<u8>)` and every path door under it), not a rule.

## item 83 — A socket's address is three answers, and `getpeername` is the connect's record

    `getsockname`/`getpeername` answered one thing for every socket with no
    name - `AF_UNIX` plus an empty path, three bytes - and that one answer was
    wrong in three directions. Linux keeps the *address* on the socket
    (`unix_sk(sk)->addr`) and the *peer's* address on the socket too
    (`unix_peer`), so both outlive the node and the peer they name.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_unnamed.c`,
    `probe_unix_names{,2}.c`, `probe_unix_accepted_name.c`,
    `probe_unix_bind_after.c`, `probe_unix_from.c`, `probe_unix_noname_bytes.c`,
    Linux 7.2.11):

    * `getsockname` on a fresh socket of any type, and on a `socketpair` end,
      is `addrlen` **2** - the bare family, no path byte written;
    * after `bind` of the *empty* path it is `addrlen` **3** - the family and
      its NUL, a name of zero bytes rather than no name;
    * `getpeername` is `ENOTCONN` (107) for every unconnected shape (bound,
      unbound, listening alike), the *listener's bound path* on a connected
      client, and the bare family on an **accepted** end (the client bound
      nothing);
    * `getsockname` on an accepted end is the *listener's* bound path -
      inherited - and that inheritance is what makes a second `bind` there
      `EINVAL` (22), while the client end of the same connection answers the
      unnamed address and binds happily;
    * both names survive the node: a listener that `unlink`s its own path still
      answers it, and so do both connected ends; `getpeername` still names a
      peer that has been closed;
    * a datagram `sendto` naming a destination does **not** move the socket's
      peer - only `connect` does;
    * a delivery from an unbound sender reports `addrlen` **0** and writes not
      one byte of the caller's storage, where a sender bound to the empty path
      reports 3.

    Served by two facts where there was one flag: `UnixState::local` (written
    by `bind` - path, abstract or autobind - and by `accept`, which copies the
    listener's) and `UnixState::peer_name` (written by `connect` from the
    listener's `local`). `is_bound` now asks "does this socket answer a name"
    rather than "did `bind` run", which is the split an accepted end needs.
    `write_addr` answers the nameless case with `write_no_name` (the bare
    family, two bytes) and a delivery from an unnamed sender takes its own
    door, `write_no_source` (`addrlen` zero, nothing written) - three answers
    where there was one, each measured.

    **Pins**: `t-unix` 80 -> 90 claims (the unbound socket's two bytes and its
    untouched storage, the empty path's three, the accepted end inheriting the
    name, the `EINVAL` that earns, the client end's bare family and the bind it
    is therefore allowed, and `getpeername` naming the dialled listener);
    `t-unix-dgram`'s unnamed-sender claim rewritten from "three bytes of empty
    name" - this kernel's shape - to "`addrlen` zero, storage untouched"; and
    `t-unix-rustix`'s `getpeername` claim follows the same rule. Green on a
    current host first, and `ok:` for `ok:` identical on both machines.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. Counterfactual (the three accessors put back to
    the old flag, every pin kept): `pass=191 fail=2`, five claims red by name -
    `t-unix`'s `the accepted end's own name is the listener's`,
    `so a bind on the accepted end is EINVAL`, `and its own bind is served` and
    `and the peer is the name the connect dialled`, and `t-unix-rustix`'s
    `getpeername on the connected end names the listener`.

    **What stays named**: the address of a socket that travels through
    `SCM_RIGHTS` was not measured here, so nothing is claimed about it -
    `t-unix-scm` pins the descriptor's credentials and not its name.

## item 84 — A second `connect` moves a datagram's peer, and the name is resolved before it is refused

    `connect` on a unix socket answered `EISCONN` whenever the socket already
    had a peer, whatever its type. Linux decides by type, and asks the *name's*
    question first.

    **Measured whole on a current host first**
    (`.workbuddy/probe_unix_reconnect{,2,3,4,5}.c`,
    `probe_unix_dgram_connect.c`, `probe_unix_selfconnect.c`,
    `probe_unix_connect_order{,2}.c`, Linux 7.2.11):

    * a **datagram** socket's second connect to another live name returns 0, and
      `getpeername` reads the new one - a message sent afterwards reaches the
      new peer only; connecting back to the first moves it back;
    * a refused reconnect (a dead name, a wrong-type name, a family-only
      address) leaves the **previous peer in place**, because the name is
      resolved before anything is pinned;
    * a **stream** or **sequenced-packet** socket's second connect is `EISCONN`
      (106) - but only after the name resolves, so a dead path is `ENOENT` (2)
      and an unheld abstract name `ECONNREFUSED` (111);
    * a **listening** socket's own connect is `EINVAL` (22), a *bound but not
      listening* one's is `ECONNREFUSED` (111), and so is a bound datagram
      socket's;
    * a connect to a node that is not a socket is `ECONNREFUSED` at mode 0666
      and `EACCES` at mode 0000 - the permission is asked before the kind.

    Served by moving the `EISCONN` gate onto "is this a stream" instead of "has
    this socket a peer", so a datagram's reconnect reaches `connect_to`'s
    datagram arm and moves `dgram_peer` and `peer_name`; and by resolving the
    name before the gate in both `connect_path` and `connect_named`.

    **Pins**: `t-unix-dgram` 57 -> 60 claims (the second connect moving the peer,
    `getpeername` naming the dialled socket, and a refused reconnect leaving the
    previous peer in place), and `t-unix` 90 -> 91 (a connected stream socket
    naming a dead path hears `ENOENT`, not `EISCONN`). Green on a current host
    first, and `ok:` for `ok:` identical on both machines.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. Counterfactual (the old `EISCONN`-first rule for
    both types, every pin kept): `pass=192 fail=1`, and the one red claim is
    `a datagram socket connects a second time, and its peer moves (got Socket is
    connected (os error 106))`.

## item 85 — `listen` is the named call, and a connected socket is not a listener

    `listen` accepted anything that was not a datagram socket: a fresh, unbound
    stream socket opened an accept queue nothing could ever reach. Linux's
    `unix_listen` asks the socket's **address** first - `EINVAL` when there is
    none - and its **state** as well, so a connected socket and an accepted end
    are refused too.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_listen.c`,
    `probe_unix_listen2.c`, Linux 7.2.11):

    * `listen` on a fresh, unbound `SOCK_STREAM` or `SOCK_SEQPACKET` socket is
      `EINVAL` (22);
    * on a bound one, and on an **abstract**-bound one, it is 0 - the rule is
      the address and not the filesystem;
    * a second `listen` is 0 and only replaces the backlog; a negative backlog
      and 0 are 0 too;
    * on a **connected** stream socket and on an **accepted** end it is
      `EINVAL`;
    * on a datagram socket, bound or not, it is `EOPNOTSUPP` (95).

    Served by two refusals in `net::unix::listen`: no address
    (`local_addr().is_none()`) is `EINVAL`, and a channel
    (`end_of().is_some()`) is `EINVAL`. The datagram refusal was already
    Linux's number and is now pinned bound and unbound rather than only
    unbound.

    **Pins**: `t-unix` 91 -> 96 claims - the two unbound refusals, the datagram
    one, an abstract-bound socket listening, and a second `listen` succeeding.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. Counterfactual (both refusals removed, every pin
    kept): `pass=192 fail=1`, and the red claims are
    `listen on an unbound stream socket is EINVAL` and
    `listen on an unbound seqpacket socket is EINVAL`.

## item 86 — `shutdown` is a flag on the socket, not a handshake with a peer

    `shutdown` on a unix socket answered `ENOTCONN` when there was no channel.
    Linux's `unix_shutdown` never asks: it sets a flag on the socket and
    returns 0, so an unconnected stream socket, a listener and a datagram
    socket all succeed, and the only thing refused is the `how` word.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_shutdown.c`,
    Linux 7.2.11): an unconnected `SOCK_STREAM` answers 0 for `SHUT_RD`,
    `SHUT_WR` and `SHUT_RDWR`; so does a listening socket, an unconnected
    datagram socket, and a connected socket of every type; a second `shutdown`
    of the same half is 0; and `how` = 3 or -1 is `EINVAL` (22) - the one
    refusal the call has.

    Served by removing the `ENOTCONN` arm from the handler: it closes the
    channel when the socket has one and reports success either way. The `how`
    bound was already there and is what the negative case needs too, because
    the argument arrives as an unsigned word.

    **Pins**: `t-unix` 96 -> 99 claims - the unconnected stream, the datagram
    socket, and the two out-of-range `how` words through the raw door (the word
    is the claim, and `rustix`'s enum cannot spell 3 or -1).

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. Counterfactual (the `ENOTCONN` arm restored,
    every pin kept): `pass=192 fail=1`, and the two red claims are
    `shutdown on an unconnected stream socket answers 0` and
    `shutdown on a datagram socket answers 0`.

## item 87 — The "implicit rendezvous" was never Linux's shape

    The AF_UNIX module header carried a divergence since the family was written:
    "a bound *stream* socket that was never `listen`ed answers `ECONNREFUSED`;
    Linux's implicit rendezvous at a socket nobody listened on is state this
    kernel declines to model". It was a *record*, not a measurement, and it made
    a correct answer look like a gap - `t-unix-rustix` asserted it as
    "(divergence: Linux joins)" and `t-unix` accepted either answer under
    `MPOS_ON_HOST`.

    **Measured on a current host first** (`.workbuddy/probe_unix_unlistened{,2}.c`,
    Linux 7.2.11):

    * `SO_ACCEPTCONN` on a bound, never-`listen`ed stream socket is 0;
    * `connect` to it is `ECONNREFUSED` (111);
    * the same name connects (0) the moment `listen(fd, 4)` runs;
    * a bound, never-`listen`ed **sequenced-packet** socket is `ECONNREFUSED`
      too;
    * a bound **datagram** socket connects (0) - a datagram needs no `listen`.

    So both machines already agreed; what was wrong was the record. `t-unix`'s
    claim loses its `host_run()` escape and asserts the refusal on every
    machine, `t-unix-rustix`'s loses the "(divergence: Linux joins)" text, and
    the module header and §4.1's divergence list drop the entry.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both.

    **What this is worth recording as**: the trap `AGENTS.md` §5 names - "a
    recorded measurement is about the machine that made it" - in its other
    direction. A *claim* that was never measured became a permanent-looking
    refusal, and two tests were built to accept it. The tell was the claim
    naming no probe, no register and no run.

## item 88 — `accept`'s `addr` is the connector's name, and its two pointers are asked separately

    `accept` wrote the **listener's** bound path into the caller's address
    buffer. Linux writes the **connector's** name there: it is the answer to
    "who just connected", and a server that reads its own address back
    concludes every client is itself.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_accept3.c`,
    `probe_unix_accept{,2,4,5}.c`, Linux 7.2.11):

    * `accept` with an **unbound** client reports `addrlen` **2** - the bare
      family, no path byte written;
    * `accept` with a client that bound a path reports that path and its own
      length - the *client's* name, never the listener's;
    * a 4-byte room still reports the *needed* length and copies four bytes;
    * a NULL `addr` is served, and `addrlen` is left exactly as the caller left
      it (a seeded 12345 comes back 12345);
    * a NULL `addrlen` beside a real `addr` is `EFAULT` (14);
    * `accept4` with an unknown flag bit is `EINVAL` (22), before anything else.

    Served by `Pending::Stream` carrying the connector's address beside the
    listener's, `with_state_connect` recording it as the accepted socket's
    `peer_name`, and `accept`'s write-back reading that instead of the
    listener's own name - so `accept` and `getpeername` on the accepted end
    agree, as they do on Linux. The two pointer arguments are then asked about
    separately: a NULL `addr` skips the write-back and a NULL `addrlen` beside
    a real `addr` is `EFAULT`.

    **Pins**: `t-unix` 99 -> 106 claims - the unbound client's two bytes, a
    client that bound its own name reported as itself, the NULL-`addr` shape
    with `addrlen` untouched, and the NULL-`addrlen` refusal.

    Gate: x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32
    passed, 0 failed on both. **Two counterfactuals**, one per half, each with
    every pin kept: the write-back put back to the listener's name is
    `pass=192 fail=1` with both address claims red; the old
    `addr_ptr != 0 && len_ptr != 0` condition restored is `pass=192 fail=1`
    with `accept with a real addr and a NULL addrlen is EFAULT` red.

## item 89 — `fstatfs` names the filesystem the descriptor was opened on, not the root's

    A descriptor carries no path, and an inode does not name the filesystem it
    sits in - so this door answered by *identity*: a mount's own root inode
    named that mount, and everything else - every regular file, every directory
    below a mountpoint, every `/proc/<pid>` file - reported the root
    filesystem. `df` read a mounted filesystem as the root's free space, and
    `statfs(2)` on a path disagreed with `fstatfs(2)` on a descriptor opened
    through it, which are Linux's one answer about one file.

    **Measured whole on a current host first** (`.workbuddy/probe_fstatfs.c`,
    Linux 7.2.11): Linux asks the vfsmount the open took, so a descriptor for a
    file *inside* a mount names that mount. `/` and `/etc/hostname` both answer
    this machine's ext4 (the second is a regular file inside it), `/proc` and
    `/proc/self/status` both answer procfs, `/dev/shm` and a file this probe
    wrote there both answer tmpfs, and in every row `fstatfs(fd)` equals
    `statfs(path)`; a closed descriptor is `EBADF` (9); `sizeof(struct statfs)`
    is 120 with `f_type` at 0. The shape that used to fall back is not an edge
    case, it is every node that is not its own mount's root - nothing else can
    answer the question.

    **The landing** makes the reverse lookup the forward one's mirror rather
    than a second rule beside it. The descriptor already records the device
    number it was opened on (`File::dev` - the record `fstat`'s `st_dev` has
    been answered from since §15's own work on that field, which is why the two
    doors already agreed about it while disagreeing about everything else), and
    `MountTable::point_of_dev` walks `mount_points` - the very table
    `serving_point` reads forward - for the record carrying that number, taking
    the deepest mountpoint when two records share one, which is the
    longest-prefix choice the forward walk would have made. `fstatfs` keys on
    it, and keeps the inode-identity walks for a descriptor no path made (a
    pipe end, a socket, an anonymous inode), which really is on the root
    filesystem. `MountTable::fs_of_dev` hands back the instance **and** the type
    from that one record, so `f_blocks` and `f_type` cannot come from two
    different mounts, and a number no mount claims resolves to the root slot -
    the fallback every other door here takes.

    **Pins**: `t-statfs`, 24 -> 26 claims - the mountpoint's own descriptor
    opening, a procfs file inside that mount opening, `fstatfs` naming procfs
    rather than the root, the mountpoint's descriptor agreeing with the
    inside-file one, and `statfs` on the file's path agreeing with `fstatfs` on
    its descriptor - **green on a current host first (26 of 26,
    `MPOS_ON_HOST=1`, musl build run on the host's own Linux)** and `ok:` for
    `ok:` identical on both machines. `/proc` is the fixture and `/tmp` is
    deliberately not: the first machine's boot mounts a ramfs at `/tmp` **over
    a ramfs root**, so a claim comparing those two answers is answered alike
    whether or not the kernel is right. Five claims shaped that way were
    written first, stayed green in the counterfactual below, and were **dropped
    rather than kept as decoration** - a pin that cannot go red is not
    evidence. Two of the claims that remain do not lean on the fixture's magic
    at all: comparing the *mountpoint's* descriptor with the file-inside-it
    one catches exactly the old answer, whichever numbers the machine's root
    happens to carry. Beside the pin, one host unit test in `vfs::mount` binds
    the two directions: the device number the forward walk hands out for
    `/mnt/inside` finds the same instance back, and a number no mount claims
    answers the root slot.

    **Counterfactual** - the pre-item body restored (`fstatfs` asks the inode by
    identity again), every pin kept: x86_64 `pass=204 fail=1 (205 built)`,
    `mpos-kunit 32 passed, 0 failed`, exit 33, and the one red program is
    **`t-statfs`** with exactly three claims red - "and it agrees with fstatfs
    on the mountpoint's own descriptor", "fstatfs on a file inside a mount
    names the mount, not the root" and "and statfs on that file's path agrees
    with fstatfs" - while its `/tmp`, root, whole-struct and `EBADF` claims
    stay green, which is what says the pin measures this item and nothing else.

    **Gate**: x86_64 `pass=205 fail=0 (205 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 33; aarch64 `pass=204 fail=0` (204 staged, 1 left out - the
    machine's own gap), `mpos-kunit 32 passed, 0 failed`, reaching `=== END`;
    both machines' `ok:` lines byte-identical (26 of 26). Host unit suite 585
    (was 584), `xtask` 123; `cargo fmt --check` clean on the touched files;
    clippy's warning set unchanged; the depgraph ratchet holds - the new call is
    `syscall -> vfs::mount`, an edge that already exists.

    **Named**: nothing here measures `statvfs`/`fstatvfs`, which are not
    syscalls at all on x86_64 - glibc and musl build them over `statfs` in
    userspace, so there is no number here to answer - and at the time of
    writing no pin asked what `f_fsid` should carry: Linux answers there with
    the tuple it mints per superblock, and this kernel wrote zeros. That second
    gap is item 90, right below.

## item 90 — `f_fsid` names the filesystem instance, and it is not zero any more

    item 89 named this gap in its own closing paragraph: `fstatfs` and
    `statfs` now agree about every field except the one that says *which*
    filesystem answered. `f_type` is the kind - two ramfs mounts, or two ext4
    mounts, share it - so a caller comparing two mounts through `statfs(2)`
    had nothing to compare, and `f_fsid` is the field Linux's own man page
    offers for exactly that. This kernel wrote zeros there.

    **Measured whole on a current host first** (`.workbuddy/probe_fsid.c`,
    Linux 7.2.11): `sizeof(struct statfs)` is 120, `offsetof(f_fsid)` is 56,
    `sizeof(f_fsid)` is 8, and every row is non-zero - `/` and `/etc/hostname`
    answer `0x0bbbdac55153df56`, `/proc` and `/proc/self/status` answer `0x18`,
    `/sys` `0xb986c123eab0af2c`, `/tmp` `0x2cfcbb6662e13bc9`, and in every row
    `fstatfs(fd) == statfs(path)`. The split is the constraint: `/proc`'s value
    is **exactly its own `st_dev`** (`0x18`, i.e. `MKDEV(0,24)`) run through
    Linux's `new_encode_dev`, because procfs has no UUID; every filesystem with
    one - ext4, tmpfs, sysfs - answers a 64-bit hash instead, different on
    every machine and every boot. So this item can pin the *structure* and not
    a value, and it says so rather than pinning a number that would be this
    machine's alone.

    **The landing** gives the field one source instead of inventing a second
    identity beside `st_dev`. `abi::encode_fsid` spells `new_encode_dev`
    (`include/linux/kdev_t.h`) - checked against a C program before it was
    trusted (`.workbuddy/check_encode.c`: `MKDEV(0,24)` -> `0x18`,
    `MKDEV(0,25)` -> `0x19`, `MKDEV(1,0)` -> `0x100`, `MKDEV(259,3)` ->
    `0x10303`) - and both doors feed it the device number they already report
    as `st_dev`: the descriptor's recorded `File::dev` for `fstatfs`,
    `mount::dev_of_path` for `statfs`, both going through the same reverse walk
    item 89 built. `write_statfs` writes the eight bytes as **two `u32`s, low
    half first**, rather than one `u64` store, because the layout is a
    `__kernel_fsid_t` and not a word - and a single store would have been right
    on these two machines and wrong on a big-endian one.

    **Pins**: `t-statfs`, 26 -> 36 claims. `f_fsid` has no spelling through
    either wrapper - `rustix::fs::StatFs`'s `Fsid::val` is private, and
    `libc`'s own `fsid_t` is not one shape on every target - so the new claims
    read a caller-owned 120-byte buffer by offset through `syscall(SYS_statfs)`
    and `syscall(SYS_fstatfs)`, which is the honest way to ask what bytes the
    kernel wrote. Claimed: the root's `f_fsid` is non-zero, procfs's is
    non-zero, two names one instance serves agree, two mounts alive at once
    differ, and `fstatfs` on a descriptor agrees with `statfs` on its path. The
    three raw *reads* are claims of their own, so the block cannot skip
    silently and print green having measured nothing. **Green on a current host
    first (36 of 36, `MPOS_ON_HOST=1`)** and `ok:` for `ok:` byte-identical on
    both machines. Beside them, two host unit tests: `write_statfs` now asserts
    the two halves separately as well as the word they form, and
    `encode_fsid_is_linuxs_device_encoding` pins the encoding against the four
    `MKDEV` rows the C program produced.

    **Counterfactual** - the field left cleared again, which is this item's own
    pre-item body: the host unit test
    `write_statfs_reports_the_superblock_magic_per_filesystem` reddened first
    (`585 passed; 1 failed`), stopping the gate before QEMU ever booted. With
    that one assertion restored to its pre-item form so the guest arm could
    run, x86_64 answered `pass=204 fail=1 (205 built)`, `mpos-kunit 32 passed,
    0 failed`, exit 33, and the one red program was **`t-statfs`** with exactly
    three claims red: "the root filesystem reports a non-zero f_fsid", "procfs
    reports a non-zero f_fsid" and "and two mounts that are both alive report
    different ones". The two *equality* claims stayed green there - zero equals
    zero - and are not claimed as the discriminating pins: what they catch is a
    half-fix, one door handed an identity and the other still cleared.

    **Gate**: x86_64 `pass=205 fail=0 (205 built)`, aarch64 `pass=204 fail=0`
    (204 staged, 1 left out - the machine's own gap), `mpos-kunit 32 passed, 0
    failed` on both; both machines' `t-statfs` `ok:` lines byte-identical (36 of
    36). Host unit suite 586 (was 585), `xtask` 123; `cargo fmt --check` clean;
    clippy's warning set unchanged on both targets; the depgraph ratchet holds
    - the new call is `syscall -> abi`, and `syscall -> vfs::mount` already
    existed.

    **Named**: the *value* is this kernel's device number encoded, not a UUID
    hash, so a caller may not assume `f_fsid` here equals Linux's on the same
    filesystem - the field is promised as an identity, not as a particular
    number. Nothing pins `f_frsize` (still zero, which reads as `f_bsize`) or
    `f_flags`. And the `EBADF` claim in this program no longer borrows a
    hardcoded fd 99 - it mints its own by opening and closing a file, because
    99 was open in the caller's table the first time the host run happened and
    the claim failed there while passing in the guest.

## item 91 — a signal is sent by someone who may send it

    `kill`, `tkill`, `tgkill` and `rt_sigqueueinfo` had no permission rule at
    all: a process that had dropped to another user could signal a root
    process. Linux's question has two halves, `kill_ok_by_cred` and
    `CAP_KILL`, and this kernel asked neither.

    **Measured whole on a current host first** (`.workbuddy/probe_killperm.c`,
    Linux 7.2.11, uid 1000): `kill(1, 0)` is `EPERM`, `kill(self, 0)` is 0,
    `kill(child, 0)` is 0 and `kill(-1, 0)` is 0. The comparison is the
    sender's **real or effective** id against the target's **real or
    saved-set** id - not the target's effective id, which is where a setuid
    program's privilege lives. The record door's own shape was measured in
    the same pass (`.workbuddy/probe_qinfo.c`): `si_code` -1, -2, -7, -8 and
    -100 are accepted and arrive verbatim with the caller's `si_pid`/`si_uid`
    (4242/4242) and `si_value`; 0, +1 and `SI_TKILL` (-6) are `EPERM` to
    another process. The old door overwrote the two identity words with the
    sender's and collapsed every non-`SI_QUEUE` code to `SI_USER`.

    **The landing** is one rule in one place: `crates/cred::kill_ok` (the four
    uid comparisons, host-tested) and `sched::may_signal` (over the task
    table, falling through to `userns::Caller::capable_in` with `CAP_KILL`
    asked of the **target's** user namespace). `kill`/`tkill` ask it for a
    positive target, `kill`'s group branch goes through
    `send_signal_group_checked` (skip members the caller may not signal;
    `EPERM` only when none are permitted), `tgkill` asks after its own
    thread/group checks, and `rt_sigqueueinfo` asks after the code rule and
    now preserves the caller's record through `SigInfo::signed`. The kernel's
    own raises do not ask it: a timer, a child's death and a console handover
    have no userspace sender.

    **Pin**: `t-signal-permission` (new, default group, both machines; host
    run first). Host: uid 1000 against pid 1 and itself. Guest: a 65534 child
    asks its root parent and itself through all three doors (EPERM, 0, EPERM,
    EPERM) and the root parent's `kill(child, 0)` is 0 - only `CAP_KILL` can
    explain that pair; a uid-0 child with the sets `capset`-emptied gets
    `EPERM` on the same 65534 grandchild it could signal before; and a root
    child's `SI_TKILL`/`SI_USER` records to its parent are `EPERM` from the
    code rule. `SIGCONT` is the delivered number on purpose: the uid claims
    must be printable in a counterfactual that removes the rule.

    **Counterfactual** - `may_signal` made to return `true`, every pin kept:
    `t-signal-permission` is the one red program, with exactly four claims
    red - the `kill`, `tgkill` and `rt_sigqueueinfo` `EPERM` claims and
    `without CAP_KILL the same signal is EPERM` - while the root/CAP_KILL
    control and both code-rule refusals stay green.

    **Gate**: x86_64 `pass=193 fail=0`, `mpos-kunit 32 passed, 0 failed`;
    aarch64 `pass=192 fail=0` (192 staged, 14 left out by `tests.toml`),
    `mpos-kunit 32 passed, 0 failed`. Host unit suite 586, xtask 139.

    **Named**: `SIGCONT`'s session clause (Linux also requires the sender and
    target to share a session when the target is in another thread group;
    this kernel has process groups standing in for sessions), and `kill(-1)`
    still means the caller's own group rather than every signalable process -
    the interpretation the door already carried, now filtered by the same
    rule.

## item 92 — a pidfd names a process, not a number

    A pid is a slot index here, and `Table::alloc_slot` hands a freed slot to the
    next tenant, so a handle that stores a bare number names whatever is there
    later. `pidfd_open` closes that hole by recording a generation beside the slot;
    `pidfd_send_signal` and `waitid(P_PIDFD)` consume the descriptor. The three are
    one family, and §2.6 landed the permission rule first because the signal door
    would have been a second way onto it.

    **Measured whole on a current host first** (Linux 7.2.11,
    `.workbuddy/probe_pidfd*.c`, `probe_waitid_pidfd_zombie.c`, `probe_flags.c`,
    `probe_selfsig2.c`). Open: flags are validated before the pid; `O_NONBLOCK` is
    the only accepted flag and `O_CLOEXEC` alone is `EINVAL` even though the
    descriptor is always close-on-exec; `pid <= 0` is `EINVAL`, an unknown number
    `ESRCH`, a non-leader tid `ENOENT`, and pid 1 opens without privilege.
    Descriptor: `F_GETFL` `O_RDWR`/`O_RDWR|O_NONBLOCK` (`0x2`/`0x802`), `fstat`
    mode `0700` with no `S_IF*` bits, size 0, nlink 1, `/proc/self/fd/<n>` reading
    `anon_inode:[pidfd]`, `read`/`pread`/`write` `EINVAL` even at length zero, and
    `lseek` `ESPIPE`. Readiness has two facts: `POLLIN` when the last thread leaves
    the group, `POLLHUP` only after the reap. `pidfd_send_signal`'s order is flags →
    descriptor → record → signal range → existence → authority, and the scopes are
    a **choice** - `0`, `1`, `2`, `4` each alone; `1|2`, `2|4`, `1|4` and anything
    above 4 are `EINVAL` (flags 3, 5, 6, 7, 8 measured; `probe_flags.c`). A zombie
    accepts a send under scopes 0/1/2 and is `ESRCH` under 4 even with a live
    worker; after the reap every scope is `ESRCH`. A NULL record becomes `SI_USER`
    naming the sender; a caller's `SI_QUEUE` record and its ids/value arrive
    verbatim, as do self-targeted `SI_USER`/`SI_TKILL` records with spoofed ids.
    `waitid(P_PIDFD)`: negative id `EINVAL` before any lookup, free/wrong-kind
    descriptor `EBADF`, live non-child `ECHILD`, `O_NONBLOCK` (from open or
    `F_SETFL`) `EAGAIN` on a live child with nothing, explicit `WNOHANG` still the
    cleared zero, zombie reported normally. A dead child under
    `WSTOPPED`/`WCONTINUED` alone is `ECHILD` - for `P_PID` too
    (`probe_waitid_classes.c`).

    **The landing.** `Table::generations` (`[1; MAX_TASKS]`) and
    `next_generation` (`2`) give every slot a tenant number: the reserved init and
    scheduler slots carry 1 without a new edit site, and `alloc_slot` bumps. A
    freed slot keeps the dead tenant's number until reused, so a pidfd reads "gone"
    either way. `vfs::pidfd::PidFd` stores the slot and generation;
    `sched::pidfd_still_names`/`pidfd_group_alive`/`pidfd_task_alive`/
    `pidfd_live_member`/`pidfd_identity` answer the later questions.
    `NodeKind::Anon` is the measured `fstat` shape (`abi::mode_of` gives `0700` and
    no format bits; `dt_type_of` `DT_UNKNOWN`), `File::read` skips its zero-length
    short-circuit for the kind, `Inode::fd_name` supplies
    `anon_inode:[pidfd]`, and `fsync` is `EINVAL`. `waitid` resolves `P_PIDFD`
    through the caller's table, reads `O_NONBLOCK` off the file description, and
    threads `WEXITED` through the scan/wait/poll/`has_matching_children` chain (the
    pre-existing `P_PID` class bug closes with it). `send_signal_group_info` is the
    one group door that carries a caller-authored record through the fan-out,
    applying §2.6's `may_signal` per member.

    **The first sweep caught the flags rule.** Three `pidfd_send_signal` flag
    claims were red in the guest while the host run was green - the host measures
    Linux, not this kernel. The handler tested `flags & !SCOPES`, which accepts
    every combination of known bits; Linux's rule is the four values. The probe
    above settled it, the handler matches `0 | 1 | 2 | 4` now, and the restored
    mask is this item's counterfactual: guest red, host still green.

    **Pins**: `t-pidfd` (new, default group, both machines) - 100 claims, the `ok:`
    text identical on both (the first machine's console writes CRLF) - covering the
    descriptor shape and refusals, the non-leader `ENOENT`, pid 1's `ECHILD`, the
    live/zombie/reaped lifecycle, epoll, the records and all four scopes,
    non-blocking waits, `F_SETFL`, and the verbatim self-target. `t-waitid`'s `P_PIDFD` claim moved to the measured
    `EBADF`/negative-`EINVAL` pair; host tests live in `vfs::pidfd` and in
    `abi::tests` (the type-less `st_mode`, beside `mode_of`).

    **Gate**: x86_64 `pass=194 fail=0` (194 built, 6 lanes), aarch64 `193 staged,
    pass=193 fail=0` (14 left out by `tests.toml`), `mpos-kunit 32 passed, 0
    failed` on both; host unit suite 589, `xtask` 139; `cargo fmt --check` clean;
    the depgraph ratchet holds (`vfs -> sched` and `syscall -> vfs` are both
    recorded edges).

    **Named**: `pidfd_getfd(2)` (438), `clone3`'s `CLONE_PIDFD` and
    `process_madvise(2)` (440) are not served and fall to the `ENOSYS` catch-all.
    **`pidfd_getfd` was served by item 93**, below.
    The older anonymous descriptors' `S_IFCHR` divergence landed as item 99,
    below. `st_blksize` stays 512 where Linux's pidfd answers
    4096 (the global choice, not a pidfd one), and `st_dev`/`st_ino`/uid/gid are
    machine-specific and not pinned. `fdinfo` for a pidfd (`Pid:`/`NSpid:`) was not
    measured and is not promised.

## item 93 — a fetched descriptor is the same open file, not a copy of it

    A pidfd names a process; Linux's fourth door on that name also reaches in
    and takes one of the process's descriptors. `pidfd_getfd(438)` duplicates
    the **open file description**, not the descriptor: two fetches of one
    target share one offset, and closing one leaves the other - the same
    relation `dup` has, across a process boundary.

    **Measured whole on a current host first** (Linux 7.2.11,
    `.workbuddy/probe_pidfd_getfd{,2}.c`; the first probe's live child was a
    mistake worth keeping in the record - one pipe served both directions, so
    the parent's close ended the child's read and every fetch answered `ESRCH`
    until a second pipe kept it alive). **The order** is flags → pidfd
    descriptor → task → authority → target fd, not the argument order: a
    non-zero flags word is `EINVAL` even beside `pidfd = -1` (flags `1` and `2`
    measured); a number naming nothing or a valid non-pidfd descriptor is
    `EBADF`; a reaped process, a zombie, and a leader that exited under a live
    worker are all `ESRCH`; a non-dumpable same-uid target is `EPERM`, and the
    authority question comes *before* the target fd, so a non-dumpable target
    with fd `999999` answers `EPERM` rather than `EBADF`; only then is a missing
    target fd `EBADF` (`-1` and `999999` measured). **The descriptor**: the
    number is the lowest free one, `F_GETFD` carries `FD_CLOEXEC`
    unconditionally, `F_GETFL` carries the target's access mode, and two
    fetches of one target share an offset - read three bytes through the first
    and the second's position is also three, then read two through the second
    and get bytes four and five. **What is enough**: a live, same-uid process,
    with no parent relation; a process always fetches its own; a fetched pidfd
    is still a pidfd (`poll` and `pidfd_send_signal` work through it).

    **The landing.** `nr_x86_64.rs`/`nr_aarch64.rs` name `PIDFD_GETFD = 438`,
    and `proc::handle_pidfd_getfd` resolves the pidfd through the caller's
    table, checks the generation with `sched::pidfd_still_names`, asks
    `sched::may_inspect` (the §2.6 `ptrace_may_access` rule), and takes the
    target's table from the new `sched::pidfd_fd_table` - which answers `None`
    for a reaped slot, a reused generation, or a task that has exited, the
    three `ESRCH`s above. The description comes from `FdTable::clone_at` and
    lands with `current_fds().install`, the lowest-free slot the fd contract
    already promises, then `fd_flags_set(..., FD_CLOEXEC)` spends the one flag
    the door never asks about.

    **The guest caught a measurement that did not transfer.** The host run is
    uid 1000 and measured `EPERM` on a non-dumpable target in the main process.
    The guest runs the test as root, and root holds `CAP_SYS_PTRACE`, so
    `ptrace_may_access` grants the fetch before the dumpable flag is read - the
    EPERM claims were red on the first sweep while the host stayed green. The
    pin moved to a fixture where a target and a caller both drop to uid 65534
    (the drop is attempted and may fail on a host that is already
    unprivileged, in which case the same uid is already the shape the rule
    needs). The rule is Linux's; the number is now measured on the machine that
    serves it.

    **Pins**: `t-pidfd` 100 → **121 claims** on both machines (the `ok:` text
    identical on the two), covering the order and its five errnos, the shared
    description, `FD_CLOEXEC`, the access mode, a fetched pidfd, the
    unprivileged authority pair, a process's own descriptor, and a zombie's
    `ESRCH` before and after the reap. No host test: the scheduler's pidfd
    helpers live behind `target_os = "none"`, and the ABI claims are the
    program's.

    **Gate**: x86_64 `pass=194 fail=0` (194 built, 6 lanes), aarch64 `193
    staged, pass=193 fail=0` (14 left out by `tests.toml`), `mpos-kunit 32
    passed, 0 failed` on both; host unit suite unchanged, `xtask` 139; `cargo
    fmt --check` clean; depgraph holds (`syscall -> sched` was already an edge).

    **Named**: `clone3`'s `CLONE_PIDFD` and `process_madvise(2)` (440) remain
    `ENOSYS`. `pidfd_getfd` is the one door that reaches another process's
    descriptor table; `pidfd_getfd`'s own `fdinfo` is still not promised, and
    the older anonymous descriptors' `S_IFCHR` divergence landed as item 99.
    The authority rule keeps the known §2.6 divergence: Linux's
    `PTRACE_MODE_ATTACH_REALCREDS` compares group ids too and this kernel
    compares the real/effective uid set; every task is root on this image, so
    nothing in the gate can tell them apart.

## item 94 — the extensible `clone` door, and the one that names its own child

    `clone(2)` is a 32-bit flag word with an exit signal in its low byte, five
    positional arguments and no room for anything new. `clone3(435)` replaces
    all of that with one versioned `struct clone_args`, and the reason it is on
    this list is its `CLONE_PIDFD`: the process and a descriptor naming it come
    back in the same call, so there is no window between a returned pid and a
    later `pidfd_open` in which that number could be handed to another tenant -
    the hole item 92 closed for a number already held, closed for the creation
    itself.

    **Measured whole on a current host first** (`.workbuddy/probe_clone3{,b,c,d,e,f,g,h,i}.c`,
    Linux 7.2.11; the numbered list is the order the probes were run, and two of
    them were wrong in ways worth keeping). **The size rules**: a `size` below
    `CLONE_ARGS_SIZE_VER0` (64) is `EINVAL` *before* the pointer is read (a bogus
    pointer with `size = 8` is `EINVAL`, with `size = 64` `EFAULT`), the words
    between `size` and `VER2` (88) read as zero, and a `size` past `VER2` is
    allowed only while every byte past it is zero (`E2BIG` otherwise). **The
    stack rules**: `stack` is the *base* of the region and the child starts at
    `stack + stack_size` - not `clone(2)`'s "the stack pointer itself" - both
    fields set together or neither, and neither set means the child inherits the
    caller's stack pointer. **The exit signal** is a field of its own, 0 through
    `SIGRTMAX` (64); 65 is `EINVAL`. **The flags**: `CLONE_SIGHAND` without
    `CLONE_VM` and `CLONE_THREAD` without both are `EINVAL`; `CLONE_CHILD_SETTID`
    is served and needs no pointer (alone, beside `CLONE_VM`, with a real
    `child_tid` or with none); `CLONE_PARENT_SETTID` needs none either, and a
    destination the caller cannot write is *skipped*; `CLONE_DETACHED` -
    `clone(2)`'s historical no-op - is `EINVAL`; an unknown bit is `EINVAL`
    (Linux's own `clone3` check, unlike `clone(2)`'s absence of one);
    `CLONE_PIDFD` with `pidfd = 0` is `EFAULT` and two writes aimed at one
    address (`pidfd == parent_tid`) are `EINVAL`, while different destinations
    are served; `CLONE_INTO_CGROUP` is `EBADF`; `set_tid`/`set_tid_size` must be
    both set or both zero, a size above one is `EINVAL`, and one well-formed
    entry needs a new pid namespace (this kernel refuses it `EOPNOTSUPP`, Linux
    would serve it).

    **Two probe mistakes are worth their lines.** The first draft of this item
    was measured with `CLONE_CHILD_SETTID` spelled `0x400000`, which is
    `CLONE_DETACHED` - so "clone3 rejects `CLONE_CHILD_SETTID`" was a rule about
    the wrong bit, and the pin's own host run caught it (the corrected probe is
    `probe_clone3h.c`). The second: a probe whose child returned 0 and carried on
    through `main` rather than branching to an exit forked a tree of copies of
    itself, which is how a `clone3` fixture teaches the difference between "the
    child returns from the call" and "the child is a fresh function call".

    **The landing.** `nr_x86_64.rs`/`nr_aarch64.rs` name `CLONE3 = 435`; the
    argument struct's decode and every rule above sit in
    `abi::clone3_read_len`/`clone3_tail_is_zero`/`clone3_verdict` (host-compiled,
    host-tested); `proc::handle_clone3` reads the struct, asks the verdict, builds
    the same `CloneRequest` the legacy door builds and hands it to
    `sched::clone`; `pidfd_for_task` - the one place a pidfd is built, now shared
    with `pidfd_open` - mints `CLONE_PIDFD`'s descriptor, and a child whose
    descriptor cannot be delivered is killed rather than left without a name.
    `crates/abi` gains `CLONE_PIDFD`, `CLONE_CLEAR_SIGHAND` and
    `CLONE_INTO_CGROUP` (the three bits a 32-bit `clone` word cannot carry).

    **Pins**: **`t-clone3`**, a new program, **37 claims**, green on a current
    host first and the 37 `ok:` lines byte-identical on both machines - the
    delivered descriptor (close-on-exec, `O_RDWR`, naming the child while it is
    alive, `ESRCH` after the reap, and a *different* number from a later
    `pidfd_open`), the size discipline, the flags, the exit signal, the stack
    pair with a child that really reports from the top of its own mapping, and
    `CLONE_CHILD_SETTID` read back by the child itself. Beside them, the
    host-tested leaf: `clone3_args_are_validated_in_linuxs_order` and the three
    new `mpos-abi` constant assertions. The counterfactual (the dispatch arm
    removed, so `clone3` falls to `_ => ENOSYS`) is all **36 claims red** at the
    time of that run and no other program red.

    **Gate**: x86_64 `pass=195 fail=0` (195 built, 6 lanes), aarch64 `194 staged,
    pass=194 fail=0` (14 left out by `tests.toml`), `mpos-kunit 32 passed, 0
    failed` on both; host unit suite 590, `mpos-abi` 5, `xtask` 139; `cargo fmt
    --check` clean; clippy 0 errors on both bare-metal targets; depgraph holds.

    **Named**: `process_madvise(2)` (440) is the pidfd family's last `ENOSYS`
    (probed and deferred: its only real advice, `MADV_DONTNEED`, is refused
    `EINVAL` for another process on this host, and the rest are hints this kernel
    answers as no-ops, so a door here would be a shape without work behind it).
    `CLONE_CLEAR_SIGHAND` was `EOPNOTSUPP` at this landing because `fork` here
    started every process at `SIG_DFL` (a divergence of its own, not inherited
    from the parent as POSIX requires), so accepting the bit could not be told
    from ignoring it. **Item 96** closes that prerequisite and serves the flag.
    `CLONE_NEWTIME` through this door is `EOPNOTSUPP` (this
    kernel has no clock namespace to hand a child through `clone`; `unshare` is
    the door that serves it). `CLONE_PTRACE` and any unknown bit are `EINVAL`
    (this kernel's designed unknown-bit rule, which `clone(2)` shares). `clone3`
    carries no `CLONE_VM`-without-`CLONE_THREAD` sharing here - a copy marks the
    contract better, as `vfork`'s own note says - so a caller relying on that
    word's shared memory is a divergence named rather than served.

## item 95 — a peer's close is a hang-up, and a message socket's end-of-file carries it

    A connected `AF_UNIX` end whose peer closed reported **nothing** here on a
    `SOCK_SEQPACKET` connection, and a named `SOCK_STREAM` end reported readable
    without the hang-up its `socketpair` twin had. Linux reports both as
    `POLLIN|POLLHUP` (`EPOLLIN|EPOLLHUP` through epoll) and answers the receive
    after the queued messages with 0. The boot this cost: OpenRC's udev-settle
    runs `udevadm settle`, which pings udevd over a connected seqpacket control
    socket and polls it; udevd closes, Linux returns at once, and this kernel's
    client slept out the **120-second** settle default at
    `* Waiting for uevents to be processed ...`.

    **Measured whole on a current host first** (`.workbuddy/probe_unix_close.c`
    and `.workbuddy/probe_seq_send_close.c`, Linux 7.2.11): `socketpair` and
    named ends, seqpacket and stream alike,
    answer poll `0x011`, epoll `0x11`, `recv` 0, and a seqpacket `send` `EPIPE`;
    a **datagram** `poll(POLLIN|POLLOUT)` answers `POLLOUT` alone (`0x004`),
    `recv` `EAGAIN` and `send` `ECONNREFUSED`, because its peer can be replaced
    by another `connect` (`.workbuddy/probe_dgram_close.c`). Measured in the
    guest before the fix (`unixcloseprobe.c`): both seqpacket shapes `poll=0`
    `revents=0x000` and `recv=-1 EAGAIN`; the named stream `0x001` against the
    pair's `0x011`.

    **The landing.** A connected seqpacket socket's existing weak peer reference
    now answers `message_peer_gone`; `take_dgram`/`peek_dgram` drain the queue
    first and then answer `Ok(None)` (rendered as 0 by `read_at`),
    `wait_for_dgram`'s condition includes it, `poll_ready` reports readable when
    queued *or* gone, and `poll_hup` answers it - with streams forwarding to the
    wrapped `DuplexEnd` so the named end matches its pair. `Drop` takes the
    peer's `read_waiters` under the critical section that clears the slot and
    wakes them after the borrows are released.

    **Pins**: **`t-unix-close`** (new, default group, both machines), 19 claims -
    the queued-message-then-0 order, the poll/epoll shapes on a named pair, a
    `socketpair` end and a named stream, two timed wakes (a child closes 100 ms
    into a 5 s wait) and the datagram contrast. Green on a current host first;
    in the guest the wake measures 99 ms (`poll`) and 97 ms (`epoll_wait`).
    Counterfactual (the gone-peer predicate removed from the take, peek, wait,
    readiness and `poll_hup`): **7 of 19 red** by name, the wakes answering at
    their 4997/5000 ms deadlines, every datagram and message-ordering claim
    green. Gate: x86_64 stage 1 1073/0, stage 2 `pass=196 fail=0`,
    `mpos-kunit 32 passed, 0 failed`. The image now reaches `mpos login:` 3.7 s
    after QEMU starts where the settle used to own 120 s of the boot.

    **Named**: the epoll return carries this kernel's usual extra
    `EPOLLRDNORM` bit (`0x51` where Linux answers `0x11`); the claim asks
    `EPOLLIN|EPOLLHUP`, and the extra bit predates this item. **The send side
    of the same close** is item 97, and measuring it corrected this item's
    first draft: a seqpacket `send` to the dead peer answers `EPIPE` **without**
    `SIGPIPE`, a stream's answers `EPIPE` **with** it, and a datagram's is
    `ECONNREFUSED` once and then `ENOTCONN`.

## item 96 — a child inherits its parent's signal table, and `exec` resets only the caught slots

    A `fork`/`clone` child here started at `SIG_DFL` whatever the parent held,
    where POSIX and Linux copy the table - the reason item 94 refused
    `CLONE_CLEAR_SIGHAND` `EOPNOTSUPP`: the flagged and unflagged child were
    identical. The other half of the same state was wrong too: `exec` reset
    *every* disposition (an ignored signal has to stay ignored) and flushed the
    pending set (Linux keeps it).

    **Measured whole on a current host first** (`.workbuddy/probe_sig_inherit.c`
    and `.workbuddy/probe_altstack_fork.c`, Linux 7.2.11): a `fork` child
    carries the parent's handler, `SIG_IGN`, blocked mask **and alternate
    stack**; after an `exec` a caught handler reads `SIG_DFL`, `SIG_IGN` stays,
    the blocked mask and a pending blocked signal cross, and the old alternate
    stack is gone; `clone3` without the flag hands the handler to the child and
    with `CLONE_CLEAR_SIGHAND` hands `SIG_DFL`; `CLEAR|SIGHAND` is `EINVAL`
    (with `CLONE_VM` or without), and so is the pair with `CLONE_THREAD`; a
    thread's `rt_sigaction` is the process's.

    **The landing.** The fork/clone child copies `parent.dispositions` unless
    `CLONE_CLEAR_SIGHAND` asks for the default table; the alternate stack
    crosses; `clone3_verdict` serves the flag and refuses the `CLONE_SIGHAND`
    pair; `exec` calls `Actions::reset_caught` and leaves the pending set and
    the blocked mask alone.

    **Pins**: **`t-signal-inherit`** (new, default group, both machines), 12
    claims - the fork inheritance and a real `execve` of the test's own image
    reporting DFL/IGN/mask/pending through a pipe; **`t-clone3`** 37 -> 40
    claims - the plain child's handler, the flagged child's `SIG_DFL`, and both
    `EINVAL` pairs. `t-signal-frame`'s fatal-fault child now resets `SIG_DFL`
    itself: it had relied on the old divergence after installing a `SIGSEGV`
    handler, and Linux's fork hands that handler down. The first gate run
    measured exactly that (`pass=196 fail=1`, `status 0x0`). Counterfactual
    (the fork fields and the exec reset put back), every pin kept: a guest
    single-test run of `t-signal-inherit` is **4 of 12 red** - the fork
    inheritance, the kept `SIG_IGN`, the kept pending signal and the whole
    after-exec word; the DFL and mask claims stay green.

    **The release stack this needed.** With the inheritance in place the
    aarch64 release sweep crashed at the first user `fork`; `spawn_copy`'s
    frame is ~13.8 KiB of a 16 KiB release stack and its callees no longer fit.
    The isolation was measured: reverting only the fork fields booted, while
    only the 16-byte alternate-stack copy in place crashed two ways on two
    boots (`free of 0xe000: address is not in any zone`; then a branch to
    address 0). The release stack is 32 KiB now (`STACK_ORDER` 3, debug stays
    64 KiB) and the frame measurement is in `src/sched/runtime/mod.rs`.

    **Gate**: x86_64 stage 1 1074/0, stage 2 `pass=197 fail=0`, `mpos-kunit 32
    passed, 0 failed`, exit 0; aarch64 196 staged, `pass=196 fail=0`,
    `mpos-kunit 32 passed, 0 failed`, exit 0. The reasoning and the counterfactual
    detail are §2.7.

    **Named**: a thread's table is still a copy, not Linux's shared
    `CLONE_SIGHAND` table; the kernel-spawned program path (`load.rs`) still
    starts at `SIG_DFL`, which is not a user `exec` and is not claimed as one.

## item 97 — a write to a broken channel is `EPIPE`, and the signal behind it

    §4.1.12 made a peer's close visible to a **reader**; this is the sender's
    half, and it was wrong in three ways. A seqpacket `send` to the dead peer
    answered `ENOTCONN`; a stream's write answered `EPIPE` but raised no
    `SIGPIPE`; and no broken channel in this kernel raised one at all, so a
    pipe's write returned `EPIPE` to a program Linux would have killed. The
    signal's generation is one primitive now (`sched::raise_sigpipe`, an
    `SI_USER` record through the same `send_signal` a `kill` uses), and each
    door raises it where Linux does.

    **Measured whole on a current host first** (`.workbuddy/probe_sigpipe.c`,
    `probe_sigpipe2.c`, `probe_sigpipe3.c`, `probe_sigpipe4.c`, Linux 7.2.11):

    | the broken channel | the call | handler runs? |
    |---|---|---|
    | a pipe with no reader | `EPIPE` | **yes**, before the call returns |
    | a `SOCK_STREAM` pair whose peer closed | `EPIPE` | **yes**, every call |
    | `send` on a listener | `EPIPE` | **yes** |
    | a reset stream's **second** write | `EPIPE` | **yes** (the first is `ECONNRESET`) |
    | a `SOCK_SEQPACKET` pair whose peer closed | `EPIPE` | **no** |
    | a connected `SOCK_DGRAM` whose peer closed | `ECONNREFUSED`, once | no |
    | a socket that never connected | `ENOTCONN` | no |

    The datagram row's second half is the subtle one: the first failing send
    **forgets the connection**, so the next call is `ENOTCONN`. A seqpacket
    connection is remembered and answers `EPIPE` again. The handler's record
    is `SI_USER` (`si_code == 0`). `MSG_NOSIGNAL` keeps the `EPIPE` and
    suppresses only the signal; a blocked signal is pending while the call
    returns `EPIPE` and runs on unblocking; `writev`, `splice` and `sendfile`
    into a broken pipe each raise once; a **partial** write answers its count,
    and a pipe raises for the break whereas a stream does not (57344 with the
    handler run against 219264 silent, same race).

    **The landing.** `sched::raise_sigpipe`; raises at `write`/`writev`/
    `splice`/`sendfile` on `VfsError::BrokenPipe`, at the channel and IP arms
    of `sendto`/`sendmsg` with `MSG_NOSIGNAL` threaded through, and the
    seqpacket exemption asked of the socket (`UnixSock::silent_broken_pipe`).
    `UnixSock::message_peer` answers the datagram's `ECONNREFUSED`-once and
    forgets the slot; `pair_ends` carries the pipe/stream partial-write split.

    **Pins**: **`t-sigpipe`** (new, default group, both machines), **32
    claims** - the rows above with their handler counts and `si_code`, the
    `MSG_NOSIGNAL` pair, the blocked sequence, the three copy calls, and a
    forked child at `SIG_DFL` that dies of signal 13. Green on a current host
    first and the same in the guest on both machines. Counterfactual (the
    primitive a no-op): **10 of 32 red** by name, every one a signal claim;
    all errno claims stayed green.

    **Gate**: x86_64 198 programs, `pass=198 fail=0`; aarch64 197 staged,
    `pass=197 fail=0`; `mpos-kunit 32 passed, 0 failed` on both. Reasoning and
    the measurement tables are §4.1.13.

    **Named**: a TCP reset's first write is `ECONNRESET` on Linux and `EPIPE`
    here (smoltcp clears the tuple, so the first/second distinction is not
    kept); the signal follows this kernel's errno. vsock's `EPIPE` is
    unchanged and its signal half unmeasured - there is no host device to
    measure against.

## item 98 — a stream's receive buffer is its window, and a listener hands it to the socket it accepts

    Item 58 made `SO_RCVBUF` Linux's *number*; the number governed nothing on a
    stream. Linux copies the listener's `sk_rcvbuf` onto an accepted socket
    (`inet_csk_clone_lock`) and sizes the receive window from it, so a caller
    that tunes a listener is tuning the connection that arrives. This kernel kept
    the number on the listening socket and built every accepted connection with
    the default 64 KiB ring.

    **Measured whole on a current host first** (`.workbuddy/probe_tcp_rcvbuf.c`,
    Linux 7.2.11): a server that never reads, a client whose send buffer is
    4096, the listener's option set before or after `listen` (the same answer):

    | listener asks | accepted socket reads | unread bytes absorbed |
    |---|---|---|
    | 2048 | 4096 | 8192 |
    | 32768 | 65536 | 47104 |
    | 262144 | 524288 | 212886 |
    | (unset) | 131072 | — |

    The stored figure is twice the ask (item 58) and the effective capacity is
    about half the stored figure; the inheritance is the option's, not the
    connection's, so `getsockopt` on the accepted socket reports the listener's
    number.

    **The landing.** `net::tcp_rx_bytes(stored)` = half the stored figure
    (floored at 1, capped at `TCP_RX_MAX` = 128 KiB); `add_tcp_sized` builds a
    socket with that receive ring; `Listener::rx_bytes` carries it through the
    pool and its refills; `set_listener_rx` rebuilds the pool's `Listen`
    sockets when the option changes after `listen` (a `SynReceived` handshake
    stays where it is); `settle_tcp` carries it when a socket is relocated to
    the loopback; and `accept` copies the stored number into the accepted
    socket's `SockOpts`.

    **The pin found a send bug.** `tcp_send` fell through to its blocking park
    when `send_slice` queued only *part* of the caller's buffer, even for a
    `MSG_DONTWAIT` send - it gated only the "queued nothing" case. Against a
    2048-byte window and a peer that never reads, the test parked forever (gdb
    on the booted machine: `tcp_send`'s `park_until`, deadline `u64::MAX`,
    scheduler idle). Linux's `tcp_sendmsg` returns the bytes it copied and
    answers `EAGAIN` only when none fit; the partial arm now returns `sent` on a
    non-blocking send. That fix is part of this item's pin even though it is not
    part of the window: the window would never be observable without it.

    **Pin**: `t-sockbuf` grown to **18 claims** - inheritance before `listen`
    (ask 2048 → accepted 4096) and after it (32768 → 65536), then three sized
    listeners pushed until they refuse (asks 2048 / 32768 / 262144) with two
    relative claims (`large > small * 2`, `beyond > large`). Guest:
    **18432 / 49152 / 147456**; host: 8192 / 47104 / 286561. Green on the host
    first and in the guest on both machines. Counterfactual (the ring pinned
    back to the default, the send fix kept): the two inheritance claims stay
    green and both effect claims go red, all three capacities collapsing to
    81920 (64 KiB default ring plus the 16 KiB client queue). The number was
    already Linux's; the window was not.

    **Gate**: x86_64 198 programs, `pass=198 fail=0`; aarch64 197 staged,
    `pass=197 fail=0`; `mpos-kunit 32 passed, 0 failed` on both. The production
    boot reaches `mpos login:` with every OpenRC service `[ ok ]`.

    **Named**: a datagram's receive ring is still fixed (16 KiB), so `SO_RCVBUF`
    there remains a report (item 58); a connected stream's own
    `setsockopt(SO_RCVBUF)` changes the number without resizing the live ring
    (smoltcp's buffers are fixed at construction); a handshake already in
    `SynReceived` when the option changes keeps the ring it started with; the
    ring is capped at
    128 KiB per socket so a sysctl-sized ask cannot make eager mebibytes per
    socket under §8.0's panicking allocator, while the number still reads back
    Linux's; and a stream's `SO_SNDBUF` does not size its send ring
    (`TCP_TX_BYTES` is fixed). Reasoning and tables: `docs/notes/` §4.3.3.

## item 99 — an anonymous descriptor has no file type, and its `/proc` name is its own

    `eventfd`, `timerfd`, `signalfd`, `epoll`, `inotify` and `pidfd` are
    descriptors, not files: Linux puts them on its `anon_inode` filesystem,
    where `st_mode` has **no** `S_IF*` bit at all. This kernel reported the
    first five as `S_IFCHR`, borrowing the wrong one of the two shapes it had
    (`NodeKind::CharDev`), and named them `anon:[<ino>]` in `/proc/<pid>/fd`
    because only the pidfd had a spelling of its own.

    **Measured whole on a current host first**
    (`.workbuddy/probe_anon_fd_stat.c`, `.workbuddy/probe_anon_read0.c`,
    `.workbuddy/probe_inotify0{,b}.c`, Linux 7.2.11):

    | descriptor | `st_mode` | `/proc/self/fd/<n>` |
    |---|---|---|
    | `eventfd` | `000600` | `anon_inode:[eventfd]` |
    | `timerfd` | `000600` | `anon_inode:[timerfd]` |
    | `signalfd` | `000600` | `anon_inode:[signalfd]` |
    | `epoll` | `000600` | `anon_inode:[eventpoll]` |
    | `inotify` | `000600` | `anon_inode:inotify` (no brackets) |
    | `pidfd` | `000700` | `anon_inode:[pidfd]` |

    The type bits are zero in every row, and the permission word is not one
    number for the family - which is why the kind carries it. A zero-length
    read is the same "what is this" question through the read door, and its
    order is the queue's first for one member only: `eventfd`, `timerfd`,
    `signalfd`, `epoll` and `pidfd` answer `EINVAL` before looking at
    anything; a non-blocking `inotify` answers `EAGAIN` while empty and
    `EINVAL` once a record is queued (a blocking one waits and then answers
    `EINVAL`).

    **The landing.** `NodeKind::Anon { mode }` carries the permission word
    (the type is always zero), and all six inodes now use it - `0600` for the
    five, `0700` for the pidfd. Each of the five implements `fd_name`, which
    `File::display_name` already consulted before its `anon:[ino]` fallback.
    `InotifyInstance::read_at` answers the zero-length case in Linux's order:
    queued → `EINVAL`, empty and non-blocking → `EAGAIN`, empty and blocking →
    wait, then `EINVAL`.

    **Pin**: the new `t-anon-fd`, **26 claims** - the three shape answers for
    each of the six, the seven zero-length-read answers, and a path-opened
    file's link as the control that an anon name is not simply returned for
    everything. Green on a current host first, with the same `ok:` text in
    the guest on both machines. One consequence rides with the kind: `File::read`'s
    zero-length short-circuit exempts `Anon` (the pidfd already needed that, its
    read method being absent), so the five now reach their own `read_at` with
    an empty buffer - which is exactly where their `EINVAL` lives. On the old
    `CharDev` kind the short-circuit answered `0` for all five.

    **Gate**: x86_64 199 programs, `pass=199 fail=0`; aarch64 198 staged,
    `pass=198 fail=0`; `mpos-kunit 32 passed, 0 failed` on both.

    **Named**: `st_blksize` stays this kernel's 512 where Linux answers 4096
    (the global choice, not an anon-inode one), and `st_dev`/`st_ino`/uid/gid
    are machine-specific and not pinned. A pidfd's `fdinfo`
    (`Pid:`/`NSpid:`) is still not measured and not promised.

## item 100 — POSIX message queues are a family, not a stub: `ENOSYS` was the honest answer to a kernel with no queues at all

    The six calls (`mq_open` 240, `mq_unlink` 241, `mq_timedsend` 242,
    `mq_timedreceive` 243, `mq_notify` 244, `mq_getsetattr` 245) fell through the
    dispatch to `ENOSYS`, and that was the right answer for a kernel with nothing
    behind them - `unshare(CLONE_NEWIPC)` was refused for the same reason, since a
    namespace with nothing to isolate isolates nothing (§6.3). This is the family
    served, measured on a current host first (Linux 7.2.11,
    `.workbuddy/probe_mqueue{,2}.c` and `probe_mqueue_names.c`, whose raw-syscall
    half is what separated the *kernel's* name rule from the libc's) and recorded
    with its evidence in §6.12. The names are the object identity (`unlink`
    retires the name and the descriptors keep the queue), messages come out
    highest priority first and FIFO within one, a full queue's non-blocking send
    and an empty queue's non-blocking receive are both `EAGAIN`, an expired
    absolute `CLOCK_REALTIME` deadline is `ETIMEDOUT`, the access mode is the
    *descriptor's* (`mq_send` on `O_RDONLY` is `EBADF`), one notification
    registration per queue (`EBUSY` on a second, fired by the send that finds the
    queue empty, `SI_MESGQ` with its `sigval`), and a queue is pollable and
    epoll-watchable (`POLLOUT` empty, `POLLIN|POLLOUT` with a message, `POLLIN`
    full).

    **Owner**: `src/vfs/mqueue.rs` (the objects: the name registry, the priority
    insert, the one registration slot, the per-uid `RLIMIT_MSGQUEUE` charge) and
    `src/syscall/linux/ipc.rs` (the six doors: the name rules, the attribute
    block, the absolute-timeout conversion and the park). `epoll.c`'s watchability
    gate gained one line: a queue reports `S_IFREG` and is nonetheless watchable,
    which is what Linux's mqueue filesystem says too.

    **Pin**: the new `t-mqueue`, **41 claims** (libc's `mq_*` wrappers, because
    `rustix` binds none of the family). Green on a current host first.

    **Gate**: x86_64 `pass=201 fail=0 (214 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 0, three consecutive runs. The second machine compiles the
    change; its sweep could not boot on this host (no DRM render node), so no
    aarch64 figure is claimed.

    **Named**: `CLONE_NEWIPC` is still refused - its isolation is SysV objects
    *and* POSIX queues, and only the second half exists now. There is no
    `/dev/mqueue` (no mqueuefs), so a queue's name is its only spelling and the
    descriptor's `st_size` is 0 where Linux counts the filesystem entry's 80
    bytes. The `msg_max`/`msgsize_max` ceilings are constants, not sysctls, and a
    blocked sender/receiver is pinned only by the timeout side.

## item 101 — System V shared memory: four calls, one object model, and the number scheme the other two families will share

    `shmget` (29/194), `shmat` (30/196), `shmctl` (31/195) and `shmdt` (67/197)
    fell through the dispatch to `ENOSYS` - the first third of the "SysV IPC
    objects" that `CLONE_NEWIPC` had been refused for since §6.3. Measured on a
    current host first (Linux 7.2.11, `.workbuddy/probe_sysv_shm.c`,
    `probe_shm_perm.c`, `probe_shm_ids.c`, `probe_shm_rmid.c`,
    `probe_shm_order.c`), recorded with the evidence in §6.13. The shapes that
    are Linux's and are now served: a segment's reported size is the size *asked
    for* while its mapping is whole pages; `shmget` applies **no umask**; a
    segment's number is `slot + generation * 32768`, so a number retained across
    the segment's removal is `EINVAL` and never names its replacement; the errno
    order on a live key is `EEXIST`, `EINVAL` (a larger size), `EACCES` (only
    when `shmflg` names permission bits); an explicit attach address is taken
    only when free and a busy one is `EINVAL` rather than relocated, `SHM_RND`
    rounds down, `SHM_REMAP` replaces; `shm_nattch` counts address spaces, so a
    `fork` raises it and the child's exit lowers it; `IPC_RMID` *marks* a segment
    - its number keeps answering until the last detach, its key reads back as
    `IPC_PRIVATE`, and a `shmget` on that key makes a different segment; the three
    permission refusals are `EACCES` (`shmat`, `IPC_STAT`) and `EPERM`
    (`IPC_SET`, `IPC_RMID`); and `/proc/sysvipc/shm` prints the header and column
    widths `ipcs` parses.

    **Owner**: `src/mm/sysv_shm.rs` (the segment table, the lazily-allocated
    frames behind a `MappedObject`, the marking lifetime) and `src/mm/ipc_id.rs`
    (the slot-and-generation arithmetic, shared with the two families still to
    come), `src/syscall/linux/ipc.rs` (the four doors, the permission model,
    `IPC_INFO`/`SHM_INFO`/`SHM_STAT`), `src/vfs/procfs.rs` (`/proc/sysvipc/shm`),
    and `src/sched/runtime/child_mm.rs` (the attachment fold,
    `shm_attachment_count`, which is what makes `fork` count and a split mapping
    count once).

    **Pin**: the new `t-sysv-shm`, **61 claims** (`libc` for the family -
    `rustix` binds no System V IPC call - and the three `shmctl` records spelled
    `#[repr(C)]` because the layout is the ABI). Green on a current host first.

    **Gate**: x86_64 `pass=202 fail=0 (214 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 0, two consecutive runs. The second machine compiles the change
    (kernel release + clippy, and the pin builds for it); its sweep could not boot
    on this host (no DRM render node), so no aarch64 figure is claimed.

    **Named**: the other two SysV families (semaphores and message queues) are
    still `ENOSYS`, so `CLONE_NEWIPC` is still refused with its reason narrowed
    to them. The table is the *machine's* rather than a namespace's, `SHM_LOCK`
    only sets a mode bit (there is no swap to hold a page out of), `shm_rss` is
    the pages that have a frame, and `SHMMAX` (64 MiB) / `SHMALL` (32768 pages) /
    `SHMMNI` (4096) are constants this kernel reports through `IPC_INFO`, not
    sysctls.

## item 102 — System V semaphores, and the undo list that makes `CLONE_SYSVSEM` mean something

    `semget` (64/190), `semop` (65/193), `semctl` (66/191) and `semtimedop`
    (220/192) fell through the dispatch to `ENOSYS` - SysV's second family, and
    with it the `SEM_UNDO` mechanism that `CLONE_SYSVSEM` had been accepted as a
    no-op for want of. Measured on a current host first (Linux 7.2.11,
    `.workbuddy/probe_sysv_sem.c`, `probe_sem2.c`, `probe_sem3.c`,
    `probe_sem_unshare.c`, `probe_ipc_order.c`), recorded with the evidence in
    §6.14. The shapes that are Linux's and are now served: `semtimedop`'s
    timeout is **relative** and expiry is `EAGAIN`; an operation array is atomic,
    `sem_num` past the set is `EFBIG` (where `semctl`'s `semnum` is `EINVAL`),
    more than `SEMOPM` is `E2BIG`, unknown `sem_flg` bits are ignored, and the
    values stay inside `0..=SEMVMX` with `ERANGE` at both ends; `GETALL`/`SETALL`
    are `unsigned short` arrays; a blocked `semop` is counted by
    `GETNCNT`/`GETZCNT`, woken by another process's raise, ended by a
    handler-bearing signal with `EINTR` and woken by `IPC_RMID` with `EIDRM`;
    `IPC_RMID` **removes** a set at once (unlike a segment's mark); `SEM_UNDO` is
    per process, is **not inherited by `fork`**, and its adjustment is the
    operation **negated**; the triads gate `semop` and the `GET*`/`SET*`
    (`EACCES`, a `0000` set refusing even its owner), the creator or
    `CAP_SYS_ADMIN` gates `IPC_SET`/`IPC_RMID` (`EPERM`), and `IPC_SET` reads its
    buffer first (`EFAULT`); and `/proc/sysvipc/sem` prints the header and
    columns `ipcs` parses.

    **Owner**: `src/sched/sysv_sem.rs` (the set table, the operations, the waiter
    table, the undo list and its exit application), `src/syscall/linux/ipc.rs`
    (the four doors and `semctl`'s command vocabulary),
    `src/syscall/linux/abi.rs` (the two machines' `semid64_ds` offsets),
    `src/sched/runtime/{task,child_exit}.rs` (the per-process field, the
    `CLONE_SYSVSEM` sharing and the exit pass), `src/syscall/linux/posix.rs`
    (`unshare`'s half) and `src/vfs/procfs.rs` (`/proc/sysvipc/sem`).

    **Pin**: the new `t-sysv-sem`, **70 claims**, plus seven host tests in
    `sched::sysv_sem` that pin the operation arithmetic, the waiter registration
    and the undo fold without a machine.

    **Gate**: x86_64 `pass=203 fail=0 (214 built)`, `mpos-kunit 32 passed, 0
    failed`, exit 0. The second machine compiles the change (kernel release +
    clippy, and the pin builds for it with its own 88-byte `semid64_ds`); its
    sweep could not boot on this host (no DRM render node), so no aarch64 figure
    is claimed.

    **Named**: the SysV **message-queue** family is still `ENOSYS`, so
    `CLONE_NEWIPC` is still refused with that one reason left. The table is the
    machine's rather than a namespace's. `SEMMNI`/`SEMMSL` are 4096/1024 here
    where Linux's are 32000 each - a configuration difference the calls report,
    not a shape one. A thread that `unshare`s `CLONE_SYSVSEM` gets a list of its
    own carrying its records (measured to match Linux for a single process; the
    sibling-sharing case is unpinned). `SEM_STAT_ANY` is served but not claimed
    by the pin.

## item 103 — System V message queues: the last family `CLONE_NEWIPC` was waiting on

    `msgget` (68/186), `msgsnd` (69/189), `msgrcv` (70/188) and `msgctl`
    (71/187) fell through the dispatch to `ENOSYS` - SysV's third family and
    the last thing standing in front of `CLONE_NEWIPC`'s contents. Measured on
    a current host first (Linux 7.2.11, `.workbuddy/probe_sysv_msg.c`,
    `probe_sysv_msg_qbytes.c`), recorded with the evidence in §6.15. The
    shapes that are Linux's and are now served: the queue is FIFO and
    *selection* is the receive's rule (`type 0` front, `type > 0` first of
    that type, `type < 0` the **lowest** type `<= -type`, `MSG_EXCEPT` another
    type); a receive that cannot fit the message without `MSG_NOERROR` waits
    for one that fits (`E2BIG` only under `IPC_NOWAIT`); `MSG_COPY` answers by
    queue index without taking, needs `IPC_NOWAIT` and answers `ENOMSG` past
    the end; the byte budget (`msg_qbytes`, 16384) is what "full" means
    (`EAGAIN` under `IPC_NOWAIT`, a wake from the receive otherwise), a
    non-positive `mtype` and a `msgsz` past `MSGMAX` are `EINVAL`; a blocked
    send or receive ends on its opposite number, `EINTR`, or `EIDRM`; the
    receive's `msgsz` is gated by its **sign** (`LONG_MAX` accepted,
    `LONG_MAX + 1` `EINVAL` with the message still queued) and a destination
    it cannot write is `EFAULT` **after** the message has been taken, so the
    failed call loses it;
    `IPC_SET` reads its buffer first, rewrites uid/gid/mode, maps the uid and
    gid through the **caller's** user namespace and refuses an uncovered one
    with `EINVAL` (`ipc_update_perm`'s rule, measured with a one-id map), needs
    `CAP_SYS_RESOURCE` for a `msg_qbytes` **raise** and accepts a lowering
    below the queued bytes; the triads gate send/receive/`IPC_STAT`
    (`EACCES`) and the creator or `CAP_SYS_ADMIN` gates
    `IPC_SET`/`IPC_RMID` (`EPERM`); and `/proc/sysvipc/msg` prints the header
    and columns `ipcs` parses. `msqid64_ds` is 120 bytes on both machines -
    x86_64's own `msgbuf.h` already carries the time64 padding, so one offset
    table serves both ports where `semid64_ds` needed two.

    **Owner**: `src/sched/sysv_msg.rs` (the queue table, the send/receive
    operations with selection, the waiter table), `src/syscall/linux/ipc.rs`
    (the four doors and `msgctl`'s command vocabulary),
    `crates/cred` (`CAP_SYS_RESOURCE`), `crates/err` (`ENOMSG`) and
    `src/vfs/procfs.rs` (`/proc/sysvipc/msg`).

    **Pin**: the new `t-sysv-msg`, **84 claims** (76 green on an
    unprivileged host run; eight more need root - the six another-user claims,
    `IPC_SET`'s id rewrite and the permission split asked from a dropped
    child - and are skipped with a line when it is absent), plus eight host tests in `sched::sysv_msg` - the host tests pin
    the selection rules, the budget, the wait-or-truncate fit rule, copy mode
    and the empty-queue wait the first draft panicked on.

    **Gate**: x86_64 `pass=204 fail=0`, stage 1's host suites `1098 tests, 0
    failed`, `mpos-kunit 32 passed, 0 failed`, exit 0 (the sweep's own QEMU
    exit is 33). The aarch64 arm builds - kernel, loader, the pin and
    `clippy` - but its sweep could not boot on this host (`egl: no drm render
    node available`, the limit §6.14 recorded), so no aarch64 figure is
    claimed; §6.15 carries that line.

    **Named**: `CLONE_NEWIPC` is still refused - nothing its isolation would
    cover is missing any more, but each family's table is the machine's
    rather than a namespace's; threading the tables through the namespace
    object is the next landing. `MSGMNI` is 4096 here where Linux's is 32000.
    `MSG_STAT_ANY` is claimed now, together with the permission split (`MSG_STAT`
    asks read permission and the `_ANY` spelling does not) - asked from a caller
    without `CAP_IPC_OWNER`, because that capability lets both spellings answer a
    mode-`0000` object. A negative or huge `msgtyp` under `MSG_COPY` is `ENOMSG`
    and takes nothing - measured and pinned. The `MSG_STAT` *argument form* and
    its return value are left **open**: probes on one host disagreed (a slot was
    `EINVAL` in one run and resolved in another; the return value was the id in
    one and `0` in another), so the kernel keeps the reduction that accepts both
    and §6.16 says why.

## item 104 — `CLONE_NEWIPC`: the seventh namespace kind, and the last thing `ipcs` waited on

    `unshare(CLONE_NEWIPC)` and `setns(fd, CLONE_NEWIPC)` were `EOPNOTSUPP`:
    the four families the kind isolates were served (items 100–103) and the
    object that isolates them was not. Measured on a current host first
    (`.workbuddy/probe_ipcns.c`, run as root inside a user namespace; the record
    is §6.16): the number an object is known by **restarts at 0** in a fresh
    namespace, an id from the namespace left behind answers `EINVAL`, a **key**
    resolves only inside its own namespace, all four families move together,
    `/proc/sysvipc/*` is the reader's view, a `fork` stays where its parent is,
    and `setns` back restores the first namespace's objects. §6.16 also records
    the trap the measurement handed over: a number is not an identity across
    namespaces, so every object now carries a machine-wide handle and the
    waiter/undo tables key by it.

    **Owner**: `src/vfs/ns.rs` (`Kind::Ipc`, the `Namespace::Ipc` handle, the
    machine's number, the `/proc/<pid>/ns` entry), `src/sched/runtime/task.rs`
    and `child_signal.rs` (the `ipc_ns` slot and the doors),
    `src/syscall/linux/{abi,posix}.rs` (the verdict and the two syscalls), and
    the four tables — `src/sched/sysv_{msg,sem}.rs`, `src/mm/sysv_shm.rs`,
    `src/vfs/mqueue.rs` — each a slot space per namespace.

    **`clone(CLONE_NEWIPC)` is served too** (same day): the child moves into the
    fresh namespace and the creator keeps its own (measured,
    `.workbuddy/probe_clone_ipc.c`), which is one field on a door that already
    served five kinds - the verdict's `SERVED`, `CloneRequest::ipc_ns`, and the
    syscall layer minting it where it mints `net_ns`. A thread asking for it
    stays `EOPNOTSUPP`.

    **Pin**: the new `t-ns-ipc`, **39 claims**, every one run against a real
    kernel before it was pointed at this one, plus two host tests -
    `sched::sysv_msg`'s `a_namespace_has_its_own_slot_space` and `vfs::ns`'s
    `the_last_reference_frees_the_ipc_tables`. The pins that had
    asserted IPC was *unserved* moved with the measurement: `t-caps` and the
    whole-word refusals in `t-ns-net`, `t-ns-pid`, `t-ns-uts` and `t-ns-mnt`
    now name `CLONE_NEWCGROUP`, and `t-ns-setns`'s directory listing expects the
    `ipc` name.

    **Gate**: x86_64 `pass=205 fail=0`, stage 1 `1099 tests, 0 failed`,
    `mpos-kunit 32 passed, 0 failed`, exit 0; `depgraph` within the ratchet. The
    aarch64 arm builds (kernel, loader, clippy) but its sweep cannot boot on this
    host (`egl: no drm render node available`), so no aarch64 figure is claimed.

    **The tables are freed when the namespace dies**, and it is the *object* that
    says when: the task's slot and a `/proc/<pid>/ns/ipc` descriptor hold the
    namespace by `Arc`, and the last drop frees the four tables' spaces for its
    number. Without it a loop of `unshare` + one queue grew the kernel without
    bound; with it the objects a task that *left* still uses stay alive by their
    own reference, which is Linux's `ns_common`/`shm_file` arrangement. The
    observable half is pinned: `setns` into a namespace whose last member left
    still finds its queue (`t-ns-ipc`), and `drop(ns)` empties all four tables
    in `vfs::ns`'s host test.

    **Named**: `CLONE_NEWCGROUP` is the last kind with nothing behind it, and it
    waits on the cgroup tree it would isolate.
