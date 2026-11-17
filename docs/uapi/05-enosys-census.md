# The `ENOSYS` census — which numbers this kernel refuses, classified

`AGENTS.md` §3.2 says the `_ => ENOSYS` arm should stay crowded, and it is. The
question this file answers is which of those numbers are **gaps** and which are
numbers **Linux itself refuses**, because those are two different things and a
flat "70 numbers return ENOSYS" list cannot tell them apart.

## How it was measured

Both sides, with zero arguments, each call in a child so a destructive call cannot
take the measuring process with it:

- Linux 7.3.4, `build/scratch/census/c.c` — `syscall(nr, 0,0,0,0,0,0)` per number;
- this kernel, a guest program over the same list.

The zero-argument call is deliberate and it has a limitation worth stating: an
answer like `EINVAL` or `EFAULT` proves the kernel **reached its argument
checks**, which is enough to show the call exists; it does not prove the call's
work is implemented. The classification below is therefore about *existence of a
dispatch path*, not about completeness — where a number is already served, its
own `t-*` pin is the record of the work.

## The result

| | count |
|---|---|
| numbers probed | 69 |
| **Linux answers `ENOSYS` too** | **19** |
| Linux answers something else | 50 |

### The 19 that match — our refusal is Linux's answer

`uselib` 134 · `sysfs` 139 · `_sysctl` 156 · `create_module` 174 · `get_kernel_syms` 177 ·
`query_module` 178 · `nfsservctl` 180 · `getpmsg` 181 · `putpmsg` 182 · `afs_syscall` 183 ·
`tuxcall` 184 · `security` 185 · `set_thread_area` 205 · `get_thread_area` 211 ·
`lookup_dcookie` 212 · `epoll_ctl_old` 214 · `epoll_wait_old` 215 · `vserver` 236 ·
`quotactl_fd` 443

These are dead or machine-specific entries: glibc owns `set_thread_area` in
userspace, `_sysctl` was removed in 5.5, and the rest were never wired to anything.
`ENOSYS` is exactly right and no work is owed.

### The 50 where Linux answers something else — the real candidates

Grouped by what the zero-argument answer proves:

- **`EPERM` (1)** — a privilege gate is reached, so the call exists: `acct` 163,
  `swapon` 167, `swapoff` 168, `init_module` 175, `delete_module` 176,
  `kexec_load` 246, `finit_module` 313, `kexec_file_load` 320, `pkey_alloc` 330,
  `fspick` 433.
- **`EFAULT` (14)** — an argument pointer is validated, so the call exists:
  `adjtimex` 159, `perf_event_open` 298, `clock_adjtime` 305, `landlock_create_ruleset` 444,
  `cachestat` 451, `lsm_list_modules` 461, `listxattrat` 465, `removexattrat` 466,
  `open_tree_attr` 467, `listns` 470.
- **`EINVAL` (22)** — a flag or struct word is checked: `ustat` 136, `ioperm` 173,
  `remap_file_pages` 216, `migrate_pages` 256, `bpf` 321, `map_shadow_stack` 453,
  `lsm_get_self_attr` 459, `lsm_set_self_attr` 460, `setxattrat` 463, `getxattrat` 464,
  `file_getattr` 468, `file_setattr` 469.
- **`EBADF`/`ENODEV`/`ENOTDIR`/`EPROTO`** — a descriptor or path was resolved:
  `landlock_add_rule` 445 (`EBADF`), `landlock_restrict_self` 446 (`EBADF`),
  `process_mrelease` 448 (`EBADF`), `uprobe` 336 (`EPROTO`),
  `fchroot` 472 (`ENOTDIR`), `quotactl` 179 (`ENODEV`).
- **`EINTR` (4)** — `restart_syscall` 219, which only has meaning mid-restart.
- **answers `0` with zero arguments** — these prove existence but say nothing about
  the work, and each needs a real-argument measurement before it can be called a
  gap of any shape: `modify_ldt` 154, `iopl` 172, `mbind` 237, `set_mempolicy` 238,
  `get_mempolicy` 239, `move_pages` 279, `pkey_mprotect` 329, `pkey_free` 331,
  `set_mempolicy_home_node` 450, `rseq_slice_yield` 471.
- **`memfd_secret` 447 returns a descriptor** on that host, so the call is fully
  served there.

## What this changes, and what it does not

It does **not** say any of the 50 should be implemented tomorrow. Several are
already recorded as deliberate, measured refusals in
[`01-syscall-table.md`](01-syscall-table.md) §1.1 with the reason and the
prerequisite — `pkey_*` (no PKU on this hardware, so serving them would hand a
caller a boundary that is not there), `cachestat` (no page cache to describe),
`memfd_secret` (no per-page direct-map carve-out). Those decisions stand: a call
that Linux serves and this kernel refuses *on purpose* is a **named gap**, not an
omission, and §3.2 prefers the honest `ENOSYS`.

What it does change is the starting point for the next pass. Before this file, the
answer to "what is left" was a 69-item list with no ranking. Now the 19 matching
refusals are closed business, and the 50 are ordered by what their answer proves:
the `EPERM`/`EFAULT`/`EINVAL` groups are calls other software can reach and expect
to work, and the ten that answer `0` to zero arguments are the ones that most need
a **real-argument** measurement before anyone decides what they should do.

## Which numbers **real workloads** actually reached

The census above is a synthetic sweep: it calls every number on purpose. The
complementary question is which numbers real programs hit and were refused — and
the kernel answers it directly, because `note_enosys` prints
`mpos: syscall <nr> -> ENOSYS` once per number.

Grepping every gate lane's serial log and the Chromium image's, across the whole
suite plus a Chromium run, finds **exactly two** such notes:

| machine | number | what it is | Linux's answer |
|---|---|---|---|
| aarch64 | 98 | `futex` (`202` on x86_64) | `ENOSYS` for that call shape too |
| x86_64 | 202 | `futex` | `ENOSYS` for that call shape too |

Both are the same thing on two machines, and both are **correct**: `t-pi-futex`
issues `FUTEX_LOCK_PI | FUTEX_CLOCK_REALTIME`, a clock flag the PI operations do
not take, and Linux refuses that pair with `ENOSYS` as well. So it is a served
call refusing one argument shape, not a missing call.

**What this establishes**: with 338 x86_64 programs and 334 aarch64 programs
exercising the ABI — the ioctl families, DRM, evdev, sockets, namespaces, the
SysV/POSIX IPC families, ptrace, seccomp, io_uring, and the Chromium run — **no
real workload asked for a syscall this kernel does not have**. The 50 candidates in
the table above are therefore *latent* gaps: reachable in principle, unreached in
practice by everything this repository runs.

That is a different and more useful statement than "50 gaps remain". It means the
next pass should pick from those 50 by **what a program outside this repo would
need**, not by what the list's order suggests — and it means a red
`mpos: syscall N -> ENOSYS` line appearing in a future lane is genuinely news,
because today there is none.

**A note on reading those two numbers.** `futex` is `98` on aarch64 and `202` on
x86_64, and `nr_aarch64.rs` carries the mapping in a comment beside the constant.
A reader who assumes a number means the same thing on both machines will read
`syscall 98` as `getrusage` and conclude the kernel has a defect it does not have
— which is what happened while writing this section, and is the same trap
`AGENTS.md` §5 records for `struct stat` and the `O_*` words.
