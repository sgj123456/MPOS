# §3 Phase 3: Filesystem Enhancements (Medium Priority)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 3: Filesystem Enhancements (Medium Priority)

### 3.1 Hard Links — **DONE**

`linkat` creates a real second name (ext4 adds a dirent and bumps `i_links_count`;
ramfs shares the `Arc<dyn Inode>`), `unlink` decrements and frees only at zero,
and `stat`/`statx` now report the true `st_nlink` (ext4 via `Inode::nlink()`,
other kinds falling back to the `.`/`..` convention).

**Changes**:
- Add `link_count()` and `add_link()` / `rm_link()` to `Inode` trait
- ext2/ext4: implement `i_links_count` in on-disk inode, decrement on unlink, free when zero
- ramfs: share `Arc<dyn Inode>` across directory entries
- Test: `ln file1 file2 && echo test >> file2 && cat file1` shows same content

### 3.2 File Locking (fcntl F_SETLK) — **DONE**

POSIX record locks are real: a global table keyed by `(dev, ino)`, each entry
`{owner, pid, class, kind, [start,end), ofd}`, in `src/vfs/locks.rs` (the range
math, conflict rule and split-on-relock are host-tested). Both lock families live in
that one table but never see each other - a `class` of `Record` (`fcntl`
`F_SETLK`/`F_GETLK`/`F_SETLKW`) and `Whole` (`flock`) conflict only within the
class, as Linux defines the two to be independent. `F_SETLK` takes or drops a
byte range and refuses a blocked request with `EAGAIN`; `F_GETLK` reports the
first conflicting lock (its kind, range and holder pid) or clears to `F_UNLCK`;
`F_SETLKW` blocks and wakes on release. `flock(LOCK_SH|LOCK_EX|LOCK_UN)` covers
the whole file (`[0, infinity)`), shared/exclusive arbitration identical to the
record locks, and blocks unless `LOCK_NB`. A holder that dies is pruned on the
next grant/query.

`owner` is deliberately *not* the same thing in the two classes, because Linux
draws the line in two places: a record lock is owned by the thread group, a
`flock` by the open file description (the `Arc<File>` the fd table shares, so a
`dup` shares the lock and a second `open()` does not). The two owners are
released by two different events, and the close path asks both: `drop_on_close`
takes the process's POSIX record locks on **any** close of a descriptor to the
file (the rule that surprises people), and `drop_with_description` takes the
description's own - its `flock` and its `F_OFD_*` lock - on the close of the
**last** descriptor referring to it, which the close path knows from the
description's reference count after the descriptor is gone. The second half is
Linux's since 2026-10-16 (§15.16 carried the divergence and its measurement
first, and `t-filelock` now pins the rule: a fresh `open()` still finds each
class in the way after a `dup`'s close). `t-filelock`
(`user/abitest/src/bin/t-filelock.rs` before S6 batch 11) pins both, with the
contending-second-`open` and the sharing-`dup` cases side by side.

**The third ownership model, `F_OFD_*`, is a record lock the description owns.**
Linux numbers the open-file-description twins `36..=38`, straight after
`F_GETLK`/`F_SETLK`/`F_SETLKW`, and they are the same byte-range request in the
*same conflict domain* - an OFD lock refuses a POSIX one and the reverse - but
their owner is the open file description rather than the thread group, so two
independent `open()`s of one file inside one process contend while a `dup`
shares. Every clause was measured on the host before the shape was written
(`.workbuddy/probe_ofd.c`): the independent second `open()` answers
`EWOULDBLOCK`, the `dup` takes it, a POSIX lock through the second description is
refused by the OFD holder, and `F_OFD_GETLK` reports `l_pid = -1` - there is no
process to name, which is exactly why Linux's `locks_translate_pid` returns `-1`
before it ever translates. The implementation is one `ofd` flag on the lock
entry (the owner is `File::ofd()`, the same description identity `flock` keys on)
and one arm each in the `record_lock` set and query paths; `F_OFD_GETLK` is
`F_GETLK` minus the translation. Pinned by `t-filelock`'s nine new claims on both
machines.

