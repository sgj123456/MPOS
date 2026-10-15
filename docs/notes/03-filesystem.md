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
`dup` shares the lock and a second `open()` does not). `release_on_close`
applies the two different rules - POSIX's "closing any descriptor drops the
process's record locks", and "the flock dies with its own description" - and
`user/tests/src/bin/t-filelock.rs` (`user/abitest/src/bin/t-filelock.rs` before
S6 batch 11, its own flock half since corrected there for the second half of the
second rule: see §15.16) pins both, with the contending-second-`open`
and the sharing-`dup` cases side by side.

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

The `F_OFD_*` lock inherits the named `flock` divergence above rather than
closing it: `release_on_close` drops a description's OFD lock on **any** close of
that description, where Linux waits for the last reference - the same
measurement (§15.16) applies (case D of `.workbuddy/probe_ofd.c`: an OFD lock
survives `close(dup)` on a real host), and no claim fails on it because
`t-filelock` names the divergence instead of pinning it.

**Gate.** x86_64 `=== SUMMARY pass=173 fail=0 (173 built)` with `mpos-kunit 32
passed, 0 failed` and exit 33; aarch64 `=== SUMMARY pass=172 fail=0`, the same 32
cases, exit 0. The program counts did not move - the new verdicts are nine more
`ok:` lines inside `t-filelock`, whose `flock`/POSIX half is unchanged. The
host-side lock test `an_ofd_lock_is_a_record_lock_owned_by_the_description`
lands with the `ofd` flag; clippy reports nothing on either bare-metal target or
the host lib, and `cargo xtask depgraph` is within the ratchet with no edge
added.

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

*Remaining:* `atime` is not bumped on read (ext4 carries it but no read path
updates it). ramfs/tmpfs now keeps one clock - stamped at creation and by
writes, truncates and directory-entry changes - which is what stopped
OpenRC's deptree freshness check from reading every `/run` file as 1970.
The legacy `utime` and `futimesat` now store
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

---

