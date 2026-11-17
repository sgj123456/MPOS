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

- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).
