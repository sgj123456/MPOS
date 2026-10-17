# vfs

**Home:** `src/vfs/` · **Owns:** inodes, name edges, mounts and path resolution, the per-process descriptor table, and every filesystem behind them — the real ones, the pseudo ones, and the anonymous objects a descriptor reaches (pipes, epoll, timerfd, eventfd, signalfd, inotify, memfd, dma-buf, locks).

## What it owns

The seam of the whole module is the `Inode` trait in `src/vfs/mod.rs`. Everything above it —
resolution, the fd table, the syscall layer — speaks only `Arc<dyn Inode>`; everything below it
(ramfs, the ext4 driver, the tar initramfs, procfs, sysfs, devfs, p9) implements the trait over its
own storage. This is why the trait's methods all have default bodies that *fail*: a filesystem that
cannot support an operation keeps the default and never writes a stub, which is the codebase rule
that a gap gets a named refusal rather than a shape that looks finished (AGENTS.md §3).

Dentries — the name-to-inode edges — deliberately have **no cache of their own yet**. Each
filesystem holds its edges directly (ramfs in per-directory maps), and `mount::Vfs::resolve` is the
only walker, treating every edge as fresh; the `mod.rs` header says a shared dcache waits for a
disk-backed filesystem whose directories are worth caching, because the memory filesystems already
are their own cache. A contributor should not add a second walker: resolution, `..`, mount
crossing and symlink splicing all live in `Vfs::resolve_walk` and nowhere else.

`Vfs` is an **instance**, not a global, so host tests can build isolated trees; the kernel holds one
in `mount.rs`'s `static STATIC`, reached through the free functions `mount_root`, `mount`,
`resolve`, `resolve_at`, `pivot_root` and friends. That split is worth preserving when adding state
to the mount table — put it on `Vfs`, not on a second static.

What deliberately does **not** live here: the syscall numbers, dispatch and argument validation
(`src/syscall/`, see [syscall](syscall.md)); the terminal *emulator*, the `K_*`/`KD_*` mode
vocabulary and the active-console number (the `term` leaf crate); the block-device and driver
vocabulary (`blk`, `drivers`); and the per-device `ioctl` families, which Phase 11 B2 moved down
into the drivers that own them.

## Key files

Grouped by area, because "where do I put this change" is the question the page exists to answer.

