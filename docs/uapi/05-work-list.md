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
    `t-fd-name` (37 claims, green on this host first, its `ok:` lines byte-identical on both
    machines), which
    claims the pair `stat`/`lstat` draws, `statx` both ways, the four refusals, the mutators with
    their answers read back through the descriptor, the name inside a path, `chdir`, the fresh
    description with its own flags and position, the deleted file, the closed descriptor and
    `/dev/fd/N`. **Four things stay named in §5.2**, each measured, each its own owner: a *foreign*
    process's descriptor (this kernel's link lends a descriptor only to the caller's own thread
    group, so a foreign one falls back to splicing its report as a path — a named file is then
    reached by the path it reports, and a channel answers `ENOENT` where Linux hands over the
    pipe); `st_dev` (the file's through the name on Linux, the *name's* filesystem here — the
    limitation §5.1 item 5 already owns, and the reason `t-fd-name` compares kind, size and inode
    and merely prints both devices); `..` after the name (Linux jumps the walk onto the file, so
    the parent is the *file's*; here it is the name's directory, because the walk's chain is what
    answers `..` and a jumped-to node's parent is not a fact this kernel's inodes carry); and the
    link's own metadata (`lstat`'s mode and size: Linux derives the mode from the descriptor's
    direction and reports 64 bytes, this kernel's `ProcLink` inherits the trait's `0` and its own
    `size: 0`). A fifth thing the same pin measured became an entry of its own in §5.2, because it
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
    as root and a hand host run need not), its `ok:` lines identical on both machines; the `S_IFIFO`
    section claims the node on both machines now that item 26 serves it. **A third fix
    the pin forced, in the same subject**: `stat`'s permission half collapsed a stored `0000` into
    the kind's default — `mode_of` was handed a bare `u32` and read `0` as "nobody said" — so a file
    `mknod` was told to make with mode `0` (or one `chmod 000`ed) *enforced* its empty mode through
    `may_as`, which asks `keeps_modes`, while *reporting* `0644`. The stat doors now pass
    `Option<u32>` through the new `reported_mode` (`src/syscall/linux/fs/file.rs`): a filesystem that
    keeps modes reports what it holds, **including `0000`**, and one that keeps none answers its
    kind's default. Two things stay named in §5.2 rather than fixed: the **device number**
    `mknod`'s `dev` argument names is not written on the node; and **`CAP_MKNOD` is not checked**, so
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
