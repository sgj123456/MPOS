# UAPI §5.3

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

### 5.3 Needs its prerequisite named before it can be honest

- **Real-time signals `33..=64`.** `SIGRTMIN` (32) is queueable, maskable and waitable; the
  rest have no queueable number and no disposition storage. Prerequisite: a 64-bit
  disposition table and a queued-signal list per task (`docs/notes/` §2.3).
- **`rseq`** (§1.1) — a per-thread registration maintained across preemption.
- **`PTRACE_EVENT_SECCOMP`** — carrying a `siginfo_t` through a stop/resume, which the
  stop model does not have (`docs/notes/` §8.2).
- **Real `MSG_PEEK` over TCP/UDP.** Refused `EOPNOTSUPP` today (honest); serving it needs
  peek-preserving delivery in the stack (`docs/notes/` §12.2 B2).
- **`SO_LINGER{on, 0}` and per-socket buffer sizes.** Both are record-only; the first needs
  an unsolicited reset on egress, the second needs buffers not sized once at set
  construction (`docs/notes/` §4.3.3/§4.3.4).
- **`AF_PACKET` `SOCK_RAW`** (frames with their header) — `EOPNOTSUPP`; needs the header
  passed through untouched. The header *parse* landed with the `mpos-link` leaf
  (`crates/link`'s `EthFrame`, `docs/notes/` §4.5); what is still missing is the socket
  side — writing a caller's header verbatim and checking its length rather than inventing
  one from the device's MAC.
- **A named `SOCK_SEQPACKET` `AF_UNIX` connection** carries no message boundaries — served
  as the byte stream this family has (`docs/notes/` §4.1).
- **Console gaps:** `console=ttyN` pinning is refused rather than approximated, and
  `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c).
- **The `console` serial transmit wait** is fatal-signal-only, like `wait4` (§1.2).