| File | What lives there |
|---|---|
| `src/vfs/mod.rs` | The `Inode` and `FileSystem` traits, `NodeKind`, `Metadata`, `Ready`, `VfsError`, `FsStats`, `Ino`, the `MappedInode` bridge to `mm`, the permission checks (`may_access`, `may_as`, `may_search`, `may_create_in`, `may_remove`, `may_modify`, `may_write`, `may_alter`), the inode-attribute flag vocabulary (`FS_*`, `INODE_FL_VISIBLE`, `INODE_FL_MODIFIABLE`, `apply_flag_change`) and the `read_file`/`read_dir`/`create_file`/`split_parent` convenience layer |
| `src/vfs/mount.rs` | `Vfs`: the mount table, `Mount`/`MountRecord`/`MountPoint`, `mount_at`, `bind_mount`, `remount`, `unmount`, `move_root`/`move_mount`, `pivot_root`, `resolve`/`resolve_from`/`resolve_walk`, `resolve_no_follow`, `resolve_parent`, `path_of`/`parent_of`, `mount_records`, and the `st_dev`/filesystem-identity answers (`dev_of_path`, `dev_of_inode`, `fs_of_path`, `fs_serving_inode`, `fs_id_of_inode`, `fstype_of_inode`), plus the free-function kernel table |
| `src/vfs/path.rs` | `components` (walkable names, `.`/`..` handed through) and `normalize` (the spelling `statfs` matches mountpoints by — *not* resolution) |
| `src/vfs/file.rs` | `File` (an inode plus a cursor, status flags, access mode, open label and `st_dev`), `FdTable` (slots, per-fd `FD_CLOEXEC` word, `RLIMIT_NOFILE` ceiling), `kernel_fds`/`current_table`, `for_each_last`, and the `open`/`open_opt`/`open_inode`/`read`/`write`/`close`/`dup2` entry points |
| **Real filesystems** | |
| `src/vfs/ramfs.rs` | The reference in-memory `Inode`: per-directory maps, `Usage`/`with_limit` for a real `ENOSPC`, modes/owners/times/`NodeFlags`. Also the storage behind `mount -t tmpfs` (the syscall factory maps `tmpfs`/`ramfs` to `RamFs`, stamping `01777` on a tmpfs root) |
| `src/vfs/ext4/mod.rs` | The ext2/ext4 driver and its `FileSystem` impl (`Ext4Fs::mount`): write-through block cache, first-fit allocation, block maps and extent trees, feature triage into maintain / read-only / refuse |
| `src/vfs/ext4/{super,inode,mapping,extent,dir,dirent,dx,hash,csum,alloc,cache,util}.rs` | The superblock and group descriptors, inode I/O, block mapping, the extent tree, linear and hashed (`dx`) directories, checksums, allocation, the block cache and shared helpers — the driver's internal seams, one file each |
| `src/vfs/p9.rs` | The 9P2000.u client (`P9fs`, `P9Inode`), spoken over the `P9Device` contract `drivers::virtio::fs9p` provides; the fid table and the mapping onto `Inode` |
| `src/vfs/tar.rs` | The ustar reader (`entries`, `unpack_into`) — the initramfs format, and the caller that gives ramfs nodes their recorded modes |
| **Pseudo filesystems** | |
| `src/vfs/procfs.rs` | `/proc`: generated files, the process tree (`/proc/<pid>`, `/proc/self`), `/proc/mounts` and `/proc/self/mountinfo` from the mount table, `/proc/net`, `/proc/sys/kernel/{hostname,domainname}`, plus `Process`/`MapLine`/`format_maps` |
| `src/vfs/sysfs.rs` | `/sys`: the class/kernel tree `udev`-style tooling walks (`/sys/class/block`, `/sys/class/input`, `/sys/dev/char/N:M`), the PCI leaves and the writable `uevent` attributes |
| **Device nodes** | |
| `src/vfs/devfs.rs` | The device filesystem and the `CharDevice` trait (`read`/`write`/`write_error`/`ioctl`); `DevFs::{register,register_id,register_block,register_symlink,register_node}`, `NodePerms` (mode/owner that stick), and the node types for char, block, symlink, socket and directory entries |
| `src/vfs/devpts.rs` | `DevPts`: the naming layer over pty slaves, `/dev/pts/<N>` |
| **IPC objects** | |
| `src/vfs/pipe.rs` | `PipeEnd`/`DuplexEnd`, the ring buffer, `PIPE_BUF` record semantics, EOF and `EPIPE`, `splice`'s pipe contract |
| `src/vfs/epoll.rs` | `Epoll`, the interest list keyed by descriptor *and* open file description, `EPOLLET`/`EPOLLONESHOT`, `poll_ready` scanning |
| `src/vfs/timerfd.rs` | `TimerFd`, the tick-deadline/interval/count state machine, `TFD_TIMER_ABSTIME`, `TFD_TIMER_CANCEL_ON_SET`, the timespec conversions |
| `src/vfs/eventfd.rs` | `EventFd`, the counter, `EFD_NONBLOCK` |
| `src/vfs/signalfd.rs` | `SignalFd`: blocked-and-masked signals read as `signalfd_siginfo` — a child's death among them, because it *is* the queued `SIGCHLD` (no second table; `docs/uapi/` §5.1 item 29) |
| `src/vfs/inotify.rs` | `InotifyInstance`, the global `(dev, ino)` watch registry, `emit`/`emit_write`, encoded event records |
| `src/vfs/memfd.rs` | `Memfd`, real shareable pages, `MFD_*`, `F_SEAL_*` (gated `target_os = "none"`) |
| `src/vfs/dmabuf.rs` | `DmaBuf`: exporter page references, the geometry a GEM import needs (gated `target_os = "none"`) |
| `src/vfs/locks.rs` | The advisory-lock table: `fcntl` record locks and `flock` whole-file locks, range resolution, `drop_on_close`/`drop_with_description` (the two release rules) with the `release_on_close`/`release_description`/`release_process`/`release_on_process_exit` wrappers |
| **Terminals** | |
| `src/vfs/tty.rs` | `Tty` (the line discipline state machine `LineDiscipline`, blocking/interruptible reads, `VMIN`/`VTIME`, foreground pgrp, window size, `kd_ioctl`, `vt_ioctl`), `Attached`, `VtCaller`, `ControllingTty`, `Tty0`, the `init`/`global`/`by_dev_id` registry |
| `src/vfs/termios.rs` | The `Termios` structure and the `struct termios`/`termios2` layouts (`SIZE`, `TERMIOS2_SIZE`, `NCCS`), and the `TCGETS`/`TCSETS` flag vocabulary |
| `src/vfs/pty.rs` | `Pty`, `Ptmx`, `PtyMaster`, `PtySlave`: the pair registry, `allocate_dynamic`, `lookup`, `grantpt`/`unlockpt` locking, master↔slave byte flow |
| `src/vfs/vcmux.rs` | The console multiplexer: one serial wire, six consoles, `^A`-digit switching and the `VT_PROCESS` handshake (gated `target_os = "none"`) |

