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

### 3.15 The extended-attribute family keeps a store on the memory filesystems — **DONE (2026-10-30)**

The family (188–199: `setxattr`/`getxattr`/`listxattr`/`removexattr` and their
`l`/`f` forms) was wired to the argument checks and then answered
`EOPNOTSUPP` — the honest report for a filesystem with no xattr support, and
the reason `fastfetch`-style probing callers stopped seeing `ENOSYS`. But it
was the *whole* answer, and it is the wrong one for the filesystems this kernel
actually keeps in memory: `ramfs`/`tmpfs` are where Linux stores `user.*`
attributes, so a caller asking a tmpfs for an attribute must be told whether the
name is set, not that the medium cannot hold one.

**Measured on Linux 7.2.11 first** (`build/scratch/xattr/probe*.c`, copied to
`.workbuddy/probe_xattr*.c`; uid 1000, a tmpfs file under `/dev/shm`, with an
ext4 file beside it for the comparison):

| question | measured answer |
|---|---|
| `setxattr("user.a", "v1", 2, 0)` on a tmpfs file, dir, unix socket | `0` on all three |
| `getxattr(name, ptr, 0)` | the value's length; the pointer is never read (a `(void*)1` probe answers the same) |
| a buffer one byte short | `ERANGE` |
| an attribute nobody set | `ENODATA` (61) |
| `XATTR_CREATE` on an existing attribute | `EEXIST` (17) |
| `XATTR_REPLACE` on a missing one | `ENODATA` |
| **both bits together** | **not `EINVAL`** — the existence rules apply, so `EEXIST` when it is there and `ENODATA` when it is not (the man page's "contradictory pair is `EINVAL`" is wrong) |
| a bit outside the vocabulary (`0x4`, `0x8`) | `EINVAL`, decided **before** the path is walked |
| value 65536 bytes / 65537 / 70000 | `0` / `E2BIG` (7) / `E2BIG` — and `E2BIG` is decided before the pointer is looked at |
| value pointer this kernel cannot read | `EFAULT` |
| empty value (size 0) | a value: set `0`, the length probe `0` |
| name 256 bytes / empty name / a bare prefix (`"user."`) | `ERANGE` / `ERANGE` / `EINVAL`, all before the path is walked |
| `listxattr` | names end to end, **each NUL-terminated** (one attribute of six bytes lists as seven), no trailing separator beyond that; probe = exact size, one byte short = `ERANGE`, size 0 ignores the pointer |
| `getxattr`/`listxattr` with a 70000-byte buffer | served — there is no size cap in this direction; only `setxattr`'s value is capped |
| `user.*` on a FIFO, a device node, a symlink via `l` | set `EPERM` (1); get `ENODATA`; list `0` |
| `user.*` on a directory, regular file, unix socket | set `0`; get/list carry the names |
| the following form through a symlink | reaches the target |
| `f*` forms over `O_RDONLY` and directory descriptors | served; an `O_PATH` descriptor is `EBADF` |
| `system.foo`, `foo.bar` | `EOPNOTSUPP` (95) — Linux's "no handler for this namespace" answer |
| `security.foo`, `trusted.foo` | stored by root, `EPERM` for uid 1000 (tmpfs keeps both, because it has LSM hooks and per-caller visibility) |
| ext4, `user.*` | same answers as the tmpfs for `user.*`; `security.`/`trusted.` `EPERM` for uid 1000 |

**Served, in that shape.** `src/vfs/xattr.rs` is the store and it is
host-compiled: `Vault` is one `critical_section::Mutex<RefCell<Vec<Entry>>>` per
inode, `classify` is the name/namespace check, `check_flags` the flags word, and
the module's eleven host tests pin the round trip, both probes, `ERANGE`,
`ENODATA`, the two flags bits, the 64 KiB edge and the empty value. `Inode`
grew one door, `fn xattr_vault(&self) -> Option<&Vault>` (default `None`), which
`RamFile`, `RamDir` and `RamSocket` answer with their own store; `RamFifo`,
`RamDev` and `RamSymlink` keep the default because the generic layer's per-kind
rule refuses them before any store is asked.

The handlers in `src/syscall/linux/fs/file.rs` were rewritten to the measured
precedence: `setxattr`'s flags word, then the value's size, then the name
(`ERANGE`/`EINVAL`), then the path, then the namespace, then the kind, then
whether the filesystem keeps a store, then the pointer. Three savings fall out
of that order: a bad flags word or name never pays for a walk (measured: the
same calls answer `EINVAL`/`ERANGE` on a path that does not exist), and a
filesystem with no store (`ext4`, `procfs`, `devfs`) preserves the old
`EOPNOTSUPP` for a request whose arguments are sound — so the two contracts
compose instead of one replacing the other.

**Named divergences.** (1) Only `user.*` is kept: Linux's tmpfs stores
`security.*` (world-readable) and `trusted.*` (root-only), and this kernel has
neither an LSM nor per-caller visibility to keep those properties honest, so
both answer `EOPNOTSUPP` — Linux's own answer for a namespace with no handler.
(2) `open(O_PATH)` is not a descriptor shape this kernel makes (the flag is
carried through `F_GETFL` like any other), so Linux's `EBADF`-over-`O_PATH` rule
has nothing to catch; the pin therefore does not claim it. (3) A *negative*
value pointer with a size of 0 is not read — the empty value is stored, so a
`setxattr` with `(void*)1, 0` succeeds where a filesystem without a store
answered its own refusal. (4) Nothing here charges an attribute to a mount's
usage: the precedent is `RamFs`'s directory entries, which are uncharged too.

**Pin**: `user/tests/src/bin/t-xattr.rs`, rewritten from the refusal pin to the
new contract (44 claims): the store and its probes, both flags bits and their
measured non-`EINVAL` combination, the 64 KiB edge, every name refusal and its
precedence over the walk, the list encoding, the per-kind refusals, the fd
forms, procfs's `EOPNOTSUPP`, and a `mount -t tmpfs` whose file stores
`user.*` too. Its host branch carries the same claims against real Linux with
the three named divergences accepted there (`ours_or_linux`), so the program
measures Linux's shape rather than this kernel's special case.
x86_64 `all 231 user tests passed` (29.32 s, `mpos-kunit 34 passed`), aarch64
`all 230 user tests passed` (25.41 s, `mpos-kunit 34 passed`).
Counterfactual — `xattr_vault` answering `None` for every inode — is
`pass=230 fail=1` (231 built): `t-xattr` alone, 15 of its 44 claims red — every
one of them a store claim — while the flags, name and namespace refusals stayed
green, which is the shape the old pin had.

Files: `crates/err` (`ENODATA`), `src/vfs/xattr.rs` (new), `src/vfs/mod.rs`
(`xattr_vault`), `src/vfs/ramfs.rs` (three stores), `src/syscall/linux/fs/file.rs`
(the handlers), `user/tests/src/bin/t-xattr.rs`, and the uapi/roadmap rows.
### 3.16 The mount API's superblock half — **DONE (2026-10-30)**

The mount family landed in two halves. The *attachment* half (`open_tree`,
`move_mount`, `mount_setattr`, item 122, §3.11) and the introspection pair
(`statmount`/`listmount`, item 129, §3.14) were already served; the *superblock*
half - `fsopen`/`fsconfig`/`fsmount`, the three calls that name a filesystem and
speak its options *before* there is a place to put it - answered `ENOSYS`, with
the note that "this kernel mounts a `FileSystem` object directly and has no
`fs_context` for those four to name".

That note was true of the code and wrong as a conclusion. `mount(2)` here
already builds the object and hangs it on a target in one function
(`mount_new`'s type match plus `MountTable::mount_at`); what it had no *door*
for was the middle state - a filesystem that exists and is attached nowhere. So
the item is the same model with the two ends cut apart, not a second model.

**Measured on Linux 7.2.11 first** (`build/scratch/mntapi/p{1,2,3,4,5}.c`, built
`gcc -O1 -static`, run through `sudo -n unshare -m --propagation private`:
`fsopen` wants `CAP_SYS_ADMIN`, so an ordinary user's first answer is `EPERM`
and the reference needs the mount namespace; the probes are archived as
`.workbuddy/probe_mntapi*.c`):

| call | Linux's answer |
|---|---|
| `fsopen("tmpfs", 0)` | a descriptor; `/proc/self/fd/N` → `anon_inode:[fscontext]`, `fstat` = 0-byte `0600` file, `fcntl(F_GETFL)` → `O_RDWR` |
| `read(2)` / `write(2)` on that context | `ENODATA` (61) / `EINVAL` (22); `pread`/`lseek` → `ESPIPE` (29) |
| `fsopen("nonexistentfs", 0)` | `ENODEV` (19) |
| `fsopen(..., 0x1)` / `(..., 0x8000_0000)` | accepted (`FSOPEN_CLOEXEC`, and the flag is *set*) / `EINVAL` |
| `fsconfig(SET_STRING, "size", "1M")` on a tmpfs context | 0 (tmpfs reads it) |
| `fsconfig(SET_STRING, unknown key)` / `SET_STRING` with a null value | `EINVAL` / `EINVAL` |
| `fsconfig(SET_BINARY)` / `SET_FLAG("size")` | `EINVAL` / `EINVAL` |
| `fsconfig(command 9)` | `EOPNOTSUPP` (95) |
| `fsconfig(CMD_CREATE)`, then any `SET_*` or a second `CREATE` | 0, then `EBUSY` (16) |
| `fsconfig(CMD_RECONFIGURE)` with an option, on a created tmpfs context | `EINVAL` |
| `fsmount` before `CMD_CREATE` / a second `fsmount` from one context | `EINVAL` / `EBUSY` |
| `fsmount`'s flag word | only `FSMOUNT_CLOEXEC` 0x1 and `FSMOUNT_NAMESPACE` 0x2; anything else `EINVAL` |
| `fsmount`'s *third* argument (mount attributes) | `RDONLY`/`NOSUID`/`NODEV`/`NOEXEC`/`NODIRATIME` accepted; `MOUNT_ATTR_IDMAP` `EINVAL` (no idmapped mounts) |
| the descriptor `fsmount` answers | `fstat` = the root directory (`041777` for tmpfs), `read(2)` = `EBADF` (9), `fsconfig` on it = `EBADF` |
| `move_mount(desc, "", …, MOVE_MOUNT_F_EMPTY_PATH)` | 0, and the name then answers the new filesystem's `st_dev` |
| `fspick(path)` on a mount point / a non-mount | a descriptor / `EINVAL`; `fsmount` of it `EBUSY`; `fsconfig(CMD_RECONFIGURE)` with nothing → 0 |
| `fsopen("ext4")`, `SET_STRING("source","/dev/null")`, `CMD_CREATE` | 0, then `ENOTBLK` (15) |

**What landed.** `fsopen`(430) names one of the twelve types `build_filesystem`
can build and answers a `FsContext` descriptor (`src/vfs/fscontext.rs`); the
descriptor reports a 0-byte file, `read(2)` is `ENODATA` and `write(2)` `EINVAL`
- the shape Linux gives it. `fsconfig`(431) speaks one `(key, value)` into the
context and `FSCONFIG_CMD_CREATE` builds the `FileSystem` through the *same*
factory `mount(2)` uses (the match was extracted out of `mount_new` into
`build_filesystem`, so the two doors cannot drift). `fsmount`(432) spends the
context once and answers a mount descriptor (`MountFd::fresh`) that carries the
never-mounted filesystem, its type, its source and its read-only bit;
`move_mount` then attaches it with `MountTable::mount_at` - a *first* mount, not
a bind, because `bind_mount` requires a source already served by a mounted
filesystem, which is exactly what this object is not. `fspick`(433) stays a
`ENOSYS` gap: what it picks a mount *for* is reconfiguration, and no filesystem
in this kernel can be reconfigured (Linux's own tmpfs answers `EINVAL` to a
reconfiguration that carries an option).

The state machine is the measured one and lives in the object rather than in
the syscall layer: an option after the creation is `EBUSY`, a second creation
`EBUSY`, `fsmount` before the creation `EINVAL`, a second `fsmount` `EBUSY`, and
a context closed before creation costs nothing (nothing was built).

**Two named divergences, and one that is not.** (1) An option this kernel does
not read is refused `EINVAL` rather than recorded into a data string nothing
parses - so a tmpfs `size=` was `EINVAL` where Linux accepts it until item 139
made the two numbers real (`§3.18`). That was
`mount(2)`'s own existing silence about the option made explicit; accepting it
would be the "returning success for something not implemented" rule 2 forbids,
and the alternative (implementing a tmpfs size budget through this door) is a
filesystem option the tmpfs does not take today. (2) No filesystem here takes
an option by descriptor, path or byte blob, so `SET_FD`/`SET_PATH`/
`SET_PATH_EMPTY`/`SET_BINARY` are `EINVAL` by name (Linux's own answer for a
type with no handler for the command). (3) Not a divergence but a fix found on
the way: a mount descriptor's `read(2)` used to be `EISDIR`, because its `fstat`
reports the root directory and the directory rule answered before the
descriptor could; Linux answers `EBADF` there (measured for both the `fsmount`
descriptor and the `open_tree` clone this kernel already served), so `Inode`
gained `refuses_file_io()` and the read door asks it before the kind. The same
answer now comes from both descriptors.

**Pin**: `user/tests/src/bin/t-fsmount.rs` (21 claims - the context's shape, the
state machine's refusals, the descriptor's directory/`EBADF` shape, the attach
and the device the attached name answers). Its claims were first run *as a
reference against real Linux as root* (`sudo -n`, no `MPOS_ON_HOST`): every
claim agreed except the deliberate `size=` refusal, which was the point of it then (`item 139`
made that claim Linux's: `an option tmpfs reads moves its room`).
`t-mount-api`'s old quartet claim changed with the behaviour - it now asserts
the trio is served and `fspick` is still `ENOSYS`.

**Gates**: x86_64 `all 234 user tests passed`, `mpos-kunit 34 passed, 0 failed`;
aarch64 `all 233 user tests passed`, `mpos-kunit 34 passed, 0 failed`
(`build/scratch/fsmount-gate4.log`, `fsmount-gate-a1.log`). **Counterfactual**, two of them, and the first attempt is worth recording:
disabling `move_mount`'s fresh-attach branch (`fresh.filter(|_| false)`, which
falls back to the bind path) left the whole sweep **green** - a bind of the new
filesystem through an empty source name also resolves, so that branch is not
what the attach claim measures. The two stubs that do discriminate:
(1) the three dispatch arms put back to `ENOSYS` → `t-mount-api`'s `fsopen`
claim and `t-fsmount`'s first claims red, and the sweep reports 2 of 234
(`build/scratch/fsmount-cf2.log`); (2) `refuses_file_io()` skipped in the read
door → exactly one claim red, `read(2) on a mount descriptor is EBADF (is
-21)`, everything else green (`build/scratch/fsmount-cfb.log`) - the fix half
is pinned as sharply as the feature half.

Files: `src/vfs/fscontext.rs` (new), `src/vfs/mountfd.rs` (`FreshMount`, the
no-byte-stream door), `src/vfs/file.rs` (the read door asks it first),
`src/vfs/mod.rs` (`NoData`, `as_fs_context`, `refuses_file_io`), `src/uaccess.rs`
(`ENODATA`), `src/syscall/linux/fs/file.rs` (`build_filesystem` extracted, three
handlers, `move_mount`'s fresh branch), both `nr_*.rs` (430-432),
`user/tests/src/bin/t-fsmount.rs` (new pin), `user/tests/src/bin/t-mount-api.rs`
(the quartet claim), and the uapi/roadmap rows.

### 3.17 A node with no attributes refuses the change, it does not accept it — **DONE** (2026-10-30)

Item 137 named this divergence and deliberately did not widen its own fix: `chmod` on
a procfs node answered `0` here where Linux answers `EPERM`. The tempting rule — "a
filesystem that keeps no modes refuses `chmod`" — fails its own measurement, because
sysfs keeps no modes either and answers differently depending on who asks.

**Measured first** (Linux 7.2.11, `build/scratch/attrrefuse/p1.c`, once as uid 1000
and once as root; every door in the `chmod` family, then `chown` and `utimensat`):

| target | uid 1000 | root |
|---|---|---|
| `chmod` / `fchmod` / `fchmodat(fd, "", AT_EMPTY_PATH)` on `/proc/self/status` | **EPERM** | **EPERM** |
| the same three on `/proc/sys/vm/max_map_count` | **EPERM** | **EPERM** |
| the same three on `/proc/self` (a directory) | **EPERM** | **EPERM** |
| the same three on `/sys/kernel`, `/sys/fs/cgroup` | EPERM | **0** |
| the same three on `/dev/null`, `/tmp` | EPERM (not the owner) | **0** |
| `chown(self)` / `fchown` / `utimensat` on `/proc/self/status` | 0 / 0 / 0 | 0 / 0 / 0 |
| the same three on `/proc/sys/vm/max_map_count` | EPERM / EPERM / EPERM | EPERM / EPERM / **0** |

The reading that matters: `EPERM` on procfs is **the node's answer, not a permission
verdict** — root reads the same `EPERM` an ordinary user does, because procfs's inodes
have no `setattr` operation at all. On sysfs the `EPERM` an ordinary user gets is the
ordinary owner/capability rule, and root's `0` there is the same question answered the
other way. Two questions, one of which this item can answer honestly and the other of
which it must leave named.

**Landed**: `Inode::refuses_mode_change()` — default `false`, and the question is asked
of the **inode** rather than the filesystem precisely because sysfs and procfs disagree
while both keeping no modes. procfs's five inode types (`ProcFile`, `ProcCtl`,
`ProcLink`, `ProcDir`, `ProcConfig`) answer `true`; `chmod_mode` (`src/syscall/linux/
fs/file.rs`) asks it immediately after the immutable check and **before** `may_setattr`,
so all four doors (`chmod`, `fchmod`, `fchmodat`, `fchmodat2`) answer `EPERM` without
ever consulting the caller — which is what makes root's answer the same as a user's.

**Pin**: `t-proc-modes` grows eleven claims, in the place that program is about: the
`EPERM` of `chmod` on `/proc/self/status` and `/proc/sys/vm/max_map_count`, the word
each reports afterwards (an answer of `0` that writes nothing is the shape this pins
against), `fchmod` and the `AT_EMPTY_PATH` spelling through a descriptor, and a tmpfs
control that the same call *is* accepted on a filesystem that keeps bits — so a blanket
refusal fails this program rather than passing it. All green on a current host first
(`MPOS_ON_HOST=1` against Linux 7.2.11), then on both machines.

**Gates**: x86_64 `all 236 user tests passed` / `mpos-kunit 34 passed, 0 failed`
(verdict `pass=236 fail=0`, `X-EXIT=0`, `build/scratch/attr-gate2.log`); aarch64 `all 235 user tests passed` / `mpos-kunit 34 passed` (`A-EXIT=0`, total 31.71 s, `build/scratch/attr-gate-a4.log`; the first two aarch64 attempts failed on stage 1 alone - the documented host-suite load flake, 706 tests green standalone - and one more on the documented console-RX kunit flake, `arch.console_receive` + `ipc.tty`; the re-run quoted above).

**Counterfactual**: the funnel's answer disabled (`if false && inode.refuses_mode_change()`) -> `t-proc-modes` alone red, `pass=235 fail=1` of 236, with exactly the four `EPERM` claims red - `chmod on /proc/self/status is EPERM … (got 0)`, `chmod on /proc/sys/vm/max_map_count is EPERM … (got 0)`, `fchmod on a proc file's descriptor is EPERM (got 0)`, `the AT_EMPTY_PATH spelling is EPERM too (got 0)` - while the two word claims and the tmpfs control stayed green, which is the point of claiming the control (`build/scratch/attr-cf.log`).

**What stayed named here** (closed by §3.19, item 140): `chown` and `utimensat` on a
procfs node answered `0` at this point; the two doors' refusals are the node's answer
rather than a store, and §3.19 landed them. sysfs's own `chmod` half — root's is `0`
and an ordinary user's is `EPERM` there, where this kernel answers `0` to both and
stores nothing — stays named in `docs/uapi/` §5.2.

**A lesson about the instrument.** The probe above ran its matrix as root, and the
matrix included `chmod("/tmp", 0640)`. It did what it was told: the development host's
`/tmp` lost its sticky bit and its `1777` for the length of a turn, and every command
the harness spawned failed in its own temporary directory before it could start. An
instrument that changes the machine it runs on is aimed at a path it created under
`build/scratch/`, never at a directory the machine shares with everything else.

### 3.18 A memory filesystem's room is a number in its option string — **DONE** (2026-10-30)

`/tmp`'s byte budget (§8.0, `t-fill-ramfs`) was a fixed quarter of the machine and
nothing else: a mount could not be told how much room it had, and the number of
*names* it could hold was not a number at all. That second half is the same hole the
first one closed, one door over — a directory of empty files charges the mount no
bytes and costs one inode of kernel heap each, so a `create` loop asked the heap for
memory through a path with no bound on it, and the answer to a heap with nothing left
is a panic. Linux's tmpfs has both words in its option string and answers `ENOSPC` at
either.

**Measured first** (Linux 7.2.11; `mount(8)` is the instrument because it drives the
new mount API, so its errors are `fsconfig()`'s):

| option | measured |
|---|---|
| default | `df` shows **16 GiB** of a 32 GiB machine (half of memory); `df -i` shows 4 014 765 inodes (half the pages) |
| `size=1M` | `statfs` = 256 4 KiB blocks; sixteen 64 KiB writes landed, the seventeenth is **`ENOSPC`**, and the file is exactly 1 048 576 bytes |
| `size=4k`, `size=2m`, `size=1G`, `size=50%` | all move the number (`4k` → 4 096 bytes, `50%` → 16 444 481 536) |
| `size=0` | accepted: a mount with no room |
| `size=abc`, `size=-1` | refused, and the refusal names the call: `fsconfig() failed: tmpfs: Bad value for 'size'` |
| `nr_inodes=8` | the root counts: `df -i` shows 8 total, 7 free; the eighth name is **`ENOSPC`**; an `unlink` makes room for one more; a directory, symlink and FIFO are names too |
| `nr_inodes=0` | accepted, and behaves as "no number given" (the default) |
| `frobnicate=1` | refused: `tmpfs: Unknown parameter 'frobnicate'.` |

**Landed**: `Usage` carries a second ledger (`inodes`/`inode_limit`) beside its bytes,
charged before a node is built and given back when the last reference to the node
goes — which is Linux's shape (`shmem_free_inode` runs on eviction, not on `unlink`,
so an unlinked-but-open file is still counted) and is what keeps an `open`+`unlink`
loop from hiding its inodes from the ledger. Every inode type in the filesystem holds
the mount's `Arc<Usage>` and releases on `Drop`; the mount's own root is charged too,
because Linux counts it (a `nr_inodes=8` mount held the root plus seven names). The
default inode ceiling is **half the machine's pages**, Linux's default, and each charge
also asks `machine_has_room` for the node's few hundred bytes so a create loop cannot
take the last frame — the byte ledger's own argument, made for metadata.

`usage_from_options` parses the option string with Linux's `memparse` spellings (`k`,
`M`, `GiB`, …), `size=N%` as a share of physical memory, and `size=0` as a room of
zero; `nr_inodes=0` is the default. `mode=`, `uid=` and `gid=` are accepted and not
stored (this kernel's tmpfs root is `01777` because that is what `/tmp` is, and a
mount's ids are whoever created it — a named divergence). An unknown key, a bare
word, or a value that is not a number is `EINVAL`; a value is refused **at the
`fsconfig` call that carries it**, which is where Linux refuses it, not later at
creation.

Both doors pass the string: `mount(2)`'s `data` argument (which used to be ignored
for tmpfs) and `fsconfig(2)`'s recorded options (whose key gate now admits tmpfs's
six). That last one closes the divergence item 135 wrote down — "a tmpfs `size=` is
`EINVAL` here where Linux accepts it" — and `t-fsmount`'s claim moved with it from
"an option no filesystem reads" to "an option tmpfs reads moves its room" plus "a
size that is not a number is `EINVAL`".

**Pin**: `t-tmpfs-limits` (new). Claims: a `size=64k` mount takes exactly 64 KiB and
the next write is `ENOSPC`; an `nr_inodes=8` mount holds its root plus seven names
and the eighth is `ENOSPC`; a name removed makes room for one more; a directory is a
name the ceiling counts; `size=abc`/`size=-1`/`frobnicate=1` are `EINVAL`; and a mount
with no options is the default room. Its privileged claims are checked as a reference
against real Linux first (as root: every claim `ok:`), and skip with one passing note
on a host run without privilege.

**Host tests**: `the_option_string_names_the_mounts_room` (1 MiB / 4k / 2MiB / 50% /
0 / `nr_inodes=0` = default / six refusals) and
`a_name_past_the_inode_budget_is_enospc_and_one_given_back_fits` (a four-inode mount
holds root plus three, `ENOSPC` on the fourth, `unlink` frees one, a hard link is
*not* charged — another name for the same inode).

**Gates**: x86_64 `all 237 user tests passed` / `mpos-kunit 34 passed, 0 failed` (verdict `pass=237 fail=0`, `X-EXIT=0`, `build/scratch/tmpfs-gate4.log`); aarch64 `all 236 user tests passed` / `mpos-kunit 34 passed` (`A-EXIT=0`, `build/scratch/tmpfs-gate-a1.log`).

**Counterfactual**: the tmpfs arm back to `RamFs::new()` (the option string ignored, as it was before this item) -> `t-tmpfs-limits` alone red, `1 of 237`, with exactly its five discriminating claims red - `a size=64k mount takes exactly its 64 KiB and then answers ENOSPC`, `an nr_inodes=8 mount holds its root plus seven names, then ENOSPC`, and the three `mount refuses … with EINVAL` claims (an ignored option is accepted by definition) - while the control claims stayed green (`build/scratch/tmpfs-cf.log`).

**What the item does not claim.** The default *byte* ceiling is still a quarter of
memory rather than Linux's half, for the reason `Usage`'s doc gives (no swap, and the
page cache is the kernel heap here); and `nr_inodes`' ceiling counts nodes, not the
names their directories hold, so a mount can hold fewer names than inodes would
suggest when a name is shared by `link(2)` — Linux counts the inode once there too,
which is why a hard link is not charged.

### 3.19 A node that keeps no owner and no clocks refuses or drops, as Linux does — **DONE** (2026-10-30)

Item 138 gave a `chmod` on a procfs/sysfs node its refusal (`EPERM`, the node's own
answer, for root too) and named the other two attribute doors as still open: `chown(2)`
and `utimensat(2)` answered `0` everywhere, because this tree keeps no owner and no
timestamps to write. Linux has three different answers there, and which one a node
gives is a property of the node rather than of the caller.

**Measured on Linux 7.2.11 first** (`build/scratch/attr2/p1.c` for `chown`, `p2.c` for
the three `utimensat` shapes; run twice, as uid 1000 and as root; `status` = `EPERM`,
`denied` = `EACCES`):

| node (mode) | `chown(self)` | times "now" | times explicit | times null |
|---|---|---|---|---|
| `/proc/sys/vm/max_map_count` (`0644` root) | root **`EPERM`** / user `EPERM` | root `0` / user `denied` | root `0` / user `EPERM` | root `0` / user `denied` |
| `/proc/self` pid directory (`0555`) | **`EPERM`** for both | `EPERM` for both | `EPERM` both | `EPERM` both |
| `/proc/self/status` (`0444`, owned by the reader) | `0` both | `0` both | `0` both | `0` both |
| `/proc/uptime` (`0444` root) | `0` root / `EPERM` user | `0` / `denied` | `0` / `EPERM` | `0` / `denied` |
| `/sys/kernel` directory (`0755` root) | `0` root / `EPERM` user | `0` / `denied` | `0` / `EPERM` | `0` / `denied` |
| `/dev/null` (`020666` root) | `0` root / `EPERM` user | `0` / `0` | `0` / `EPERM` | `0` / `0` |
| a tmpfs file owned by the caller | `0` both | `0` both | `0` both | `0` both |

Three readings, and they are the design:

* a `/proc/sys` file **refuses the owner** (its inode has no `setattr`, so root reads
  what a user reads) and **drops the timestamps** (all three shapes are `0` to root);
* a pid directory **refuses both** - it mirrors a process, and Linux's mirror has no
  `setattr` either;
* everything else - a sysfs directory, a `/proc` file, a device node - **drops both**,
  and who may ask is the ordinary question on top: an ordinary user's timestamps on a
  `0444` node are `EACCES` for the "now" shapes and `EPERM` for an explicit pair
  (`setattr_prepare`'s two arms), and on a `0666` device node the "now" shape is `0`
  because write permission stands in for ownership.

This kernel before the change (measured by the temporary `t-proc-attrs-diag`, run in
the sweep and deleted again): `chown` `0` on every node including the two Linux refuses,
and `utimensat` **`EPERM`** on every node but `/dev/null`, because the node's default
`set_times` is a refusal and nothing overrode it.

**Landed.** Two trait doors beside item 138's `refuses_mode_change`:
`Inode::refuses_owner_change()` and `Inode::refuses_times_change()`, asked by their
handlers *before* the caller's standing because each is the node's answer. On procfs:
true for the owner on `ProcCtl` and on the mirroring pid directory, true for the times
on the mirroring directory only, and `set_times` answering `Ok(())` (accepted, dropped)
on the node types that accept it - `ProcFile`, `ProcCtl`, `ProcLink`, `ProcDir`. On
sysfs all five node types accept the timestamps the same way. The mirror is a flag on
`ProcDir` (`mirroring()`, set by `process_dir`) rather than "the directory has generated
children": `/proc` itself has those and Linux does not report it as a pid directory.
`may_touch` gained the rule the item needed: for a node that *reports* a mode without
*keeping* one, the write-permission arm asks the reported word (`may_by_mode`) instead
of `may_as`'s "not policed", which is what makes an ordinary user's `0444` answer
`EACCES` rather than `0`. `may_setattr` needed no change - it asks ownership, and the
reported owner is root's.

**Pin**: `user/tests/src/bin/t-proc-modes.rs` grew an `owner_and_time_doors` half - the
sysctl's three time shapes and its refused `chown`, the pid directory's four `EPERM`s, a
proc file's accepted `chown`, and six claims from a child that dropped to uid 65534
(its `EACCES`/`EPERM` split on the sysctl, its device-node pair). The whole program was
run as root against real Linux first and answered every claim `ok:`.

**Gates**: x86_64 `pass=237 fail=0` / `mpos-kunit 34 passed` (`build/scratch/attr2-gate2.log`, `X-EXIT=0`); aarch64 `all 236 user tests passed` / kunit 34 (`verdict pass=236 fail=0`, `A-EXIT=0`, `build/scratch/attr2-gate-a1.log`).

**Counterfactuals**, both run on x86_64:

* the two refusal doors answering `false` - `t-proc-modes` alone red, `1 of 237`, with
  exactly three claims red (`chown on a /proc/sys file is EPERM even for root (got 0)`,
  `the pid directory refuses the three timestamps shapes with EPERM (got 0/0/0)`,
  `and it refuses chown with EPERM for root too (got 0)`), the drops staying green
  (`build/scratch/attr2-cfa.log`);
* `may_touch` back to the mode-less "not policed" answer - exactly two claims red, the
  child's "now" and null shapes on the `0444` sysctl (`bits 3a`), while its explicit-pair
  and `chown` claims stayed green because those are the ownership arm's
  (`build/scratch/attr2-cfb.log`).

**What stays named.** Per-process ownership: Linux's `/proc/<pid>` nodes are owned by
the process they describe, so a non-root caller's `chown(self)`/`utimensat` on
`/proc/self/status` is `0` there; this tree reports every procfs node as root's and
keeps no dynamic owner, so the same caller reads the root-owned file's answers
(`EPERM`/`EACCES`). That is an ownership model, not an answer, and it is the same
"reported, not kept" shape as item 137's mode. `chown` on a sysfs node answers `0` and
stores nothing - Linux's kernfs does store what root names, so a later `stat` differs
from this kernel's; named, not claimed.

### 3.20 A mount's room covers the attributes its inodes keep — **DONE** (2026-10-30)

R.8's last open door was the one item 132 wrote down as a divergence: a `RamFs`
charged file *contents* to a mount's room, and nothing else. An extended
attribute is kernel heap (a `String` name and a `Vec` value per attribute), so a
caller who set attributes in a loop grew the kernel heap with no mount budget in
the way — a `size=64k` tmpfs could hold a gigabyte of `user.*` values.

**Measured on Linux 7.2.11 first** (`build/scratch/xcharge/p.c`, uid 0, a tmpfs
mounted `size=1M`): forty 64 KiB attributes (**2 621 440 bytes**) all answered
`0` on the same file, and one 64 KiB *data* write on that file still fit. So a
tmpfs's `size=` is a page-cache budget: attributes are the filesystem's metadata
and are not counted against it. Linux bounds them where it bounds any kernel
allocation — the allocator's answer, and under pressure the OOM killer, not
`ENOSPC`.

**Landed**: a store's bytes are charged to the filesystem's room through
`vfs::xattr::Budget` (`charge`/`release`). `RamFs` implements it with its own
`Usage`, so a node's vault (`Vault::with_budget`) charges a new attribute's name
plus value, a replacement's difference, and releases on remove, replacement and
the inode's own drop (`Vault::drop` gives back everything still held). A refused
charge is `ENOSPC` (`xattr::Error::NoSpace`) — the number a tmpfs already answers
when its size is reached, so a caller that treats one errno as "the filesystem is
full" sees the same thing on both of its doors. This kernel's `size=` therefore
means *everything this mount keeps*, where Linux's means *page cache*: a named
divergence in the direction the worst-case rule requires, and the reason it is
recorded here rather than claimed against Linux.

Named residual: **directory entries are still uncharged** — one name in a
`RamDir` is a `String`, so the same argument applies to names, and the fix is
the same shape. It is left named because the accounting has to agree with
`t-tmpfs-limits`' existing "a `size=64k` mount takes exactly its 64 KiB" claim,
which counts data only (as Linux's `size=` does), and that reconciliation is its
own measured item.

Pins: `src/vfs/xattr.rs`'s three host tests (a counting room: charge, replacement
difference, drop releasing) and `src/vfs/ramfs.rs`'s
`a_mounts_room_covers_the_attributes_its_inodes_keep` (a 64 KiB mount holds two
22 KiB attributes, refuses the third, and takes the third after one is given
back). `user/tests/src/bin/t-xattr.rs` carries the same claims on the guest
(`a size=64k mount holds two 22 KiB attributes`, `a mount's size= covers the
attributes it keeps, so the third is ENOSPC`, `an attribute given back is room
for the next one`); a host run skips them because Linux accepts all three, which
is the divergence itself.

Gates: x86_64 `X-EXIT=0`, aarch64 `A-EXIT=0`. Counterfactual: with the vault
built by `Vault::new()` instead of `Vault::with_budget(...)`, `t-xattr`'s third
claim answers `FAIL: a mount's size= covers the attributes it keeps, so the third
is ENOSPC` while the other two stay green — the claim is the budget's, not the
mount's.

### 3.21 A mount's room covers the names it holds — **DONE** (2026-10-30)

§3.20 closed the attribute door and left this one named: a directory entry is a
`String` on the kernel heap, and nothing charged it to the mount, so a caller
creating names in a loop could grow the heap past any `size=` — the same
worst-case shape as the attributes, one door over.

**Measured on Linux 7.2.11 first** (`build/scratch/namecharge/p.c`, root, a tmpfs
mounted `size=65536`): ten thousand empty files with ten-byte names — about
100 kB of name bytes on a mount whose number is 64 kB — were all created, and
the mount still worked. A tmpfs's `size=` is page cache; names are dentries and
answer to the allocator. So this again is this kernel's own bound, not Linux's,
and the divergence is named rather than claimed against it.

**Landed**: every place a name enters a directory charges its length to the
mount's room (`Usage::charge_name`), and every place one leaves gives it back
(`release_name`): `create` (release again when the name turns out taken),
`symlink` (the name *and* the path it points at — both are bytes the mount
keeps), `link` (a second edge is a second name), `unlink`/`rmdir` (release after
the removal succeeded). A refused charge is `ENOSPC`, the same number the data
and attribute doors answer.

**The one existing claim this had to reconcile**: `t-tmpfs-limits`'s "a
`size=64k` mount takes exactly its 64 KiB and then answers ENOSPC" counted data
only, as Linux's `size=` does. A file's name is now part of that 64 KiB, so the
fill names its own file (`fill`, four bytes) and the claim became "holds its
name plus the rest of its 64 KiB" — `65 536 - 4` bytes land, the next one is
`ENOSPC`. That is the honest reading of this kernel's `size=` now, and the
reconciliation is the reason the item was named rather than folded into §3.20.

Host tests: `a_mounts_room_covers_the_names_it_holds` (a 8-byte mount: `abcdef`
fits, `ghij` is refused, the name given back makes room, a hard link takes room
again, a symlink asks for its target's path too) and the ledger test's numbers
now count the name (`3001` for a three-thousand-byte file called `a`).
`t-tmpfs-limits` carries the reconciled claim on the guest.

Gates: x86_64 `pass=238`, aarch64 `all 237`, both `mpos-kunit 35 passed`.
Counterfactual: `Usage::charge_name` answering `true` without charging →
`t-tmpfs-limits` alone red, exactly `a size=64k mount holds its name plus the
rest of its 64 KiB, then answers ENOSPC`, because the data alone then fits.

### 3.22 A pid directory names the process it describes — **DONE** (2026-10-30)

The pid tree reported root's ownership for every node it made, which is what a
`stat /proc/<pid>` of any program's directory answered. Linux does not: the
nodes describing a process belong to that process.

**Measured first** (Linux 7.2.11, `build/scratch/attr2/powner.c`, `powner2.c`
and `powner3.c`, as uid 1000 and as root). A plain process (uid 1000, dumpable
1) hands its ids to everything; a child that changed its ids (`setuid(65534)`,
which clears the dumpable flag) keeps them **only on the names**:

| node | mode | plain uid-1000 process | `setuid`-dropped child (dumpable 0) |
|---|---|---|---|
| `/proc/<pid>` | `0555` | 1000/1000 | **65534/65534** |
| `/proc/<pid>/task/<tid>` | `0555` | 1000/1000 | **65534/65534** |
| `/proc/<pid>/status` | `0444` | 1000/1000 | **0/0** |
| `/proc/<pid>/fd` | `0500` | 1000/1000 | **0/0** |
| `/proc/<pid>/ns` | `0511` | 1000/1000 | **0/0** |
| `/proc/<pid>/cwd`, `/proc/<pid>/root` (links) | `0777` | 1000/1000 | 1000/1000 |

A root process answers `(0, 0)` everywhere, so the two rules below are only
distinguishable on a process that changed its ids - the same shape as item
140's two attribute doors, and the reason this pin needs a child fixture rather
than a claim about the sweep's own root identity.

The two rules:
- the directory that **names** a process carries the process's `euid`/`egid`,
  whatever its `dumpable` flag says;
- everything **inside** it follows the `dumpable` flag, which a changed `euid`
  clears (`Credentials::on_identity_change`, `crates/cred/src/lib.rs`) - so the
  same process's `status` and `fd` answer root.

**Landed.** `mpos_cred::process_dir_owner` and `mpos_cred::task_dump_owner`
(pure, host-tested against the table above, in the crate the rules belong to),
one new default door `Inode::adopt_process(ids, dumpable)`, and in procfs an
`Inherited` cell per node plus, on a `ProcDir`, the process it names. The
adoption happens in `ProcDir::lookup`: a node built on demand *after* the
process directory was made (the `fd` directory, the `ns` directory, a task's
directory, and each of their entries) is handed the process on the way out, so
the rule is one funnel rather than a constructor argument threaded through every
builder. `/proc/<pid>/task/<tid>` is a mirroring directory now too - it names a
thread the way `/proc/<pid>` names a process - which also gives it the attribute
refusals item 138 gave the pid directory. `status` prints its `Uid:`/`Gid:`
lines from the task's identity (real, effective, saved, filesystem) instead of
four zeroes.

**Pin**: `user/tests/src/bin/t-proc-owner.rs`, seven claims, with the child
fixture staying alive on its stdin while the parent inspects `/proc/<pid>`. Its
first version had the child exit before the parent looked, and every ownership
claim failed against Linux for the trivial reason that the pid was gone - the
fixture now keeps the process alive until the parent is done. Reference run:
against real Linux 7.2.11 as root, all seven `ok:`.

**Gates**: x86_64 `verdict pass=240 fail=0` (25.73 s), aarch64 `all 239 user
tests passed` (25.84 s, `-35.9%`); both `mpos-kunit 35 passed`.

**Counterfactuals**, one per half of the rule:
1. the two `adopt_process` calls dropped (from `process_dir` and
   `thread_dir`) -> `t-proc-owner` alone red, exactly `the process's directory
   names the user it runs as` and `its thread directory carries the same ids`;
2. the `dumpable` flag ignored (every node handed `process_dir_owner`'s answer)
   -> exactly `a file inside it is root's, because the process made itself
   un-dumpable` and `the fd directory follows the dumpability, not the name` red,
   the other five green.

**Named divergences.** Linux's exemption for the naming directory is a predicate
in its own source; this kernel states the same set of rows structurally ("a
directory that mirrors a process") and pins the measured table.

### 3.22a The links and the thread's own trees, and a foreign descriptor followed — **DONE** (2026-10-30)

The ownership rule above stops at names; this section is what came after it,
and it found two shapes missing rather than two rules wrong.

**Measured first** (`build/scratch/fdns/p2.c`, Linux 7.2.11, run as uid 1000
and as root; `lstat` and `stat` asked separately). A plain process owns
everything in its own entry — `/proc/<pid>` `0555`, `fd` `0500`, `fd/0` link
`0500`, `ns` `0511`, `ns/net` `0777`, `exe` `0777`, `status` `0444`, all
uid 1000. A `setuid`-dropped process (`dumpable == 0`) still owns the two
directories that *name* it — `/proc/<pid>` and `/proc/<pid>/task/<tid>`, uid
65534 — and everything else answers **root**: `fd` `0500`, `fd/0` link `0500`,
`ns` `0511`, `ns/net` `0777`, `exe` `0777`, `status` `0444`, and
`/proc/<pid>/task/<tid>/fd/0` the same. So the two rules are exactly the two
functions of §3.22, one applied per node.

**Landed.** `t-proc-owner` gained seven claims for those nodes (the link's own
`lstat`, because a descriptor's *target* has an identity of its own), and
`/proc/<pid>/task/<tid>` gained the two subtrees Linux has there and this tree
did not: `fd` (`0500`, the process's descriptor table — a thread shares it) and
`ns` (`0511`, the same namespace objects), both inheriting down the `lookup`
funnel like every other child.

The second finding is a following question rather than an ownership one:
`stat("/proc/<other>/fd/0")` used to answer `ENOENT`, because the walk's magic
link door is [`Inode::as_fd`], whose answer is a descriptor **number** — only
meaningful in the caller's own table, so it refuses another process's link.
Linux follows that link to the very file the process holds
(`build/scratch/fdns/p2.c`: `stat` reports the pipe's `S_IFIFO`, `lstat` the
link). The fix adds the second payload door, [`Inode::as_fd_inode`] — the link
answers with the **inode** for a foreign descriptor (from
`sched::descriptor_inode_of`, the sibling of the `descriptor_access` that
already answered `fd/<N>`'s own word), and the walk pushes it the same way it
pushes the own-descriptor arm. `/proc/self/fd/<N>` is unchanged: its `as_fd`
answer still works, and the inode door is what a *foreign* name needed.

**Pin**: `t-proc-owner`'s new claim compares the *kind* `stat` of the link
answers with the kind the child's own `fstat(0)` reports — the descriptor the
child inherits belongs to the harness, so naming one would pin the harness
rather than the rule. Reference run against Linux as root: `got Some(4096)`
(`S_IFIFO`), `PASS`.

**Gates**: x86_64 `verdict pass=240 fail=0` (23.72 s), aarch64 `pass=239
fail=0` (43.52 s), both `mpos-kunit 35 passed, 0 failed`.

**Counterfactual**: the `as_fd_inode` arm removed from the walk ->
`t-proc-owner` alone red on exactly `a foreign descriptor's second name is the
file it holds (got None)`, the other thirteen claims green.

**Named residual**: what is still not policed is *who may read* another
process's tree — Linux's `ptrace_may_access`, which a `dumpable` flag and a uid
answer together. This kernel's uid answers ownership and permission but not
access: a process can read a sibling's `/proc/<pid>/status` it does not own.
Recorded in `docs/uapi/` §5.1's process-tree item.

### 3.23 sysfs reports its word and keeps it — **DONE** (2026-10-30)

sysfs reported a word and kept nothing: every node answered its kind's constant,
a directory answered `0` and left the rendering to the stat door, and
`keeps_modes()` was `false` - so `may_as` said "not policed" and an ordinary user
reached a write descriptor on a `0444` attribute while a root `chmod` answered
`0` and stored nothing.

**Measured first** (Linux 7.2.11, `build/scratch/sysfsattr/p.c`, `p2.c`, `p3.c`,
as uid 1000 *and* root): `/sys` is `0555`, a directory under it `0755`, a
read-only attribute `0444` and a writable one `0644`. Root's
`chmod 0700 /sys/kernel` answers `0` and `stat` then reports `0700` - the word is
stored on the inode, so **the next process sees it** - while an ordinary user's
is `EPERM` (1) and leaves the word alone. The descriptor door (`fchmod` of an
attribute's fd) takes the same path. And `open` of a `0444` attribute `O_WRONLY`
is `EACCES` (13) **for root too**: the attribute has no write method, so the
refusal is the node's answer and not a permission-bit question. `O_RDONLY`
answers a descriptor for both callers.

**Landed.** Each sysfs node carries its word in an `AtomicU32` initialised with
the kind's constant; `keeps_modes()` is `true`, so `may_as` polices the word like
any other filesystem's, and `set_mode` stores what a permitted `chmod`/`fchmod`
writes (masked to the `07777` triads, as `chmod(2)` masks). `SysDir::new` now
passes `0755` instead of `0`: a directory used to report `0` because the stat
door rendered the kind's default, but `mode()` is the word a permission check
reads, and a `0` there is the `0000` no `chmod` can set. The one directory with
its own word, `/sys`, still passes `0555` through `SysDir::own`.

**Pin.** `t-proc-modes`'s `sysfs_modes` half (11 claims): root's `chmod` on a
directory is `0` *and* the word stat then reports is the one it wrote; root's
`fchmod` of an attribute's fd is `0` and the word moves; a child that dropped to
uid 65534 gets `EPERM` for the `chmod`, `EACCES` for the write descriptor and a
descriptor for the read one; and the directory's word is still root's afterwards.
Every word the claims move is put back, so the tree is unchanged when the run
ends. The attribute is whichever one the machine publishes (this tree's
`/sys/kernel/ostype`, Linux's `uevent_seqnum`), so the pin's reference run is
real Linux as root - all claims `ok:` there.

**Gates**: x86_64 `verdict pass=240 fail=0` (20.80 s), aarch64 `pass=239 fail=0`
(53.06 s; the TCG arm's spread, not a cost of this change - the policing is one
atomic load per open); both `mpos-kunit 35 passed`; host unit tests 716.

**Counterfactual**: `set_mode` accepting and dropping the word (`let _ = mode`)
-> exactly the two store claims red, `and the word it wrote is the one stat
reports` (got `0755`) and `and the word moves with it` (got `0444`), with the
`chmod`/`fchmod` answers still `0` - the shape this item pins against.

**Named divergence, closed since.** Root's `open(O_WRONLY)` on a read-only
attribute answered a descriptor here, where Linux refuses the `open` itself
(`EACCES`) - closed by §3.25, the method door: the refusal is Linux's
`do_dentry_open` and not a permission verdict. The word lives on the node and
Linux keeps it on the inode - same lifetime, same visibility.

### 3.24 A node that describes a process is read by a caller that may inspect it — **DONE** (2026-10-30)

`/proc/<pid>/maps` reports `0444`, and an ordinary user cannot read another
user's. That is not a contradiction in Linux's file mode; it is a second rule on
top of it, and the mode is not the one that decides. Measured first, because the
answer looks like a permission verdict and is not one.

**Linux 7.2.11** (`build/scratch/ptraceacc/p.c`, a reader at uid 1000 over a
plain root child and over a child that dropped to uid 65 534; and the same
probe run as root):

| door | root target, reader uid 1000 | reader root |
|---|---|---|
| `/proc/<pid>` `opendir` | ok (54 entries) | ok |
| `status` / `cmdline` / `stat` read | ok | ok |
| `maps` read | `EACCES` (13) | ok |
| `environ` read | `EACCES` | ok |
| `fd` `opendir` | `EACCES` | ok |
| `fd/0` `readlink` | `EACCES` | ok |
| `exe` `readlink` | `EACCES` | ok |
| `ns/net` `readlink` | `EACCES` | ok |

`maps` is `0444` and `smaps` `0444`; `environ` is `0400`. So the refusal on
`maps` cannot be its word. The rule behind it is `ptrace_may_access`: a caller
may read what a process's *memory* files say when the two share a uid and the
target is dumpable. One measurement makes that concrete and is worth writing
down because it looks like a contradiction: a process that **reached** uid 1000
through `setuid(2)` is refused a root process's `maps`, while a process started
*as* uid 1000 (`sudo -n -u '#1000'`) is allowed it — the transition is what
`ptrace_may_access` reads, not the id. This kernel implements the rule the probe
drives: `shares_uid` over the four id pairs of the caller against the target's
credentials, plus `target dumpable != SUID_DUMP_DISABLE`
(`crates/cred/src/lib.rs`'s `ptrace_may_access`), which is also what
`sched::may_inspect` answers.

**The shape, as landed.** Two doors on `Inode` name the fact, and one funnel
uses it:

- `fn mode_polices_access(&self) -> bool` — the word this node reports *is* the
  verdict for reading/writing/searching it, because the filesystem keeps no
  modes and no handler does the check. procfs answers `true` on the descriptors
  and euid; the sysfs half is item 144's (`keeps_modes`).
- `fn describes_task(&self) -> Option<usize>` — the process a node answers
  *about*: a `ProcFile` built by `task_file(slot, generate)` (`maps`, `smaps`),
  an `fd`/`ns` link, and the `fd` directory. `readlink_named` and the walk ask
  it before the node's own answer.
- `may_access` runs `sched::may_inspect(TaskId(pid)) == Some(false)` first and
  answers `EACCES` before the mode check; `None` (the task is gone) falls
  through, which is exactly what `t-ns-time` measures for a dead process's
  namespace link.
- `readlink`'s check is `== Some(false)`, not `!= Some(true)`: the first attempt
  used the second and turned a dead process's `ns/time` link into `EACCES`
  (`t-ns-time` went red on exactly that claim) — the difference between "the
  machine refuses" and "the machine has nothing to ask".

**The pin** is `user/tests/src/bin/t-proc-access.rs`, 8 claims: another uid
reader is refused `maps` (`EACCES`), is *allowed* `status`/`cmdline`/`stat`,
is refused the `fd` directory and its entries, `exe` and `ns/net` (readlink),
while root reads `maps` and lists `fd`. Its host branch prints the reference
verdicts for a root process and a dropped child.

**Named divergences, plainly.** (1) The reader's own credential *transition* is
not modelled here: `may_inspect` answers on ids and dumpability, so a process
started as uid 1000 reads a root process's `maps` on Linux and is refused here
(the probe's own shape is the one the pin drives, and the measurement above
records why the two differ). (2) `pagemap`/`mem`/`auxv` are not served at all,
so their verdicts are moot. (3) `/proc/<pid>/environ` is refused by its `0400`
word, which this tree already policed, not by the ptrace rule — same errno, a
different door.

**Gates.** x86_64 `verdict pass=241 fail=0`, `mpos-kunit 35 passed`
(`build/scratch/access-gate1.log`, `X-EXIT=0`); aarch64 `verdict pass=240
fail=0`, `mpos-kunit 35 passed` (`build/scratch/access-gate-a3.log`,
`A-EXIT=0`) — after two runs whose lane 1 hit the documented EL0 flake onset
(`EXCEPTION: synchronous ESR_EL1 0x96000047` at `FAR_EL1
0xffffffff802ff000`, before any user program, with the lane then hanging at the
dump; killed that lane's QEMU and re-ran).

**Counterfactual.** With `sched::may_inspect` answering `Some(true)` for every
live process (the pre-item behaviour: nothing asked it), the gate went to
`pass=239 fail=2`: `t-proc-access`'s `its maps is still the owner's alone` red —
the claim that is *not* also enforced by a mode — and `t-pidfd` red, because
`pidfd_open`/`pidfd_send_signal` ask the same predicate. The `fd` directory and
the links stayed green under the stub, which is itself a measurement: those
refusals are carried by their `0500`/`0700` words, so only the `0444` file needed
the second rule.

### 3.25 An open for writing asks the method, not only the word — **DONE** (2026-10-31)

Item144 left one sentence named: root's `open(O_WRONLY)` on a read-only sysfs
attribute answered a descriptor here, where Linux refuses the open itself. It
looked like a permission question and is not one - the word says `0444` to
everyone equally, root's permission override passes it, and Linux still says
`EACCES`. The rule is Linux's `do_dentry_open`: a write-mode open asks whether
the node **has** a write method, and kernfs's read-only attributes do not.

**Measured first** (`build/scratch/wopen/q.c`, Linux 7.2.11, uid 1000 and root):

| node | word | `O_WRONLY` as uid 1000 | `O_WRONLY` as root |
|---|---|---|---|
| `/sys/kernel/uevent_seqnum` | 0444 | `EACCES` | **`EACCES`** |
| `/sys/devices/system/cpu/online` | 0444 | `EACCES` | **`EACCES`** |
| `/sys/kernel/vmcoreinfo` | 0444 | `EACCES` | **`EACCES`** |
| `/sys/class/net/lo/address` | 0444 | `EACCES` | **`EACCES`** |
| `/sys/class/net/lo/mtu` | 0644 (writable) | `EACCES` | descriptor |
| `/proc/version` | 0444 | `EACCES` | descriptor |
| `/proc/self/environ` | 0400 | `EACCES` | descriptor |
| a `0444` regular file | 0444 | `EACCES` | descriptor |
| `/sys/kernel` (directory) | 0555 | `EISDIR` (21) | `EISDIR` |

Two lines carry the whole design. Root on a read-only **sysfs** attribute is
refused, so the rule cannot be the mode (root overrides 0644/0444 on a regular
file and gets a descriptor — last-but-one row). Root on a read-only **procfs**
file is *not* refused, because procfs's file operations carry a write wrapper
that fails later at the write; only kernfs asks the method at the open. And a
directory's `EISDIR` still comes first, root included.

**Landed.** One door and one refusal:

- `Inode::has_write_method() -> bool` (default `true`), documented as "the word
  is a permission, the method is what an open for writing needs". `SysFile`
  answers `false` — a value kernfs renders and nothing sets, whatever `chmod`
  later puts in the word.
- `open_linux` refuses a `want & W_OK` open of such a node with `EACCES` after
  `may_access` and before the immutable/append-only checks — the same place
  Linux's `do_dentry_open` runs it — closing the descriptor on the way out,
  exactly as the other refusals there do.

**Pin.** `t-proc-modes`'s `sysfs_modes` half gained four claims, run as root
(the sweep is root): root's `O_WRONLY` on the read-only attribute is `EACCES`
and `O_RDWR` likewise, while `O_RDONLY` is a descriptor; and the contrast that
keeps the rule honest — root's `O_WRONLY` of a `0444` **regular** file is a
descriptor, so the claim is about the method and not the mode. Reference run
against real Linux as root (`sudo -n`): all four `ok:`, `PASS`.

**Gates**: x86_64 `verdict pass=241 fail=0`, `mpos-kunit 35 passed`
(`build/scratch/wopen-gate1.log`, `X-EXIT=0`); aarch64
`verdict pass=240 fail=0`, `mpos-kunit 35 passed`
(`build/scratch/wopen-gate-a1.log`, `A-EXIT=0`).

**Counterfactual**: the refusal's condition made permanently false (the
pre-item behaviour) -> `1 of 241`, `t-proc-modes` alone, red on exactly
`root's write-open of a read-only attribute is EACCES (got 3, errno 1)` and
`and O_RDWR is refused the same way (got 4, errno 1)`; the unprivileged half
stayed green, which is the point — a mode-only kernel refuses the user and hands
root a descriptor.

### 3.26 The `relatime` test was order-dependent, so stage 1 was green by luck — **FIXED** (2026-11-04)

The gate's stage 1 kept going red on `vfs::ext4::tests::relatime_moves_atime_only_when_something_has_happened`
and nowhere else, and the reading written down twice (here and in §6.77c) called it an "unexplained
concurrency transient" because the standalone rerun of the gate's own cargo command was always green.
That reading was wrong, and the measurement that replaces it is unambiguous:

```
$ .../mpos-faf2fc10fb146839 relatime_moves_atime_only_when_something_has_happened --test-threads=1
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 725 filtered out
$ .../mpos-faf2fc10fb146839 vfs::ext4::tests:: --test-threads=1
test result: FAILED. 55 passed; 1 failed; 0 ignored; 0 measured; 670 filtered out
$ cargo test -p mpos --lib --target x86_64-unknown-linux-gnu
test result: ok. 726 passed; 0 failed      (12 runs in a row, and 3 more under load)
```

**It fails every time it runs alone and passes only behind ~700 neighbours.** The mechanism is the
host clock. `src/vfs/ext4/inode.rs` answers `now()` from a real clock on the machine and from a
process-global counter on the host:

```rust
#[cfg(not(target_os = "none"))]
pub(crate) fn now() -> u32 {
    static HOST_NOW: AtomicU32 = AtomicU32::new(1_700_000_000);
    HOST_NOW.fetch_add(1, Ordering::Relaxed)
}
```

The test created a file (three draws), invented `first` from its own stamps, wrote 9 bytes (which
stamps `mtime`/`ctime` from `now()`), and then asked for `touch_atime(first + 20)` and asserted
`atime() > first`. `touch_atime` (`src/vfs/ext4/mod.rs:1419`) moves the clock only when

```rust
data.atime <= data.mtime || data.atime <= data.ctime || stamp.saturating_sub(data.atime) >= DAY
```

and in isolation `first + 20` outran every mtime a three-draw write could produce, so none of the
three clauses fired, `touch_atime` did nothing, and the assert `a read after a write moves the clock
again` fired. In the full suite ~700 other tests had advanced `HOST_NOW` far enough that the write's
own stamps were ahead of `first`, and the test passed. Two days of gate reds were this, and the
concurrency story was a coincidence of *when* the ext4 module happened to be scheduled.

Same shape as the two AGENTS.md §5 entries that were already written down (the `rand` generator and
the devpts option tests): **a module global read or written by parallel tests**.

**The fix** makes the claim true at any position in the run by asking the write for its own clocks
instead of inventing one:

```rust
file.write_at(0, b"payload").unwrap();
let written = file.mtime().max(file.ctime());
assert!(written >= born, "the write moved the metadata clocks");
file.set_times(born, written).unwrap();
file.touch_atime(written + 1).unwrap();
assert_eq!(file.atime(), written + 1, "a read after a write moves the clock again");
```

`set_times(born, written)` is the load-bearing half: without it the atime is still ahead of the
write's clocks, so the rule's `atime <= mtime` clause is unreachable and the same assert fails at a
different line (measured: a first attempt that only moved the stamp ahead of the write still failed
at `src/vfs/ext4/mod.rs:2605`).

**Pinned by the existing test, counterfactually.** Alone before the fix: `FAILED. 0 passed; 1 failed`,
ten runs out of ten. Alone after: `ok. 1 passed`, five runs out of five. The module filter went from
`55 passed; 1 failed` to `56 passed; 0 failed`, and the full suite stayed `726 passed; 0 failed` on
four consecutive runs.

**Rule for the next test that reads a clock:** a test asserting a *relationship* between two stamps
must derive both from what the code produced, never from numbers the test itself made up — the host
clock is a counter whose position depends on how many other tests ran first.

### 3.27 A stale membership entry left its slot in *no* group, so the second cgrouped program in a boot was missing from the root — **FIXED** (2026-11-04)

AGENTS.md §5 had carried this as an **observed flake, not a diagnosis**: `cargo xtask test` going red on
`t-cgroupfs`'s `mkdir grp: Os { code: 17, kind: AlreadyExists }` (`=== FAIL t-cgroupfs exit=134`), twice,
both times re-running green. The isolated shape was fine — `t-cgroupfs` alone on a fresh boot passed three
times over — which is exactly the reading that kept it a flake. What settled it was running the program
**twice in one boot** (`build/scratch/cg-boot.sh` boots the same OVMF/QEMU wiring as
`.workbuddy/one-test-boot.sh` with an `/init` that runs `/bin/t-cgroupfs` twice, `build/scratch/cg-init.sh`):

```
=== first exit=0
FAIL:  the root's cgroup.procs lists this program
=== second exit=1
```

Every claim passed on the second run except that one, and it is the first membership claim in the program.
The `EEXIST` the gate reported is a *later* symptom of the same thing: the program leaves its group behind
when an earlier claim has already failed in a way that skips its cleanup.

**The rule was written down and not implemented.** `src/vfs/cgroupfs.rs`'s header says what a stale entry
means: "The generation beside the node is the slot's tenant when the write was made: a lookup whose
generation has moved on is a stale entry for a reused pid, and reads as *root*". `group_of` implements it
(the generation is a **guard** on the arm, so a moved-on entry falls to `_ => the_tree()`), and `members_of`
did not:

```rust
Some((tenant, at)) => tasks::generation_of(tid) == Some(*tenant) && Arc::ptr_eq(at, node),
None => node.is_root,
```

A stale entry therefore compared false for **every** node — the tenant belonged to no group at all, so it
was missing from the root's `cgroup.procs` (and from every group's). The tree is one machine-wide hierarchy
(`static TREE`) that outlives every program that mounts it, and memberships are keyed by the task **slot**
with the slot's generation stored beside them; `TaskTable::alloc_slot` (`src/sched/runtime/task.rs:2153`)
bumps that generation on every allocation, so the first program to exit a cgroup hands its slot to the next
one as a stale entry. That is why it took a *second* run in the same boot to see it: the gate's lanes each
boot fresh, and two cgroup programs sharing a lane is the shape that makes the slot get reused.