The `F_OFD_*` lock shares the `flock`'s release rule rather than needing one of
its own: both are the description's, so both go with the **last** descriptor
(`drop_with_description`, the same call the close path makes for the `flock`) -
case D of `.workbuddy/probe_ofd.c` is the measurement (an OFD lock survives
`close(dup)` on a real host), and `t-filelock` pins it beside the `flock`'s.

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with `mpos-kunit 32
passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. The program counts did not move - the new verdicts are nine more
`ok:` lines inside `t-filelock`, whose `flock`/POSIX half is unchanged. The
host-side lock test `an_ofd_lock_is_a_record_lock_owned_by_the_description`
lands with the `ofd` flag; clippy reports nothing on either bare-metal target or
the host lib, and `cargo xtask depgraph` is within the ratchet with no edge
added.

**Gate, 2026-10-16 (the release rule).** x86_64 `=== SUMMARY pass=175 fail=0
(175 built)` with `mpos-kunit 32 passed, 0 failed` and exit 33; aarch64
`=== SUMMARY pass=174 fail=0`, the same 32 cases. The program counts did not
move again - `t-filelock` is 26 `ok:` lines on both machines, byte-identical -
and three host unit tests land with the two pure release functions
(`a_close_takes_the_processs_record_locks_and_no_more`,
`the_last_close_takes_the_descriptions_own_locks`,
`the_two_closes_divide_the_table_between_them`). The counterfactual is exact:
with the description's release moved back to any close, `t-filelock` is the
sweep's one red and the two new claims are precisely the two that fail.

*Not a gap:* `read`/`write` do not consult the lock table, which is what Linux
does. These are **advisory** locks: nothing refuses a `read` that walks over
another process's `F_WRLCK`. Linux's mandatory mode is a per-file opt-in -
setgid with the group-exec bit cleared, on a mount that allows it - and is
documented as broken and deprecated; adopting it here would be a divergence
rather than a completion. What the two families *are* used for here is
cooperation: `apk` and `fish` take the locks, and callers that hold one refuse
the other.

### 3.3 File Timestamps — **DONE** (ext4; ramfs legacy calls remain)

`utimensat`/`futimens` now store the access and modification clocks on ext4
(`Inode::set_times`), stamping `ctime` to the current time as the metadata
change it is; `stat`/`statx` report the three clocks separately (ext4 keeps
`i_atime`/`i_mtime`/`i_ctime` on disk) instead of collapsing them to mtime.
`open`/`write`/`truncate` bump mtime/ctime; a fresh inode is created with all
three set.

**The access clock landed on 2026-10-27**, closing this section's last named
"not bumped" gap. It took three facts that a single "update atime on read" would
have got wrong, and each was measured on a current host first
(`.workbuddy/probe_atime.c`, Linux 7.2.11; the uid-drop half in a container's
root):

* **`relatime` compares `<=`, not `<`, and it asks mtime as well as ctime.** A
  freshly created file - three equal clocks - *does* move its atime on the first
  read. The old `ext4` body used `atime < ctime` and never asked mtime, so a new
  file's first read left the clock alone;
* **`/tmp` is a tmpfs and tmpfs is `strictatime`**, not relatime: even an atime
  `utimensat` wrote into the *future* is pulled back to the read's moment. An
  ext4 mount leaves that future value standing. The two filesystems really
  answer differently, so they spell their own rule;
* **`O_NOATIME` is a promise a stranger may not ask for.** It suppresses the
  update, and an open that asks for it without owning the file or holding
  `CAP_FOWNER` is `EPERM` - at the *open*, after `may_open`'s `EISDIR`/access
  refusals, which is Linux's `do_dentry_open` order.

**The landing** moved the update out of `ext4::read_at`, where it could not see
a description, into `File::read` - the one door `read`, `pread`, `readv` and
`preadv` all come through - behind a new `Inode::touch_atime`. The default is
`Ok(())` (a virtual node invents no clock); `ext4` spells relatime over its
on-disk `i_atime`, and `RamFile` spells strictatime over three in-memory clocks.
The ramfs regular file now keeps atime/mtime/ctime apart; its directory,
symlink, socket and device nodes still fold atime into mtime, which is named
below. `O_NOATIME` is a `File`-carried open flag (`O_NOATIME`, the same number
on both machines) and the owner/`CAP_FOWNER` question is asked once at
`open_opt`. `vfs::now_seconds` is the one clock both drivers stamp with.

**Pin**: the new **`t-atime`**, 7 claims on both machines (the host's own Linux
first, `MPOS_ON_HOST=1`): a fresh file's first read moves atime; a read moves
nothing else; an older atime moves; a *future* one is pulled back (strictatime);
`O_NOATIME` leaves it; the plain description beside it still moves it; and a
65534 stranger's `open(O_NOATIME)` is `EPERM`. The relatime half - "a future
atime stands, a second read moves nothing, a day-old one moves" - is pinned by
the rewritten `vfs::ext4::tests::relatime_moves_atime_only_when_something_has_happened`,
because the sweep machine has no disk to read an ext4 file from.

**Counterfactuals, both run**: with `File::read`'s call deleted, x86_64
`pass=212 fail=1`, exactly the three moving claims red; with the `O_NOATIME`
flag ignored and the open's authority check deleted, exactly the two
`O_NOATIME` claims red. The controls (mtime/ctime unchanged) stay green in both.

**Gate**: x86_64 `pass=213 fail=0`, aarch64 `pass=212 fail=0`, `mpos-kunit 34
passed, 0 failed` on both; host suite 640; `test-host` 201/227.

ramfs/tmpfs's directory, symlink, socket and device nodes still keep one clock -
stamped at creation and by writes, truncates and directory-entry changes - which
is what stopped OpenRC's deptree freshness check from reading every `/run` file
as 1970. The legacy `utime` and `futimesat` now store
through the same `Inode::set_times` path. devtmpfs's device nodes keep them
too, both of the kinds it publishes - the node the filesystem makes around a
driver, and the block node, which was the one a boot's own refusal named -
along with every other node holding a `NodePerms` (the console tty,
`/dev/tty0`, the controlling-tty alias, `/dev/ptmx`, the DRM card, `/dev/fb0`,
`/dev/uinput`, the evdev nodes). A node reports zero until something stamps it:
Linux's devtmpfs stamps one at creation, and there is no mount-time number here
to give a node that arrives when its driver registers it. What stayed outside
longest was devfs's own directories, symlinks and socket names, which hold no
`NodePerms`; they now keep both clocks in a `Clocks` of their own, the refusal a
desktop still showed having come from that pass, and the trait's default became
`EPERM` rather than `EOPNOTSUPP`, which is the only refusal Linux gives this
call (docs/uapi/ §5.1 item 13, §15.32, §15.35).

**Changes**:
- Add `atime`, `mtime`, `ctime` fields to `Inode` trait (or a `Metadata` struct)
- ext2/ext4: store in `i_atime`, `i_mtime`, `i_ctime` on disk
- ramfs: store in `RamInode` behind mutex
- `stat` returns real timestamps
- `open`/`write`/`read`/`mkdir`/`unlink` update mtime/ctime

### 3.4 Permission and Ownership Model — **DONE** (fully Linux-aligned)

`chmod`/`chown` store real values and every access path enforces Linux's model:
- ext4 persists `i_uid`/`i_gid`/`i_mode` (read into `Inode::mode`/`owner`, written by `set_owner`/`set_mode`); ramfs keeps them in memory.
- `stat`/`fstat` return the stored uid/gid/mode.
- `open()`/`truncate` and the create/remove/`link`/`rename` paths call `vfs::may_access` → `may_as(current_owner, ..)`: root (euid 0) gets Linux's `CAP_DAC_OVERRIDE` — every check passes **except** execute on a non-directory with no x-bit for anyone (so even root gets `EACCES` running a `0644` file, and `access(X_OK)` on one is `EACCES`); otherwise the owner/group/other `rwx` triads gate `R_OK`/`W_OK`/`X_OK`, with the group triad matched against the effective gid **and the supplementary groups**, exactly like Linux.
- Directory **search** (`may_search`, `X_OK` on each path component) is enforced during resolution (`mount.rs`, `open`), not just on the final node.
- The sticky bit gates name removal; `access`/`faccessat` use real ids unless `AT_EACCESS`; `setuid`/`setgid` execution and `chmod`'s clearing of those bits by a non-owner follow Linux.
- A filesystem that keeps no modes (`keeps_modes() == false`) is not policed, so invented permissions never refuse.
- **devfs nodes keep modes and owners now.** Before this a `chmod` on any device
  node answered success and stored nothing — udev's `MODE=`/`GROUP=` rules were
  logs, and `stat` showed a number no check ever read. Every `/dev` inode class
  (the generic stream node, the ttys, the DRM card and its leases, evdev,
  `uinput`, `fb0`, `ptmx`) carries a `devfs::NodePerms` and answers
  `keeps_modes() == true`, so the triads below decide what the device really
  means.
- **They are born with devtmpfs's own numbers now, measured.** A udev-free boot
  of the host's devtmpfs (a static init under QEMU, host kernel `7.2.11-arch1-1`)
  enumerated 131 nodes; all but nine are `0600 root:root`. The wide ones are the
  stream family — `/dev/null` (1:3), `zero` (1:5), `full` (1:7), `random` (1:8),
  `urandom` (1:9) — and the tty auxiliary pair `/dev/tty` (5:0), `/dev/ptmx`
  (5:2), all `0666 root:root`; `/dev/kmsg` (1:11) and `/dev/autofs` (10:235) are
  `0644 root:root`. Everything else is `0600`: the console tty (5:1), `tty0`
  (4:0), `ttyS0` (4:64), the DRM card, evdev, `uinput`, `rtc0`, the block devices,
  `mem`, `port`. `src/vfs/devfs.rs` stamps that table — `birth_mode(name)` names
  the eight wide classes and answers `0600` for an unknown one, the block class
  births `0600`, and each driver names its own class (`DrmCard` and its leases,
  `uinput`, `FbDev`, `EvdevNode`) rather than trusting the generic node. The
  direction of the default is the measurement's, not a preference: a node born
  too tight is what udev widens, one born too wide is wrong in every boot that
  runs no udev at all.
- **The image's half is what its own udev makes of that.** The Alpine image runs
  eudev (`assets/configs/rootfs.toml`; libinput only takes devices udev tagged
  `ID_INPUT=1`), so a real boot re-widens what it recognises. Measured on the
  built image ten seconds into udev's work (`stat` through a `/etc/local.d`
  script): `null|zero|full|random|urandom|tty|ptmx` `0666 root:root`; `kmsg`
  `0644`; `console|tty0|tty1|ttyS0|uinput|rtc0` `0600 root:root`; `card0` `0660
  root:video`; `renderD128` `0666 root:video`; `vda|vda1|vda2` `0660 root:disk`.
  The classes the image's rules reach are widened the way a devtmpfs boot's are;
  the console/tty family, `uinput` and `rtc0` keep the kernel's number because no
  rule of this image's `50-udev-default.rules` reached them — so its group
  ownership for the tty class is not applied and `tty0` is not widened to Linux's
  `0620 tty`. The kernel does no group-name resolution; that is udev's half, and
  the split the note used to name as a remainder is what this now measures.
- **Pinned** by `t-dev-birth` (new, both machines): the seven `0666` classes,
  `kmsg` at `0644`, `0600 root:root` for `console/tty0/tty1/rtc0`, the first of
  `ttyS0`/`ttyAMA0`, the drm card and render node, `fb0`, evdev, `uinput`,
  `hwrng`, `hvc`/`hvc0`, `dsp`/`rtc`, and the block node — each claim skipped,
  not failed, on a machine without the device. No claim is made for `/dev/pts/N`:
  devpts `mode=`/`gid=` mount options are not applied here (a named remainder,
  not folded into this item) - devpts's `mode=`/`gid=` options and the minting
  owner are §3.8's question (item 111). `t-dsp`'s chmod cases now widen deliberately to
  `0666` and restore `0600` — chmod still stores, and the birth number is
  separate from it.
- Host-tested: `a_triad_decides_who_may_do_what` (incl. supplementary groups), `root_passes_everything_but_execute_on_a_file_nobody_made_runnable`, `may_as_answers_for_the_node_and_the_identity_it_is_given`, `the_birth_table_is_the_measured_one`, `a_node_is_born_with_devtmpfs_s_mode_for_its_class`.
- **Counterfactual** (the table reduced to one `0666`, which is the pre-item body):
  x86_64 gate `pass=215 fail=1 (216 built)`, `t-dev-birth` red on sixteen claims —
  `console`, `tty0`, `tty1`, `rtc0`, `ttyS0`, `card0`, `renderD128`, `fb0`,
  `event0`, `uinput`, `hwrng`, `hvc0`, `hvc`, `dsp`, `rtc`, `vda`, each read as
  `(is 0666 0:0)`. Restoring the table brings the arm back to `pass=216 fail=0`
  and `pass=215 fail=0` on aarch64 (the documented EL0 boot flake intervenes on
  some runs; killed and re-run, per `AGENTS.md`'s trap table).

### 3.5 /proc Enhancements — **DONE**

procfs also carries the top-level `uptime`, `loadavg`, `cpuinfo`, `version`,
`filesystems`, `partitions`, `devices`, `stat` and the `/proc/sys` knob tree
(`kernel.ostype/osrelease/hostname/pid_max/...`), per-pid `statm` and `limits`,
and `/proc/net/tcp` + `/proc/net/udp` - the live socket table `ss`/`netstat`
read. The `/proc/net` rows come from `net::collect_net_sockets` walking the
registry under the stack lock (local/remote endpoints, the numeric TCP state,
inode) and a pure `format_net` renders Linux's exact column layout (address as
little-endian hex, `:port`, state byte) - the formatter is host-tested
(`the_net_table_prints_addresses_and_state_as_linux_does`).

### 3.6 A descriptor's filesystem is the one it was opened on — **DONE** (2026-10-21)

`fstatfs(2)` used to name a filesystem by the *inode*: a mount's own root inode
named that mount, and everything else fell through to the root filesystem - so
every regular file, every directory below a mountpoint and every `/proc/<pid>/`
file reported `/`'s identity, while `statfs(2)` on the same path reported the
mount that serves it. Two doors, one file, two answers; `df` read a mounted
filesystem's free space as the root's.

The fix keys the answer by the **device number the descriptor recorded at
open** (`File::dev`, the same record `fstat`'s `st_dev` has been answered from),
so a descriptor and a path can no longer disagree: `MountTable::point_of_dev`
walks `mount_points` - the very table `serving_point` reads forward - for the
record carrying that number, choosing the deepest mountpoint among records that
share one (the same longest-prefix choice the forward walk makes, which is what
keeps the two directions one rule rather than two), and `MountTable::fs_of_dev`
hands back the instance and the type from that one record so `f_blocks` and
`f_type` cannot come from two mounts. Descriptors no path made - a pipe end, a
socket, an anonymous inode - keep the inode-identity fallback, which really is
the root filesystem.

**Measured on a current host first** (`.workbuddy/probe_fstatfs.c`, Linux
7.2.11): `/` and `/etc/hostname` both answer the host's ext4, `/proc` and
`/proc/self/status` both answer procfs, `/dev/shm` and a file written inside it
both answer tmpfs, and `fstatfs(fd) == statfs(path)` in every row - Linux asks
the vfsmount the open took, and nothing about the inode enters into it.

**Pinned** by `t-statfs` (24 -> 26 claims, `ok:` for `ok:` identical on both
machines, green on the host's own Linux first). The `/tmp` scratch-file claims
this item *started* with were dropped: the first machine mounts a ramfs at
`/tmp` over a ramfs root, so those claims were green in the counterfactual too -
a pin answered alike either way carries no information, and the claims that
remain compare the mountpoint's descriptor with the one for a file inside it,
which catches the old answer on either machine. Counterfactual (the inode-based
body restored): x86_64 `pass=204 fail=1 (205 built)`, the one red program
`t-statfs` with exactly those three claims red. Full record: `docs/uapi/`
§5.1 item 89.

---

### 3.7 `f_fsid` is the filesystem's identity, and it is no longer zero — **DONE** (2026-10-21)

`statfs(2)`'s `f_fsid` was the one field this kernel cleared: every mount
answered zero. `f_type` names the *kind* of filesystem, so two ramfs mounts and
two ext4 mounts look identical under it; `f_fsid` is the per-*instance* answer
Linux's man page offers for exactly that comparison, and zeroing it made every
filesystem look like the same filesystem to anything that compared them.

**Measured whole on a current host first** (`.workbuddy/probe_fsid.c`, Linux
7.2.11): `sizeof(struct statfs)` is 120 with `f_fsid` at offset 56, and every
row is non-zero; `fstatfs(fd)` equals `statfs(path)` in every row. The values
split two ways, and that split is the whole design constraint: `/proc` answers
`0x18` — exactly its own `st_dev` put through Linux's `new_encode_dev` — while
`/`, `/sys` and `/tmp` answer 64-bit values that are UUID hashes, different on
every machine and every boot. **Nothing here can pin a value**; what can be
pinned is the structure every row shares.

**The landing** gives the field one source rather than a second identity:
`abi::encode_fsid` spells `new_encode_dev` (`include/linux/kdev_t.h`, verified
against a C program before being trusted — `.workbuddy/check_encode.c`), and
both doors feed it the device number they *already* report as `st_dev` — the
descriptor's recorded `File::dev` for `fstatfs`, `mount::dev_of_path` for
`statfs`. So `f_fsid` and `st_dev` cannot disagree about which filesystem
answered, and the two doors cannot disagree with each other.

**Pinned** by `t-statfs` (26 -> 36 claims, `ok:` for `ok:` byte-identical on
both machines, green on the host's own Linux first): three raw reads into a
caller-owned 120-byte buffer (neither wrapper exposes the field —
`rustix::fs::StatFs.f_fsid` is a private `Fsid`), the root's `f_fsid` non-zero,
procfs's non-zero, two names one instance serves agreeing, two live mounts
differing, and `fstatfs` on a descriptor agreeing with `statfs` on its path.
The three raw *reads* are claims of their own, so the block cannot be skipped
silently. Counterfactual (the field left cleared again, which is the pre-item
body): the host unit test `write_statfs_reports_the_superblock_magic…`
reddened first, stopping the gate before QEMU; with that assertion restored to
its pre-item form so the guest arm could run, x86_64 `pass=204 fail=1 (205
built)` and `t-statfs` had exactly three claims red — both "reports a non-zero
`f_fsid`" and "two mounts that are both alive report different ones". The two
*equality* claims stayed green there by construction: zero equals zero. They
are not the discriminating pins and are not claimed to be — what they catch is
a half-fix, one door given an identity and the other still cleared.

Also fixed while here: the `EBADF` claim minted its descriptor number by
opening and closing a file of its own instead of borrowing fd 99, which was
open in the caller's table the first time this ran under a live shell, and made
the claim fail on the host while passing in the guest.

Full record: `docs/uapi/` §5.1 item 90.

---

### 3.8 A pty slave is born with its devpts mount's numbers — **DONE** (2026-10-28)

**Why**. An Alpine image's init mounts devpts with `-o mode=0620,gid=5`, and
`login` hands the terminal over with a `chown` on the slave. This kernel's
`mount(2)` never read its fifth argument at all, so every `/dev/pts/N` reported
`0666 0:0`: `PtySlave` implemented neither `mode()` nor `keeps_modes()`, and
`reported_mode` therefore fell through to the char-device kind default
(`0o020_666`). The number a terminal program reads in `stat` was a lie with a
plausible face, and the hand-over could not narrow it.

**Measured first** — Linux 7.2.11, probes under `unshare -Urm` with two ids
mapped (`build/scratch/devpts-probe/probe{,2,3,4}.py`):

| mount | slave `stat` |
|---|---|
| no options | `0600 0:0` — the kernel's own default, *not* `0620` |
| `mode=0620,ptmxmode=0666` | `ptmx 0666`, slave `0620 0:0` |
| `mode=0640,gid=1`, creator uid 1 gid 1 | `0640 1:1` |
| the same mount, creator uid 1 gid **0** | `0640 1:1` — the creator's gid is ignored, `gid=` wins |
| `uid=1`, creator root | `uid=1` — `uid=` overrides the minting euid, whose value is otherwise the default |
| `nonsense=1` | the mount fails, rc 32 (`EINVAL`) |

Two more readings shaped the design. **Each mount is its own instance by
default**: two mounts in one namespace with `0600` and `0640` handed out those
numbers from their own `ptmx`, without `newinstance`. And the host's own
`/dev/pts/0` is `600 mp:tty` — its `mountinfo` line is
`devpts devpts rw,gid=5,mode=600,ptmxmode=000`, the uid being whoever minted.

**The divergence, named rather than hidden.** Linux can give each mount its own
instance because `/dev/pts/ptmx` exists under each mount and a caller opens the
one it means. This kernel has **one** `/dev/ptmx` and one pair registry, so a
mount's options are the machine's minting defaults from then on, and a pair's
numbers are fixed when it is minted — a later mount does not repaint the pairs
that already exist. `newinstance` (already the effective behaviour), `ptmxmode=`
(no `/dev/pts/ptmx` node to apply it to) and `max=` (no accounting to bound) are
therefore refused `EOPNOTSUPP` rather than accepted and dropped; an option the
kernel does not know is `EINVAL`, as Linux measured. Options handed to any other
filesystem type are still ignored — a named, pre-existing gap, unchanged here.

**Landed.** `src/vfs/pty.rs` carries `DevPtsOptions { mode, uid: Option<u32>,
gid }` with the measured `DEFAULT { 0o600, None, 0 }`, one machine-wide
`DEVPTS_OPTIONS` behind it, and `minting_uid()` = `sched::current_fs_owner().0`
— the filesystem uid every node the process creates is stamped with, which is
the id the measurement says a slave takes. The permission store lives on the
**pair** (`Pty::perm`), not on a slave node: `PtySlave` is constructed fresh per
lookup, so a mode or owner kept there would be lost between two `stat`s, while
`login`'s `chown` has to stick. `PtySlave` forwards
`keeps_modes/mode/set_mode/owner/set_owner` to `Pty::perm`, which is also what
makes `stat` report the mount's number instead of the kind default.
`allocate_dynamic` mints each new pair with `options.mode` and
`NodePerms::new(options.mode, options.uid.unwrap_or_else(minting_uid), options.gid)`.

`src/vfs/devpts.rs` gains `apply_mount_options(data)`: comma-split fields,
`mode=` parsed octal and masked `0o7777`, `uid=`/`gid=` decimal,
`newinstance`/`ptmxmode=`/`max=` → `Unsupported`, anything else → `Invalid`, and
the result published as the machine's minting defaults (starting from `DEFAULT`
each mount, so a no-option mount resets them). `handle_mount` in
`src/syscall/linux/fs/file.rs` now reads its fifth argument — a NULL pointer is
Linux's "no options", not the `EFAULT` every other string argument answers — and
`mount_new` takes it; only the `"devpts"` arm interprets it, because a
filesystem's options are its own to interpret.

**Pinned** by `t-devpts` (new, both machines): an option-less mount births
`0600 0:0`; `mode=0640,gid=5` births `0640 0:5`; `mode=0620` births `0620 0:0`;
the mount's own root view agrees; a pair born under the first mount still reads
its own numbers after two later mounts; a `chown` moves the owner and leaves the
mode; `nonsense=1` and `mode=zz` are `EINVAL`; `newinstance`, `ptmxmode=0666`
and `max=2` are `EOPNOTSUPP`; mounting devpts on a regular file is `ENOTDIR`.
Host tests `a_mount_publishes_the_numbers_its_options_name`,
`an_option_this_kernel_cannot_make_true_is_refused` and
`a_dynamic_pair_is_born_with_the_mounts_numbers` pin the parse table, the
refusals and the pair's ownership. The listing test now judges the console's
absence by *device* (major 4) rather than by the name `1`: the first
dynamically minted pair legitimately holds that number, and the old assertion
would redden on a test-order accident rather than on a regression.

**Counterfactual** — the pre-item pair body (minted `0o666` with no mount
numbers, `// TEMP-CF`): x86_64 `pass=217 fail=1 (218 built)`,
`self-test: 1 test(s) did not pass`, and `t-devpts` red on exactly five claims,
each `(is 0666 …)` — the option-less default, `mode=0640,gid=5`, `mode=0620`,
"the first pair, after two later mounts", and the mode half of the chown claim.
Restored: **x86_64 `pass=218 fail=0`** (8.64 s stage 2, `mpos-kunit 34 passed`),
**aarch64 `pass=217 fail=0`** (22.71 s, `mpos-kunit 34 passed`), host suite
647 passed.

