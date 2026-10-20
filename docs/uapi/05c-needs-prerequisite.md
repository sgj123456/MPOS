# UAPI §5.3

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

### 5.3 Needs its prerequisite named before it can be honest

- **`waitid`'s `P_PIDFD` (idtype 3).** Refused `EINVAL` today because no pidfd
  exists here to resolve: a descriptor can name no task at all, so the honest
  answer is "this idtype is not served". Linux resolves the descriptor through
  its pidfd family and answers `EBADF` for one that is not a pidfd (measured,
  fd 9999, errno 9). Prerequisite: `pidfd_open`/`clone3`'s `CLONE_PIDFD`/
  `pidfd_send_signal` — with those, the door resolves `P_PIDFD` the way it
  resolves `P_PID` today, and `t-waitid`'s claim moves to the host's answer.
  Owner: nowhere yet. `docs/notes/` §15.11 (batch 18).
- **Real-time signals `33..=64`.** `SIGRTMIN` (32) is queueable, maskable and waitable; the
  rest have no queueable number and no disposition storage. Prerequisite: a 64-bit
  disposition table and a queued-signal list per task (`docs/notes/` §2.3).
- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **`PTRACE_EVENT_SECCOMP`** — carrying a `siginfo_t` through a stop/resume, which the
  stop model does not have (`docs/notes/` §8.2).
- **Per-socket buffer sizes.** `SO_SNDBUF`/`SO_RCVBUF` are record-only: the answer
  needs buffers not sized once at set construction (`docs/notes/` §4.3.3/§4.3.4).
  (`SO_LINGER{on, 0}`'s egress reset used to be here and is served now — it turned
  out smoltcp *can* send an unsolicited reset.)
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).
