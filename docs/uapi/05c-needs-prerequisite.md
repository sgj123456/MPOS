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


- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).