## Contracts

The traits other layers are allowed to name are in `src/vfs/mod.rs`:

- **`Inode`** (`Send + Sync + core::any::Any`) — one node, in the only form the kernel sees it.
  Every method has a failing default, so an implementation overrides exactly what its storage can
  do. The `Any` supertrait plus the inherent `impl dyn Inode { fn as_any }` is the *only* way a
  caller reaches a concrete type it already knows is behind the node; nothing in `src/vfs/mod.rs`
  names a driver or a socket family (Phase 11 B1, `docs/notes/` §11.1.1 / §11.2 Step B1). A new device
  type should add no method here — ask `as_any` from the layer that owns the type.
- **`FileSystem`** (`Send + Sync`) — a mounted instance (the superblock-equivalent): `root()`,
  `name()`, the optional `statfs()`, and the read-only promise (`read_only`/`set_read_only`). A
  filesystem with no name cannot be mounted from userland, which is the point of `mount(2)`.
- **`NodeKind`** — `File`, `Dir`, `CharDev`, `BlockDev`, `Symlink`, `Socket`. `BlockDev` is what
  hands the mount decision to userland: `Inode::as_block` turns "the device named by this path"
  into storage.
- **`VfsError`** — this module's errno vocabulary, each variant documented with the Linux answer it
  must become. Truthfulness is the contract (AGENTS.md §3): wrong shape is worse than refused.
- **`FsStats`**, **`Metadata`**, **`Ready`** — `statfs`, the facts that outlive an open, and
  `poll`'s per-node readiness.
- **`MappedInode`** — wraps an `Inode` as `mm::vma::MappedObject`, the three-question trait
  (`frame_at`, `identity`) that closed the `mm → vfs` cycle (V-13, `docs/notes/` §11.1/§11.2; the
  tenant of an address space is `mm`'s, the thing behind the pages is the VFS's).

The mount/resolution contract is on `Vfs` in `src/vfs/mount.rs`; the free functions are thin
spellings of it over the kernel's one table. The notable invariants each one keeps:

- **`resolve_walk`** is the single walker. It carries the logical chain from the root in a stack,
  which is what makes `..` correct across a mount and across a symlink splice; `resolve_at` jails
  an absolute path to the *calling task's* root while `resolve`/`resolve_from` use the machine's.
  `resolve_no_follow` and `resolve_parent` stop before the last name, because `lstat`, `readlink`,
  `symlink` and `unlink` act on the *name*, not what it points at.
- **The mount table** records two different numbers and the difference is load-bearing: `Mount::id`
  is the serving *instance* (what `EXDEV` and `linkat` ask by, shared by a bind), and `Mount::dev`
  is what `stat`'s `st_dev` reports (the device's own number, or an anonymous one for a
  pseudo-filesystem). `dev_of_path` and `mount_records` derive their answers from one place so a
  tool reading `stat` and `/proc/self/mountinfo` cannot see them disagree (`docs/notes/` §7.4).
- **`pivot_root`/`move_root`** hand a whole filesystem to `/` and park the old tree at `put_old`;
  the mounts inside the promoted tree follow by *name*, and the rest are dropped deliberately.
- **`File`/`FdTable`**: an fd number means nothing without its table. `FdTable::duplicate` shares the
  `Arc<File>` descriptions but not the numbering (fork's rule), `install` refuses a slot at or above
  the `RLIMIT_NOFILE` ceiling with `TooManyDescriptors`, and the `FD_CLOEXEC` bit lives in
  `fd_flags`. `File::ofd()` is the identity everything that arbitrates *per open file description*
  keys by — `flock`, `EVIOCGRAB`, an evdev read's client.

Where the layering is deliberately unequal, and was recently fixed or audited:

- **The `vfs ↔ console` pair is broken** (V-4, `docs/notes/` §11.1.4 / §11.2 Step D): the emulator, the
  `VC_COUNT`/`K_*`/`KD_*` vocabulary, the handover table (`vtmode`) and the active-console number
  all moved to the `term` leaf; `console.rs` reads "which console is on screen" from the leaf, so
  `console → vfs` fell to zero. Relocating `vcmux.rs` itself was evaluated and rejected (D.2b) —
  it stays in `vfs` because its pty/tty orchestration coherently belongs with the nodes it drives.
- **The `syscall/linux/fs.rs` split** (Phase 11 E, `docs/notes/` §11.2 Step E) is why the path/file
  and device-ioctl handlers now live in `src/syscall/linux/fs/{mod,file,ioctl}.rs`: the `vfs` module
  owns the objects and the per-device ioctl *contracts*, the split `fs` module owns the gate.
- **The surviving `drivers ↔ vfs` pair** is a live finding, not precedent: `vcmux`, `procfs`, `sysfs`
  and `p9` still name `drivers`/`net` for the PCI-config and device-state doors, and `procfs`/`sysfs`
  keep their machine-specific leaves behind the capability flags `mpos_pci_config` and
  `mpos_input_registry` rather than an architecture gate (`src/vfs/mod.rs`'s module docs). See
  `docs/ARCHITECTURE.md` §2/§3 for the edges and §9 for the debt audit; this page does not restate
  the counts, which come from `cargo xtask depgraph`.

## What userspace sees

The syscall gate is `src/syscall/linux/fs/` (one row per handler in docs/uapi/ §1), but the
families it serves are this module's objects. The rows this module owns in `docs/uapi/`:

- **Device-`ioctl` families** (docs/uapi/ §2): `'V'` virtual consoles (`vfs/tty.rs`
  `Tty::vt_ioctl`, state read back through `vcmux`), `'K'` console mode (`Tty::kd_ioctl`), `'f'`
  inode attribute flags (`src/vfs/ext4/`, `src/vfs/ramfs.rs`), and the termios/`TIOC*` block
  (`vfs/tty.rs`, `vfs/pty.rs`).
- **Flag words** (docs/uapi/ §3): the inode attribute flag vocabulary
  (`INODE_FL_VISIBLE`/`INODE_FL_MODIFIABLE` in `src/vfs/mod.rs`), and the filesystem-has-no-flags
  refusal (`Inode::flags` default).
- **The filesystems userland mounts and reads**: `/proc`, `/sys`, `/dev`, `/dev/pts`, `/proc/mounts`
  and `/proc/self/mountinfo`, `statfs`/`fstatfs`'s `f_type`, and the `st_dev`/`st_ino` pair that
  `t-stat-consistency` checks agree between `stat`, `fstat` and the mount table.

`eventfd2`, `timerfd`, `signalfd`, `inotify`, `memfd`, `epoll`, the lock families and the file
syscalls each have their own `docs/uapi/` row and their own conformance program below. The
authoritative list is `docs/uapi/`; this page deliberately does not copy it.

## Pins

For the pin rule (a counterfactual test, never `#[ignore]`), see AGENTS.md §3.

**Host tests** — `#[cfg(test)] mod tests` inside the file it tests, run by
`cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`:

| File | Module / tests found |
|---|---|
| `src/vfs/mod.rs` | `mod tests`: `apply_flag_change` verdicts, `split_parent`, `permitted`, `may_as`, and the root-override sentence (`root_passes_where_any_bit_of_the_kind_exists`) |
| `src/vfs/path.rs` | `mod tests`: `components` (absolute split, dot pass-through) and `normalize` (dots/slashes/root-clamp) |
| `src/vfs/mount.rs` | `mod tests` (resolution, links, bind aliases, mount crossings, `st_dev`/mountinfo agreement) and `mod cwd_tests` (a relative path starts at the given node) |
| `src/vfs/file.rs` | `mod tests`: cursor/EOF, `O_APPEND`, the status bits, access-mode enforcement, the `RLIMIT_NOFILE` ceiling, `dup2`, standard three |
| `src/vfs/ramfs.rs` | `mod tests` |
| `src/vfs/tar.rs` | `mod tests` |
| `src/vfs/pipe.rs` | `mod tests` |
| `src/vfs/epoll.rs` | `mod tests` |
| `src/vfs/timerfd.rs` | `mod tests` |
| `src/vfs/eventfd.rs` | `mod tests` |
| `src/vfs/inotify.rs` | `mod tests` |
| `src/vfs/locks.rs` | `mod tests` |
| `src/vfs/termios.rs` | `mod tests` |
| `src/vfs/tty.rs` | `mod tests` |
| `src/vfs/pty.rs` | `mod tests` |
| `src/vfs/devpts.rs` | `mod tests` |
| `src/vfs/devfs.rs` | `mod tests` |
| `src/vfs/procfs.rs` | `mod tests` |
| `src/vfs/sysfs.rs` | `mod tests` |
| `src/vfs/p9.rs` | `mod tests` |
| `src/vfs/ext4/*.rs` | `mod tests` in `mod.rs`, `super.rs`, `dirent.rs`, `cache.rs`, `extent.rs`, `csum.rs`, `hash.rs`, `dx.rs`, `util.rs` |

`signalfd.rs`, `memfd.rs`, `dmabuf.rs` and `vcmux.rs` carry no host test module of their own; their
contracts are pinned from the guest (`t-signalfd`, `t-memfd-shm`, `t-prime`) and, for the mux's pure
handover table, by the `term` crate where the vocabulary now lives. The host suite covers the whole
module only where the parent compiles for the host — the `#[cfg(target_os = "none")]` gates on
`memfd`, `dmabuf` and `vcmux` are deliberate (§8 of `docs/ARCHITECTURE.md`).

**Guest conformance programs** (`cargo xtask test`, one `t-*` per name):

| Area | Programs |
|---|---|
| Paths, names, permissions | `t-access`, `t-faccessat`, `t-faccessat2`, `t-chdir`, `t-chroot`, `t-getcwd`, `t-openat`, `t-openat2`, `t-open-flags`, `t-readlink`, `t-stdresolve`, `t-umask`, `t-fchmodat2`, `t-link`, `t-rename`, `t-renameat2`, `t-mkdirat`, `t-unlinkat`, `t-rmdir`, `t-rmtree`, `t-tmpfile-link` |
| Files, I/O and fds | `t-read`, `t-readv`, `t-write`, `t-writev`, `t-preadv2`, `t-lseek`, `t-truncate`, `t-fallocate`, `t-fadvise`, `t-copy-file-range`, `t-splice`, `t-sync`, `t-dup`, `t-dup2`, `t-dup3`, `t-fcntl`, `t-fd-list`, `t-fstat`, `t-stat-buffer`, `t-stat-consistency`, `t-statx`, `t-statx-agree`, `t-getdents64`, `t-xattr` |
| Mounts and filesystems | `t-mount`, `t-mountinfo`, `t-statfs`, `t-fsflags` (integration), `t-fill-ramfs` (integration), `t-9p` |
| IPC/descriptor objects | `t-pipe2`, `t-poll`, `t-epoll`, `t-epoll-socket`, `t-eventfd`, `t-timerfd`, `t-signalfd`, `t-inotify`, `t-filelock`, `t-memfd-shm` |
| Terminals | `t-pty`, `t-vt`, `t-tty-query`, `t-tty-eintr`, `t-tty-nonblock`, `t-tty-vtime`, `t-fionbio` |
| Devices / boundary | `t-dsp`, `t-udev-worker` (integration), `t-prime` (integration) |

Several of these are joint pins with the `syscall` module (which owns the gate) and the `net` module
(the AF_UNIX and socketpair cases); they belong in this table when the assertion is about a `vfs`
object, and in the neighbour's page when it is about the ABI or the wire.

## Evidence and open work

The reasoning and the measurements live in the docs above this directory; this page points at them
rather than re-arguing or restating numbers.

- **Why the code is shaped this way** — `docs/notes/`: §11.1.1 and §11.2 Step B1 (the `Inode`
  downcast chain, V-1, and its closure); §11.1.4 and §11.2 Step D (V-4, the terminal emulator and
  console mux, the `vfs ↔ console` pair broken, D.2b rejected); §11.2 Step E (the `fs.rs` split);
  §11.1 V-13 (the `mm`/`vfs` seam and `MappedInode`); §7.4 (`File::dev`, `st_dev` consistency and
  the wayland lock-file race); §13.16 (`vcmux` on both machines); §13.29 (the 9P dialect, measured
  against QEMU); §12.4b (the inode flag word and `FS_IOC_GETFLAGS`).
- **What is left** — `docs/ROADMAP.md`: the P3 Filesystem line, §10.5 (the ext4/virtio-blk read
  path) and the Phase-11 items (A–G, `11.D`), and "Remaining ABI support" for the calls that have
  not landed. The `drivers ↔ vfs` mutual pair is the one surviving edge this module contributes;
  its remaining half is the procfs/sysfs/9p device-state door (V-4's note), plus the lazy-mount and
  namespace work parked in docs/uapi/ §5.4.
- **Known gaps named in the code** — the `src/vfs/ext4/mod.rs` header states what the driver
  deliberately does not do (permission checks, timestamp *updates*); the `src/vfs/p9.rs` header names
  the 9P2000.u gaps (no `symlink`/`link`, and `rename` answering `EXDEV` because the dialect has no
  atomic rename); the `src/vfs/mod.rs` header names the absent dcache. Each is a named gap, not a
  stub, and each belongs in `docs/ROADMAP.md` before it is worked.
- **The edges themselves** — `docs/ARCHITECTURE.md` §2 (module graph) and §3 (what each layer
  owns), §5 (the syscall path, and why `fs` was split), §6 (measured edges and cycles), §9 (the
  debt audit). `cargo xtask depgraph` is the live measurement; this page does not carry counts.