**The fix** puts the generation back where it belongs — in the match arm, as a guard:

```rust
Some((tenant, at)) if tasks::generation_of(tid) == Some(*tenant) => Arc::ptr_eq(at, node),
_ => node.is_root,
```

which is the same shape `group_of` already had. The two readers can no longer disagree about what a stale
entry means.

**Pinned, twice over.**

1. The two-run boot: before, `=== second exit=1` with the claim above red; after, `=== first exit=0` and
   `=== second exit=0`.
2. Two claims added to `t-cgroupfs` (so the gate itself runs the shape from now on): "a forked child moves
   itself into the group" and "a slot a cgrouped task left behind still reads as root for its new tenant"
   — the second forks a child into `grp`, waits for it, then forks again so the new leader takes the slot
   the cgrouped child released, and asks whether the root's `cgroup.procs` lists it. Counterfactual on a
   single boot with the old arm restored (`src/vfs/cgroupfs.rs` backed up, both kernel and loader rebuilt):
   `FAIL:  a slot a cgrouped task left behind still reads as root for its new tenant`, `FAILED`,
   `=== t-cgroupfs exit=1`; with the fix, both `ok:` and `=== t-cgroupfs exit=0`.

**Rule:** when two readers of the same map are documented to agree, the *shape* of the agreement is part of
the rule — a generation test folded into the arm's body with `&&` answers a third thing (neither this node
nor any other) that nobody wrote down.
