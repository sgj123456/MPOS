# UAPI §5.3

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

### 5.3 Needs its prerequisite named before it can be honest

*Left this list:* `waitid`'s `P_PIDFD`, which needed a pidfd family to resolve the
descriptor through; item 92 served it, and `t-waitid`'s claim now makes the host's
`EBADF`/negative-`EINVAL` pair.

- **Real-time signals `33..=64`.** `SIGRTMIN` (32) is queueable, maskable and waitable; the
  rest have no queueable number and no disposition storage. Prerequisite: a 64-bit
  disposition table and a queued-signal list per task (`docs/notes/` §2.3).
- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).
