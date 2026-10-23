# MPOS UAPI — the userspace-visible surface, and what is left

`docs/notes/` is the evidence library: one file per phase, one landing per section, measurements
beside it. This library is the other view — an **inventory**, one row per promise the kernel makes
to a binary it did not compile, so the review question can be asked of the whole surface at once:

> *Is that Linux's answer, or a shape that looks like it?* (`AGENTS.md` §6)

"UAPI" is Linux's word for the contract part of the kernel (`include/uapi/`). Here that is:

- **Syscall numbers and their argument/return contract** — `src/syscall/linux/nr.rs`,
  `src/syscall/linux/mod.rs`.
- **Device `ioctl` request words** — one word per promise, dispatched in
  `src/syscall/linux/fs/ioctl.rs` and served by the device's own module.
- **Structure layouts crossing the boundary** — `struct stat`, `rt_sigframe`, `edid`,
  `sockaddr_ll`, `struct sched_attr`, … `struct stat` is per-ABI in layout and *length*:
  aarch64's 128 bytes use Linux's asm-generic order (`st_mode` u32 at 16, `st_nlink` u32 at 20,
  `st_rdev` u64 at 32, the `timespec` seconds at 72/88/104); x86_64's 144 (`st_nlink` u64 at
  16, `st_mode` at 24). `abi::write_stat` branches both on `target_arch` (field order and
  `STAT_BYTES`): the x86_64 order on aarch64 made `st_mode` read `st_nlink`, and the x86_64
  *length* wrote sixteen bytes past every 128-byte struct into the libc stat buffer's frame,
  which is how a non-empty `std::fs::remove_dir_all` came to die at `pc 0` (`docs/notes/`
  §13.10). Pin: `t-fstat`/`t-newfstatat` on both machines (mode/type/`nlink`/`ino`/`blocks`/
  `size` through each machine's own `Stat` — the claim the retired aarch64 payload's
  `MPOS_STAT_OK` carried; the `no_std` suite's hard-coded x86_64 `lin::Stat` went because a
  program that writes one machine's layout down cannot see the other's) and `t-stat-buffer` (a
  guarded `struct stat` through `SYS_fstat`/`SYS_newfstatat` on both machines — a caller's own
  `stat` cannot see the overflow, because musl's wrapper stats into its own struct and copies
  the fields out). The three clock *seconds* are the one shared part — 72, 88 and 104, the
  seconds half of a 16-byte `timespec` whose nanosecond half follows at 80, 96 and 112 — and
  x86_64's writer used to pack them at 72/80/88, so `st_mtime` read `st_atime_nsec` and
  `st_ctime` read zero (`docs/notes/` §13.11). `struct statx` is one layout everywhere: 256
  bytes, all zeroed and filled by Linux, `stx_atime`/`btime`/`ctime`/`mtime` 16-byte stamps at
  64/80/96/112, `stx_rdev_*` at 128/132 and `stx_dev_major`/`minor` at 136/140. This kernel
  wrote its clocks at 96/112/128, its rdev pair at 72, and stopped at 152, and no test read
  those fields back (`docs/notes/` §13.11). Pin: `t-statx-agree` (one file's device, inode,
  mode, size, link count and three clocks through `statx` **and** the `fstatat` family, which
  must agree) on both machines, and the bare-metal suite's `t-statx` for the argument shape and
  clocks compared against `newfstatat` — x86_64 only, that suite having no aarch64 port yet.
  `rt_sigframe`/`sigcontext` is per-ABI too and lives with its machine since §13.2's Stage 2 —
  `arch/src/arch/x86_64/signal.rs`, and `arch/src/arch/aarch64/signal.rs` since Stage 3b, both
  behind `hal::SignalFrame` — because `__restore_rt` reads the saved registers at fixed offsets
  (and the floating-point image is the frame's too: aarch64's `fpsimd_context` inside
  `uc_mcontext.__reserved`, x86_64's `_fpstate` that `sigcontext.fpstate` points at, §15.91).
- **Flag words and closed vocabularies** — `mmap`'s `MAP_*`, `madvise`'s advices,
  `clock_gettime`'s ids, `memfd_create`'s flags, `statx`'s flags, `prctl`'s options.
- **Wire and protocol encodings userland parses** — netlink attributes, `sockaddr_ll`,
  the BOOTP/packet frames a DHCP client reads.
- **Machine facts a program's own instructions observe** — the system registers an EL0 program
  may read, and the operations it may ask the kernel to perform on its behalf. They are the
  *machine's* rather than the ABI's, so they are easy to forget: on aarch64
  an EL0 `mrs Xt, ctr_el0` **traps** (`SCTLR_EL1.UCT` is clear, as Linux leaves it) and is
  answered by the trap handler with the cache line sizes the caller asked for, exactly as
  Linux's `ctr_read_handler` answers it; the `DC CVAU`/`IC IVAU` a code flush issues a few
  instructions later trap for the same reason and are answered by promotion
  (`user_cache_maint_handler`'s `DC CIVAC`, `IC IVAU` as itself). A kernel that treats either
  trap as a *fault* ends the process with a signal for an instruction that has no address at
  all — which is how the aarch64 desktop image's `llvmpipe` startup died, twice in a row.
  Pin: `t-el0-sysreg` (`docs/notes/` §15.36).

A change to any of these is append-only (`AGENTS.md` extending rule 3): a shipped number,
layout or encoding is a contract, because a running musl binary reads it at its own address.

## How to read a row

Every entry carries one of three verdicts:

| Verdict | Meaning | Is it work? |
|---|---|---|
| **Served** | Behaves as Linux does, and a pin would fail without it | No |
| **Refused** | `ENOSYS` / `EOPNOTSUPP` / `EINVAL` / `EPERM` — Linux's own answer for a kernel that cannot do the call (`AGENTS.md` §3.2) | No. A refusal is the honest direction to be wrong in |
| **⚠ wrong answer** | Accepted, answers success, and does less than Linux — or an errno/shape Linux would not give. The `ENOSYS`-free §3.2 failure class | **Yes.** These are the only defects |

A missing feature with a named prerequisite is not a defect; a success-for-a-thing-not-done is
(`AGENTS.md` §3).

## Routing: this library's files

| Section | What it inventories | File |
|---|---|---|
| §1 | The syscall table — every served call, refusals, named sub-gaps | [01-syscall-table.md](01-syscall-table.md) |
| §2 | Device `ioctl`s by family, plus `futex`'s operation word (§2.1) and the whole-word dispatch rule (§2.2) | [02-ioctl-families.md](02-ioctl-families.md) |
| §3 | Flag words and closed vocabularies | [03-flag-vocabularies.md](03-flag-vocabularies.md) |
| §3a | The pseudo-filesystem files (`/proc`, `/sys`) and the socket options | same file, below §3 |
| §4 | Structure layouts crossing the boundary | [04-structure-layouts.md](04-structure-layouts.md) |
| §5 | The work list: landed passes (§5.1), small measured gaps (§5.2), gaps needing a named prerequisite (§5.3), structural work parked with Phase 11 (§5.4) | [05-work-list.md](05-work-list.md) · [05b-small-measured.md](05b-small-measured.md) · [05c-needs-prerequisite.md](05c-needs-prerequisite.md) · [05d-parked-with-phase-11.md](05d-parked-with-phase-11.md) |
| §6 | Where the pins live | [06-pin-locations.md](06-pin-locations.md) |

This library used to be one file, `docs/UAPI.md`; the `§N.M` numbering is unchanged.

## Completeness — how this inventory proves it covers everything

The inventory claims **every dispatch arm has a row**. The check is mechanical; run it from the
repository root.

```bash
# 1. every name the x86_64 table numbers AND that appears as an `nr::NAME` arm in the
#    dispatch must have exactly one row in §1.0's table (and vice versa);
# 2. the two tables (`nr_x86_64.rs`, `nr_aarch64.rs`) hold the same name set;
# 3. the magic bytes filtered by `family_nr` in `src/syscall/linux/fs/ioctl.rs` are
#    exactly the families §2 lists.
grep -oE 'pub const [A-Z0-9_]+: u64' src/syscall/linux/nr_x86_64.rs | wc -l   # names numbered
grep -c '^| `' docs/uapi/01-syscall-table.md                                  # rows across §1.0–§1.2
```

§1.0 currently holds **251** rows — one per dispatch arm, including the kernel's own
`cleanup_after_kthread` (number 999 on both machines' tables: not a Linux syscall, but named so
the ABI stays Linux-shaped). The refusals in §1.1 carry their own rows, so what matters is
*arms = rows*: a syscall landing without a row here is the review finding. A simple generator:
extract `pub const NAME: u64 = N` from `nr_x86_64.rs`, keep the names appearing as `nr::NAME`
arms in `mod.rs`'s dispatch, map each to the handler module named on its arm line (`fs` / `proc` /
`posix` / `net` / `abi` / `kernel`), sort by number, print rows.
