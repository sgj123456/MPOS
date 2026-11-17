# UAPI §5.3

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md);
section numbers are unchanged from the original `UAPI.md`.

### 5.3 Needs its prerequisite named before it can be honest

*Left this list:* `waitid`'s `P_PIDFD`, which needed a pidfd family to resolve the
descriptor through; item 92 served it, and `t-waitid`'s claim now makes the host's
`EBADF`/negative-`EINVAL` pair. Also left: **real-time signals `33..=64`**, whose
prerequisite (a per-task disposition table and a queued list) landed with §2.3 -
`[x]` on the roadmap's board with `t-realtime` as the pin; this entry had gone
stale and was removed rather than left to be re-done.

*What is left here is what a prerequisite still blocks, plus the one item that is
blocked by the machine rather than by a missing mechanism:*


- **The exec argument block is 64x smaller than Linux's, and the prerequisite is a
  stack mapping decision rather than a bigger constant.** Measured on both sides:

  | | Linux 7.3.4 | this kernel |
  |---|---|---|
  | byte budget | `ARG_MAX` = **2 MiB** (`min(¼ × 8 MiB stack rlimit, 6 MiB)`), 1 MiB `execve` accepted | `ARGS_BYTES_MAX` = **32 KiB**; 32 KiB → `E2BIG` |
  | entry count | **no cap**; 6000 `argv` accepted | `ARGV_MAX` = **512**; 513 → `E2BIG` |
  | real need | largest `cmdline` on this host is **1308 bytes**; a typical process totals **2277 bytes** | |

  So no program this repository runs is near the limit, and the 32 KiB cap is not a
  live bottleneck. It is recorded because it is an ABI divergence a caller can
  reach: a build system or a wrapper that passes a long command line gets `E2BIG`
  here for something Linux runs happily.

  **Why it is not a one-line change.** `initial_stack_rsp`
  (`src/sched/runtime/load.rs`) refuses a block larger than
  `USER_STACK_PAGES * PAGE_SIZE` (16 pages = 64 KiB), and its own comment says
  why: the block must land on pages the loader has **already mapped**, because
  the pages below them are demand-paged and "the fault that would claim one has
  nothing to fault on yet - the program has not started". Raising the byte limit
  without touching that is not possible; the prerequisite is to make the
  pre-mapped stack grow with the block, and then to decide the memory bound
  (`AGENTS.md` §7.4's worst-case rule: 256 task slots × a 2 MiB block would be
  512 MiB if every task used it). The honest first step is a measurement of what
  a real program needs - which the table above now has.

- **`AT_HWCAP` is written `0` rather than measured, and on aarch64 that is a
  false claim about the machine.** The auxiliary vector carries 18 pairs here
  against Linux's 22, and most of the difference is harmless (measured in the
  guest, `probe-hwcap`): `AT_FLAGS` is 0 on Linux too, `AT_PLATFORM` is not read
  by musl at all, `AT_SYSINFO_EHDR` is correctly absent because this kernel has
  no vDSO, and `AT_MINSIGSTKSZ`'s absence is safe because a libc falls back to
  8192 where this kernel's `sigaltstack` floor is 2048 (bisected on Linux:
  1536 → `ENOMEM`, 2048 → accepted, so the floor is exact).

  `AT_HWCAP` is the one entry that is a **claim without a measurement**. It is
  zero on both machines, and the two ports differ in what that costs: on x86_64
  glibc and musl read `CPUID` directly, so it is close to decorative; on aarch64
  the bits are load-bearing — 0 is FP, 1 is ASIMD, 8 is ATOMICS — and this
  port's own context switch saves `q0..q31`, so the hardware is there and the
  vector describes a different machine. It errs in the safe direction (a libc
  takes its scalar path rather than executing an instruction the CPU lacks),
  which is why nothing has broken on it.

  **Prerequisite**: a feature probe per machine (`CPUID` on x86_64,
  `ID_AA64ISAR0_EL1` and its siblings on aarch64) plus a decision about
  `AT_HWCAP2`, and the same numbers feeding any future `/proc/cpuinfo`. Until
  then the zero is recorded as a divergence in the source comment rather than
  left to read as a measured fact.

- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).
