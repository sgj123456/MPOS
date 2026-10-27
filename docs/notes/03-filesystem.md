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
  means. Devices are born `0666 root:root` — the number `stat` already gave them —
  where Linux's devtmpfs births `0600` and lets udev widen it; that difference is
  the named remainder, and it needs per-class birth modes and group-name
  resolution, not another mechanism.
- Host-tested: `a_triad_decides_who_may_do_what` (incl. supplementary groups), `root_passes_everything_but_execute_on_a_file_nobody_made_runnable`, `may_as_answers_for_the_node_and_the_identity_it_is_given`.

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


