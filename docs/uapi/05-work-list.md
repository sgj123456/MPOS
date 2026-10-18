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
    group with no child of ours `ECHILD`). `P_PIDFD` stays `EINVAL` and moves to §5.3 with its
    prerequisite named: no pidfd family exists here to resolve a descriptor through, where Linux
    answers `EBADF` for one that is not a pidfd. The options word is Linux's whole: the three
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
    `Rss` needs), and the module's `sched` re-export. What stays named: `numa_maps` and
    `smaps_rollup` (§5.2), and the three `Pss_*` lines `smaps_rollup` has and `smaps`
    does not.

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
    shrink it); the datagram-too-big line remains the ring's rather than Linux's 65507,
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

    Still named, and measured: `/proc/<pid>/status`'s `exit_signal` line renders the constant
    `17` rather than the task's byte (a rendering nothing pins), `__WNOTHREAD` stays
    accepted-and-ignored (item 50's own), and the *state change* a death `SIGCONT` carries —
    Linux's sender-side resume of a stopped parent, measured — is the `kill` path's alone and
    is not performed for a death record.

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