Full record: `docs/uapi/` §5.1 item 111.

### 3.9 `fanotify_init`/`fanotify_mark` — the notification class — **DONE** (2026-10-28)

**Why**: the second file-event family a Linux userland reaches for. inotify
answers "what happened on this inode" per process; fanotify answers the same
question for a whole group descriptor, and its content classes add an
antivirus-shaped "may I?" round trip that needs a reply record. The
notification class is the half the tree can serve honestly now: records on a
group descriptor, marks on one object each, the measured wire shape and the
measured flag tables. The content/pre-content classes and their permission
events are refused `EOPNOTSUPP` by name — a permission handler that cannot
answer must not pretend the file was allowed.

**The measurement had to be taken as real root.** `fanotify_init` asks
`CAP_SYS_ADMIN` in the *initial* user namespace: an ordinary user is `EPERM`
before a flag word is read, and so is uid 0 inside `unshare -Ur` (both
measured). So the host kernel was booted in QEMU with a throwaway initramfs
whose `/init` is the probe (a real-root Linux 7.2.11, PID 1), and the same
probe then booted on the MPOS image to record the baseline — every call there
`ENOSYS`, which is what this item replaces.

| Question | Linux 7.2.11, real root |
|---|---|
| `init(0,0)` | fd 3; `F_GETFD=0`, `F_GETFL=0x2` (`O_RDWR`) |
| `FAN_CLOEXEC` / `FAN_NONBLOCK` | accepted; the descriptor carries both |
| `FAN_CLASS_CONTENT` / `PRE_CONTENT` | accepted |
| `FAN_REPORT_TID` (0x100) | accepted |
| `FAN_REPORT_NAME` (0x800) | `EINVAL` |
| `FAN_ENABLE_AUDIT` (0x40), `FAN_UNLIMITED_*` (0x30) | accepted |
| `event_f_flags` = 0, `O_RDWR`, `O_WRONLY`, `O_APPEND`, `O_CLOEXEC`, `O_NOATIME`, `O_CLOEXEC\|O_NONBLOCK` | all accepted |
| `ADD\|INODE` of `FAN_OPEN` by path | 0, and the same again 0 |
| mask 0, unknown mask bit, `ADD\|REMOVE`, permission mask on a notification group | `EINVAL` |
| `ADD\|MOUNT`, `ADD\|FILESYSTEM`, `FLUSH` | `0` |
| `ADD\|INODE` with `dirfd=-1` and a NULL name | `EBADF` |
| a record | 24 bytes: `event_len` 24, `vers` 3, `metadata_len` 24, `mask`, `fd`, `pid` |
| a child opening the marked file | `mask=0x20` (`FAN_OPEN`), `fd` a fresh description, `pid` = the **child's** pid |
| a 4-byte buffer / `read(NULL,0)` | `EINVAL`, and the record survives the short read |
| `REMOVE` by path after an fd-based add | `ENOENT` (no mark for that path's object) |

**What landed** — `src/vfs/fanotify.rs` (the uapi vocabulary, one
`FanotifyGroup` per descriptor with its queue, marks and waiters, the flag/mask
validators, the 24-byte record writer, the emit path over live groups, and the
`Inode` implementation whose read packs whole records and parks for news);
hooks beside the inotify ones (`open`, `exec`'s `FAN_OPEN_EXEC`, the three
read/write/attrib doors, the close path and the exit hook, registered in
`src/board/steps.rs`); `handle_fanotify_init`/`handle_fanotify_mark` in
`src/syscall/linux/fs/file.rs`; numbers `300`/`301` x86_64 and `262`/`263`
aarch64 with the two dispatch arms. The privilege test is uid 0, the same
authority proxy the keyring uses.

**Named divergences**, each a refusal rather than an approximation:

- The content and pre-content classes (and with them every `*_PERM` event and
  the reply record a caller writes back) are `EOPNOTSUPP`; Linux serves them.
- Every `FAN_REPORT_*` shape and `FAN_ENABLE_AUDIT` are `EOPNOTSUPP` — Linux
  accepts `FAN_REPORT_TID`/`FAN_REPORT_NAME`-less shapes, and answers `EINVAL`
  for some combinations. This kernel has one record shape; naming the refusal
  is the honest half.
- `FAN_UNLIMITED_QUEUE`/`FAN_UNLIMITED_MARKS` are `EOPNOTSUPP`: the queue is
  capped at 16 384 records and the mark table at 4 096 per group, so accepting
  "never drop, never cap" would promise more than the bound keeps (a full
  queue reports one `FAN_Q_OVERFLOW` record instead of dropping silently).
- `FAN_MARK_MOUNT`/`FAN_MARK_FILESYSTEM` marks and the `IGNORED_*` shapes are
  `EOPNOTSUPP`: this tree has no inode-to-mount ancestry to match a mount mark
  against, and inventing one would mis-answer "is this object in that mount".
- `event_f_flags`' open-status bits are accepted as measured but not applied:
  a notification event's descriptor is this kernel's own read-only open of the
  object, so `O_WRONLY` there does not make it write-open.
- Removing a mark answers `ENOENT` when the object has none — the measured
  answer; `FAN_MARK_ONLYDIR` on a non-directory is `ENOTDIR` (the man page's
  answer; not measured, named here as such).
- The content classes' "no reply means allow" fallback is absent because the
  classes themselves are refused; a `write` on a group descriptor is `EINVAL`
  (the notification class has no response record to carry).

**Pinned** by `t-fanotify` (both machines; the gate's programs run as root,
which is what makes the served shapes reachable): the init descriptor's
`F_GETFL`/`F_GETFD` words, `FAN_CLOEXEC`/`FAN_NONBLOCK`, the accepted
`event_f_flags` set, the `EINVAL`/`EOPNOTSUPP` flag and mask tables, a
mark by path and by `dirfd`, `EBADF` for a non-group descriptor and a bad
`dirfd`, a child's open record (24 bytes, `mask=0x20`, `pid` = the child),
the short-buffer and `read(NULL,0)` `EINVAL`s with the record surviving the
short read, no event after `REMOVE` (`ENOENT` for the second removal),
`FAN_MODIFY`/`FAN_CLOSE_WRITE` from a write, `FAN_ACCESS`/`FAN_CLOSE_NOWRITE`
from a read-open, `FAN_ATTRIB` from a chmod, `FAN_OPEN_EXEC` from an exec, and
`EPERM` for a plain host user (the host run's one measurable half). Host tests
in `src/vfs/fanotify.rs` pin the flag/mask tables, the record layout and the
queue cap.

**Gate**: x86_64 `pass=221 fail=0`, aarch64 `pass=220 fail=0` (after the
documented `t-posix getpriority` flake re-run), `mpos-kunit 34 passed` both,
host suite 665 passed. `t-enosys` no longer probes the pair — its pin is
`t-fanotify` now — and the syscall table's refused block keeps only
`userfaultfd`, the three `io_uring` numbers and `rseq`.

Full record: `docs/uapi/` §5.1 item 114.

**Counterfactual** — both dispatch arms removed: x86_64 `pass=220 fail=1`,
only `t-fanotify` red (it panics because `fanotify_init` answers `ENOSYS`);
every other program stays green.

Full record: `docs/uapi/` §5.1 item 114.

### 3.10 `close_range(2)` — a span of descriptors, closed or marked — **DONE (2026-10-28)**

**Why**: a program that starts a child with exactly three descriptors - a
shell, a supervisor, anything that has inherited a dozen fds it does not want
to pass on - has two options: `close` every number in a loop (which is what
`busybox` and glibc's `posix_spawn` fall back to), or `close_range(2)`, which
does the loop in the kernel where the range can be validated once and the
teardown can happen under one walk. The call had no dispatch arm, so it fell
to `_ => ENOSYS` and every caller took the fallback loop.

**Measured first**, on Linux 7.2.11 (`build/scratch/closerange/probe.c`,
`probe2.c`; no root needed - unlike `fanotify_init` and `userfaultfd`, this
call asks no capability):

| call | Linux |
|---|---|
| `close_range(3, 4, 0)` with 3 and 4 open | `0`; 3 and 4 closed, 2 and 5 untouched |
| `close_range(10, 5, 0)` | `EINVAL` |
| `close_range(3, ~0, 0)` | `0`; every number from 3 up closed |
| `close_range(e, f, CLOSE_RANGE_CLOEXEC)` | `0`; both still **open**, both `FD_CLOEXEC`, and a descriptor outside the span unflagged |
| `close_range(900, 910, CLOEXEC)` over free slots | `0` - a free slot is not an error and creates nothing |
| `close_range(g, g, CLOSE_RANGE_UNSHARE)` | `0`; `g` closed |
| a **sibling thread**'s `close_range(fd, fd, 0)` | the process's number is closed - a thread group shares one table |
| a **forked child**'s `close_range(k, k, UNSHARE)` | `0` in the child, and the **parent's** `k` stays open |
| `close_range(3, 4, 8)` | `EINVAL` (a flag bit that is neither) |
| `close_range(10, 5, 8)` | `EINVAL` - two reasons at once, so the measurement does not say which is checked first |

**Landed shape.** `handle_close_range` in `src/syscall/linux/fs/file.rs`:

- the two flag bits are the only ones accepted (`EINVAL` otherwise), then
  `first <= last` (`EINVAL`), then the span is clamped to
  `FdTable::slot_count()` - the ABI's `last` is a `u32` and `~0` walked
  literally is four billion iterations for a process holding twenty
  descriptors;
- `CLOSE_RANGE_UNSHARE` calls `crate::sched::unshare_files()`, which is the
  same `FdTable::duplicate()` `unshare(CLONE_FILES)` makes - so the numbering
  becomes the caller's while the open file *descriptions* stay shared, which is
  the distinction that makes the flag worth having;
- `CLOSE_RANGE_CLOEXEC` goes through the new `FdTable::cloexec_span`, which
  marks only the slots that hold a descriptor;
- otherwise each number is closed through **`close_one`** - the body
  `handle_close` was split into, so a span close owes exactly what a single
  close owes: a socket's last descriptor takes its port down, an inotify
  instance and a fanotify group and a DRM master and an evdev grab and the
  description's own `flock`/`OFD` locks all die at their last reference. A
  second, thinner close path for ranges is the shape that would leave a port
  bound and a screen claimed by a process that had closed the number.

`FdTable` gained three methods (`slot_count`, `close_span`, `cloexec_span`),
pinned by three host tests: a span lifts every number it covers and no other,
a marked span keeps its descriptors open and creates none, and a span spelled
`u32::MAX` lifts exactly the table's slots.

**Named divergence.** Linux's own check order between the flag word and
`first > last` is not separated by the measurement (both are wrong in the same
call), and this kernel checks the flags first. Both answers are `EINVAL`, so no
caller can tell them apart - named rather than silently assumed.

**Pinned** by `t-close-range` (16 claims on both machines), whose thread and
fork claims are why it uses threads at all: without a sibling there is nothing
to share with, and `UNSHARE` would look like a no-op whether or not it did
anything. The same binary passes 16/16 under `MPOS_ON_HOST=1` against Linux -
the reference direction, which is what says the expectations are Linux's.

Full record: `docs/uapi/` §5.1 item 117.

### 3.11 The mount API's attachment half — **DONE (2026-10-28, partial)**

Linux's `mount(2)` grew a family beside it: `open_tree(2)` (428) hands back a
descriptor for a tree, `move_mount(2)` (429) attaches one tree somewhere else,
and `mount_setattr(2)` (442) changes a mounted tree's attributes. This kernel
answered all three `ENOSYS`, so a program holding a mount *handle* rather than a
path (a container runtime copying a root, a service adjusting its own mount)
had no move.

**Measured on Linux 7.2.11 first**, as uid 0 inside a user+mount namespace
(`build/scratch/mountapi/probe{2,3,4}.c`; musl's headers carry none of these
constants, so the probe writes them down):

`open_tree`: `0` and `OPEN_TREE_CLONE` both give an fd whose `fstat` says
`040755` and whose `F_GETFL` is `O_PATH`; `OPEN_TREE_CLOEXEC` marks it;
`CLONE|AT_RECURSIVE` and `CLONE|AT_NO_AUTOMOUNT` are accepted; `AT_RECURSIVE`
without the clone bit is `EINVAL`; unknown flags (`0x20000000`, `0x40000000`)
are `EINVAL`; a missing path is `ENOENT`, a NULL path `EFAULT`; an empty name
needs `AT_EMPTY_PATH` (else `ENOENT`). The descriptors differ in what
`/proc/self/fd` shows: a clone's link is `/`, a reference's link is the mounted
path.

`move_mount`: the flag mask is exactly the six `MOVE_MOUNT_*` bits; `F_EMPTY_PATH`
with a clone fd attaches it (the target then shows the source's inode), with a
non-clone fd of a non-mount answers `EINVAL`, with a non-clone fd **of a
mountpoint** answers 0; a path source moves a real mount and moving it back
works; a source that is not a mount is `EINVAL`; a target missing is `ENOENT`;
a target already carrying a mount is `EINVAL`.

`mount_setattr`: the structure size must be 32 (24 is `EINVAL`, 40 is `E2BIG`,
NULL with 32 is `EFAULT`); on a tmpfs mountpoint `attr_set` of
`RDO`/`NOSUID`/`NODEV`/`NOEXEC`/`NODIRATIME`/`NOIDMAP` is accepted while
`NOATIME`/`STRICTATIME`/`IDMAP` and bits outside the vocabulary are `EINVAL`;
the same bit in both `attr_set` and `attr_clr` is accepted; `AT_RECURSIVE`
and the four `*_AUTOMOUNTS`-style paths are accepted, unknown flag bits are
`EINVAL`; the target must be a mountpoint or arrive through `AT_EMPTY_PATH`.

**Served, in that shape.** `src/vfs/mountfd.rs` is the descriptor object
(`MountFd { root, display, source_path, attachable, read_only }`, an `Inode`
that answers the mount's own metadata, names itself with the path or `/`,
and answers `EBADF` to a read/write — an `O_PATH` subject is not a byte
stream). The three handlers live in `src/syscall/linux/fs/file.rs`
(`handle_open_tree`, `handle_move_mount`, `handle_mount_setattr`), with
`mountpoint_of_path` asking the table for the **mountpoint** rather than the
crossed tree, `mount_root_of` unwrapping a descriptor so a second `open_tree`
does not stack veils, and `MountTable::{read_only_of, holds_mount,
remount_below}` carrying the two facts a descriptor cannot: which mount it
belongs to (a target arrives as the point or as the covered root) and whether
the span below a mount is recursive.

**The read-only promise is refused rather than recorded**: a mount that says
`ro` while its filesystem goes on accepting writes is a lie, so
`mount_at`/`remount`/`bind_mount` now check `fs.read_only()` after asking the
filesystem to become read-only and answer `EOPNOTSUPP` when it did not.
`ext4` keeps that promise; `ramfs` (and so `tmpfs`) never had a read-only state
to keep, which means `mount -o ro` / `remount,ro` on a tmpfs now answers
`EOPNOTSUPP` where it used to answer 0 and stay writable. That is a behaviour
change, and it is the honest direction: `mount_setattr`'s `RDO` is the same
call on the same filesystems.

**Named divergences** (each a refusal, never a wrong answer): the superblock
quartet `fsopen`/`fsconfig`/`fsmount`/`fspick` (430–433) stays `ENOSYS` — this
kernel mounts a `FileSystem` object directly and has no `fs_context` or
superblock for those four to name; `NOSUID`/`NODEV`/`NOEXEC`/`NODIRATIME`/
`NOIDMAP` answer `EOPNOTSUPP` (Linux serves them; there is no per-mount storage
for them here); `propagation` and `userns_fd` answer `EOPNOTSUPP` (this kernel
has no mount propagation and no idmapped mounts).

**Pinned** by `t-mount-api` (new, both machines; the host run skips — an
ordinary user is refused all three with `EPERM`, which the pin prints). Its
claims are the measured table above plus this kernel's two refusals. Gates:
x86_64 `all 228 user tests passed`, aarch64 `all 227` (`mpos-kunit 34 passed`
both, host suite 687); the counterfactual (the three dispatch arms removed)
`CF-EXIT=1`, `pass=227 fail=1 (228 built)` with only `t-mount-api` red.

Full record: `docs/uapi/` §5.1 item 122.

### 3.12 An eventfd's pollers are woken by its write — **DONE (2026-10-29)**

The display frontier found this one sideways: a compositor that looked frozen
for thirteen seconds after it started, and whose whole startup appeared at once
when a signal interrupted its wait (the user's "Ctrl-C 时界面先正常出现"). The
kernel's own presentation path had been measured correct; the missing piece was
that a wake never reached the parked task.

**Measured in the guest first** (`build/scratch/gpu/wakeup.c`, a musl-static
probe, two threads): one thread writes `1` to an `eventfd` after 300 ms while the
main thread sits in an `epoll_wait` with no timeout of its own; the wait returned
at its **3000 ms timeout**, not at the write. The same shape on a pipe answered
in **0 ms** and on a `socketpair` in **1 ms**. A second round on the same
descriptor answered at once, because the counter was already above zero — the
lost wake is only visible on the first cross-thread event.

**The cause**: an `Inode` that is not always ready must register a poller in
`poll_subscribe` (the door `epoll_wait`/`poll`/`select` come through), and
`src/vfs/pipe.rs` does (`waiting_readers`/`waiting_writers`). `EventFd` kept its
own `waiters` list for the blocking `read_at`→`wait_nonzero` path only, and its
write woke that list — so a thread parked in `epoll_wait` was on no list at all
and only woke at its timeout. `src/vfs/eventfd.rs` now implements
`poll_subscribe`/`poll_unsubscribe` over the same list, so the existing write
wake reaches every waiter, blocking reader and poller alike.

**Why it mattered beyond one call**: `async-io` and `zbus` wake their event loop
with an eventfd, so a compositor's main thread parked in `epoll_pwait` looked
frozen while its other threads finished work nobody could act on. On the same
image after the fix, `niri` creates its `wl_output` during startup, presents two
client frames (resources `0x4` and `0x11`) and exits cleanly on `SIGINT`
(`N31-RC 0`); on the unfixed kernel it never exited (no `RC`, no `quitting due
to receiving signal SIGINT`, seatd never saw a disconnect).

**Pinned** by `t-eventfd` (both machines): an eventfd watched by an `epoll`, a
thread writing after 100 ms, and the wait required to report the event **before
its own 1500 ms budget** — so a lost wake fails the claim instead of passing
slowly. Host Linux is the reference (`MPOS_ON_HOST=1` 4 claims ok). The
counterfactual was measured in both directions on probe images: the new claim on
the unfixed kernel `FAIL: an eventfd write wakes an epoll park at once, not at
its timeout`, and on the fixed kernel `PASS`. Gates on the fixed tree: x86_64
`all 229 user tests passed`, aarch64 `all 228` (`mpos-kunit 34 passed` both,
stage 1 `1178 tests, 0 failed`).

**The survey this fix left open, and its measurement**: `timerfd` and
`signalfd` also keep no `poll_subscribe` list, so the question was whether a
park on one of them has a door. Measured with `build/scratch/gpu/tickwake.c`
(two cases in one guest probe): a `timerfd` armed for 150 ms and watched by
`epoll_wait(…, 3000)` answered at **3000 ms** — the timeout, not the
expiration — while a `signalfd` with a blocked `SIGUSR1` queued 100 ms in
answered in **0 ms**. So the signal machinery already wakes a park, and only
the clock's own arrival had nobody behind it. That half is §3.13.

### 3.13 A timerfd's expiration reaches an `epoll_wait` park — **DONE (2026-10-29)**

`poll(2)` has always kept a one-tick floor under every timeout
(`src/syscall/linux/fs/file.rs`'s `poll_wait`: `next = since_boot_nanos() +
ns_per_tick()`, the floor named on the same clock as the deadline for §6.23's
reason). `epoll_wait` did not: it parked to the caller's deadline alone, so a
wait whose only watched descriptor was a `timerfd` slept through the expiration
it was waiting for.

**Measured** (`build/scratch/gpu/tickwake.c`, musl-static, two cases in one
guest boot): a `timerfd_create(CLOCK_MONOTONIC)` armed for 150 ms and watched by
`epoll_wait(ep, &ev, 1, 3000)` reported `epoll_wait=1 events=0x1` after
**3000 ms** on the kernel before this change — the wait ended at its timeout and
only then noticed the timer. After the floor it reports the same event after
**153 ms**. The second case in the same probe — a `signalfd` with a blocked
`SIGUSR1` queued 100 ms in by a thread — answered in **0 ms on both kernels**, so
it needs no floor: the signal machinery wakes the park (which is also why §3.12's
compositor still saw `SIGINT`).

**The door the floor is hung from**: `Inode::ready_without_waker()` (default
`false`; `TimerFd` answers `true` — the clock's arrival is nobody's call to
announce), `Epoll::needs_tick()` (any watched descriptor answering it, asked once
per wait), and `epoll_wait`'s loop: with a floor, the park's deadline is
`min(caller deadline, now + ns_per_tick)`, and a floor tick that found nothing is
**not** the caller's timeout — it re-asks, and only the caller's own deadline
ends the wait with 0. Without such a descriptor the park is unchanged
(`u64::MAX` for an infinite wait).

**Pinned** by `t-timerfd` (both machines, its new epoll section, 6 claims): a
monotonic timerfd exists, an epoll instance watches it, it is in the interest
list, armed for 100 ms, **the expiration wakes the park inside a second**, and
the report is `EPOLLIN`. Host Linux is the reference (`MPOS_ON_HOST=1`, 6/6 ok).
The counterfactual was measured in both directions on probe images with the pin
binary injected: the pre-fix kernel `FAIL: the expiration wakes the park inside a
second` (it returned at its 2000 ms timeout), the fixed kernel passes the whole
section.

Full record: `docs/uapi/` §5.1 item 125.

### 3.14 `statmount(2)` and `listmount(2)` — the mount table describing itself — **DONE (2026-10-29)**

`open_tree`/`move_mount` (§3.11) give a program a mount *handle*; these two give
it the namespace's *inventory*. `statmount(2)` (457) answers one mount's
identity and settings into a 512-byte struct plus a string area, and
`listmount(2)` (458) walks the namespace's mounts as ids. Both answered `ENOSYS`
here, so `findmnt`-style tools and every container runtime that maps a mount set
had no way to read this kernel's table - the very table the mount family was
already reading through paths.

**Measured on Linux 7.2.11 first**, as uid 0 inside a user+mount namespace
(`build/scratch/mountstat/probe.c`, `probe2.c`; both calls want `CAP_SYS_ADMIN`,
an ordinary user gets `EPERM`):

`listmount`: `LSMT_ROOT` (`u64::MAX`) walks the namespace in tree order, root
first then depth-first; `param` resumes **after** the id given (`param = ids[2]`
answered `ids[3]`, `param = last` answered 0); `LISTMOUNT_REVERSE` walks
backwards; a small `nr` is a short answer of exactly that many ids, `nr = 0`
writes nothing and answers 0, `nr` past the end answers the rest; any flag
outside the vocabulary, `mnt_id != LSMT_ROOT`, and `req.size = 20` are `EINVAL`;
a null id buffer is `EFAULT`; 36 mounts in that namespace.

`statmount`: `struct statmount` is 512 bytes followed by the string area, and a
string field is an **offset into that area**, offset 0 being the empty string -
a mount whose root is the filesystem's root reports `mnt_root` as `""` that way.
`mnt_id` must be the **unique** id (`statx`'s `STATX_MNT_ID_UNIQUE`,
`listmount`'s output): the reused 32-bit `/proc/*/fdinfo` number was `EINVAL`.
The fd form (`flags = STATMOUNT_BY_FD`) names a mount by a descriptor *on* it
and then the struct's `mnt_id`/`mnt_ns_id` must be zero; without the flag the
same struct is `EINVAL`. `mask` reports the fields **written**, not the fields
asked: bit 31 and `0xffffffff` both answered 0 with the mask carrying nothing
unknown, and `supported_mask` said what the kernel could produce. A full answer
on a tmpfs was 581 bytes, `mask 0x12fb`, `supported_mask 0x7fff`, `sb_magic
0x1021994`, `mnt_propagation 0x40000` (private), point at offset 1 (`"/tmp\0"`).
`bufsize` shorter than the fixed part is **truncated**, not refused
(`bufsize = 16` answered 0 with `size = 16` and `mask = SB_BASIC`), while a
requested *string* with no room in the area after the struct is `EOVERFLOW`
(`bufsize = sizeof` with `MNT_POINT` asked, 75); `bufsize = 0` answers 0 having
written nothing.

**Served, in that shape.** `src/vfs/mountstat.rs` is host-compiled and pure: the
two request versions (24 and 32 bytes, by their own first word, 20 and 40
refused), the mask vocabulary, `sb_magic` for the filesystems this kernel
mounts, and the renderer that writes Linux's exact offsets and one NUL-first
string area. `MountTable::record_of_id` answers a record by the table's own
attachment id (which **is** the unique id here - the table never reuses one, so
the id `statmount` names is the id `listmount` walked), and
`MountTable::record_containing` answers the mount a descriptor is on (the
longest `mountpoint_covers` prefix of its path), which is the fd form's answer.
The handlers in `src/syscall/linux/fs/file.rs` (`handle_statmount`,
`handle_listmount`) fill the struct, apply the two measured refusals
(`EOVERFLOW` only for a requested string that no longer fits, truncation
otherwise), and write the id array.

**Named divergences.** The mask is honest about what this kernel does not have:
`MNT_OPTS`, the option arrays, `mnt_root` for a bind, and the uid/gid maps are
never set (Linux answered `mask = 0` for the maps too), and `supported_mask`
names the six fields that are always renderable (`MNT_BASIC`, `MNT_POINT`,
`FS_TYPE`, `MNT_NS_ID`, `SB_SOURCE`, `SUPPORTED_MASK`). The old numbering
(`mnt_id_old`/`mnt_parent_id_old`) is left zero because this kernel has no
reused, fdinfo-visible id space. `mnt_attr` reports `MOUNT_ATTR_RDONLY` exactly
when the mount records read-only and 0 otherwise (§3.11's promise: a mount that
records read-only is one its filesystem honours), propagation is always
`MS_PRIVATE` (this kernel has one mount namespace per process and no shared
peer group), and a foreign `mnt_ns_id` is `EOPNOTSUPP` - a namespace id this
caller cannot name rather than a lie about the caller's own table.

**Pin**: `user/tests/src/bin/t-statmount.rs` - the request versions, the mask
the kernel writes, the string offsets (including the empty root), the unique id
and its parent, the tmpfs magic, the stable nonzero namespace id, the
unknown-bit answer, truncation vs `EOVERFLOW` vs zero-length, the id/flags
refusals, the descriptor form, and `listmount`'s tree order / resume / reverse /
short answers / `EINVAL`s / `EFAULT`, ending with the unmount taking the mount
out of both calls. It skips on the host (an ordinary user is `EPERM`) and runs
on both machines.

Gate: `cargo xtask test` - x86_64 `pass=230 fail=0 (230 built)`, aarch64
`pass=229 fail=0 (229 built)`, `mpos-kunit 34 passed, 0 failed` on every lane;
counterfactual with the two dispatch arms removed: only `t-statmount` red.

Full record: `docs/uapi/` §5.1 item 129.
