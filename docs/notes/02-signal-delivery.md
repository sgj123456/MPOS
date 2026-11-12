# §2 Phase 2: Signal Delivery (High Priority) — **DONE**, with three named remainders

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 2: Signal Delivery (High Priority) — **DONE**, with three named remainders

Signals are now delivered to installed handlers, not just recorded. A
signal that a process catches builds an `rt_sigframe` on the user stack,
redirects the return into the handler, and resumes the interrupted code
when the handler returns through `rt_sigreturn`.

### 2.1 Signal Frame Delivery — policy in `src/syscall/linux/proc.rs`, layout in `arch/<arch>/signal.rs`

**Both machines' frames are the machine's since §13.2's Stages 2/3b**: x86_64's in
`src/arch/x86_64/signal.rs` and aarch64's in `src/arch/aarch64/signal.rs`. Both now carry the
interrupted floating-point state, and the ABI puts it in a different shape on each: aarch64
writes an `fpsimd_context` record inside `uc_mcontext.__reserved`, while x86_64 keeps the
`fxsave` block at the frame's end and points `sigcontext.fpstate` at it (offset 440, so a
952-byte frame). `t-signal-frame` measures both halves, and the handler destroys every vector
register after reading the frame so that "they came back" can only mean the frame carried them.
A frame whose `fpstate` a handler nulls is Linux's "reset the state", measured, and comes back
with zeroed `xmm` and `mxcsr` `0x1f80` — not with the handler's registers left live. The
measurements and the landing are §15.91 (`docs/uapi/` §5.1 item 75).

`build_frame` writes a `rt_sigframe` (restorer, `sigcontext`, `siginfo_t`,
saved mask), points the return at the handler and its three arguments, and
honours `SA_ONSTACK`, `SA_RESTORER` and `SA_RESETHAND`. `proc.rs` keeps only the
policy (which stack, which mask, which restorer) plus the two scheduler calls the
machine cannot make; `hal::SignalFrame`'s `enter`/`exit` lay out and read back the
frame in each machine's `signal.rs`. `deliver_signal` runs on every syscall return
(`src/syscall/mod.rs`) and, for `SIG_DFL`, consults the `default_action`
table (terminate / stop / continue / ignore). `handle_rt_sigreturn` restores
the whole register file and the blocked mask.

### 2.2 Signal Mask Enforcement — implemented

`rt_sigprocmask` records the mask and delivery skips blocked signals
(`take_deliverable_siginfo`, `deliverable`), which stay pending until
unblocked; the unblocking syscall's own return is what runs them.
`rt_sigreturn` puts the pre-handler mask back. `rt_sigpending` answers the
pending set independently of the blocked set.

### 2.3 Signal Queue / siginfo — implemented

Each pending signal carries a `SigInfo` record (index into a
`[SigInfo; SIGNALS]` slot table on the task — correct for the non-queued
standard signals 1..=31 this kernel names). `kill`/`tgkill` deliver
`SI_USER` with the sender's pid/uid; `sigqueue`/`rt_sigqueueinfo` deliver
`SI_QUEUE` with the caller's `si_value` word through to the handler; the
`setitimer` clock delivers `SI_TIMER`. `build_frame` writes the real record
(`si_signo`/`si_errno`/`si_code`/`si_pid`/`si_uid`/`si_value`) instead of a
fixed `SI_USER`.

*Real-time signals — landed (stages A+B+C):* the pending set is now a canonical
64-bit map (signal `n` at bit `n-1`, so the libc wire set and the kernel's
internal set are one shape), the disposition table spans `1..=64` so `rt_sigaction`
installs a real-time signal and `kill`/`tgkill`/`rt_sigqueueinfo`/`timer_create`
accept the range (the refusals are `0` and `>64` at every door plus `SIGKILL`/`SIGSTOP`
for an `rt_sigaction` *install* — a bare query of either is served, §15.11 — as Linux), and
real-time signals are **queued** — a per-task bounded FIFO ring carries every copy
of `SIGRTMIN..=SIGRTMAX` in order with its own `si_value`, whether raised by
`sigqueue` or `kill`, while standard `1..=31` still coalesce. This closes the
"`rt_sigaction` answers `EINVAL` for `32..=64`" and "`33..=64` have no queueable
number" gaps.

*The queue's bound — landed 2026-10-17:* the last named divergence above (the
silent drop) is closed, in Linux's shape. The quantity is the **target user's**
queued total — across that user's tasks and across signal numbers — and the limit
is the **target's** `RLIMIT_SIGPENDING`; `sigqueue`/`rt_sigqueueinfo` answer
`EAGAIN` at it, while `kill`/`tgkill` drop the copy and still answer 0. Measured
on Linux 7.2 before it was built (`.workbuddy/probe_sigpending{1,5,7,9,10}.c`):
with the soft limit at N, N `sigqueue`s of one number are taken and the N+1th is
`EAGAIN`; a *second* RT number meets the same total; a same-uid sibling that
queued nothing is refused while the filler holds the quota; a sender whose own
limit is 1 still delivers to a target whose limit is 4; a standard signal is
never refused (a second copy coalesces instead of allocating); and `kill` at the
same moment returns 0. The storage is still the per-task ring, so the resource's
*default* is the ring's size (`RT_QUEUE_LEN`) rather than Linux's policy value
(`max_threads / 2`, ~125k), and a caller that raises the limit past the ring buys
no more room — the one bounded corner left, named where the ring lives.

Pinned by `t-rt-signal-queue` (three `sigqueue`s drained FIFO with distinct
values, two `kill`s yielding two copies, a handler run once per queued copy, and
the bound's own claims: the fill stops at the limit, the copy past it is
`EAGAIN`, a second number meets the same total, a sibling is refused, `kill` is
not) and the `RtQueue` host unit tests. The resource's row is pinned by
`abi.rs`'s `rlimits_report_the_startup_defaults_and_refuse_unknown_rows`, which
reads the default against `RT_QUEUE_LEN` itself.

### 2.4 Verification

`user/abitest/src/bin/t-signal-delivery.rs` runs a handler end-to-end in
QEMU: it asserts the handler fires once and the interrupted `kill` resumes
with its own return (2.1); that a blocked signal is held, reported pending
by `rt_sigpending`, and delivered on unblock (2.2); and that a `sigqueue`
payload arrives as `SI_QUEUE` with its `si_value` intact (2.3). The host
side pins the `si_code` values and the siginfo table.

The two waits a signal has to reach are pinned by
`user/abitest/src/bin/syscall/t-tty-eintr.rs` (a trapped `SIGALRM` ends a
blocked terminal read with `EINTR`, the handler having run first, and the
retried read still gets the line typed while it was not reading; the same
timer with `SIG_IGN` does *not* interrupt it) and
`t-tty-vtime.rs` (the `VMIN`/`VTIME` table, timing included).

The rest of the family is pinned the same way, one program per half of the
rule: `t-sa-restart.rs`, `t-tty-nonblock.rs` and `t-wait-eintr.rs`, each
described below.

### 2.5 Interruptible waits, and `VMIN`/`VTIME` — implemented

A blocking read of a terminal is the wait almost every interactive program
sits in, so its two rules are the two most expensive ones to get wrong.

**It ends on a signal the caller will actually run a handler for.** Measured
on Linux, per signal, with `strace`: `SIGALRM`, `SIGWINCH` and `SIGCHLD` each
give `read(...) = ? ERESTARTSYS`, then the handler, then
`rt_sigreturn({mask=[]}) = -1 EINTR` — the interrupting is universal, not
per-signal. And measured in the other direction: a signal whose *default
action* is to be ignored, still at `SIG_DFL`, does not interrupt anything —
a `SIGCHLD` from an exiting child, a `SIGWINCH` nobody trapped, a `SIGURG`, a
`SIGCONT` each left the read blocked for its full watchdog. So
`table_has_deliverable` skips `SIG_IGN` **and** `SIG_DFL`-with-an-ignore
default; without that second half a shell's own read of its terminal would
come back `EINTR` after every job it runs.

The kernel's own answer is `-EINTR` with nothing consumed, which is what
`VfsError::Interrupted` (→ `err_of` → `EINTR`) carries from the node up to the
dispatch, and `CharDevice::read` returns a `VfsResult` for the same reason: a
0 on the read side means end of input, and a program told its terminal closed
stops reading instead of retrying after its handler ran. `block_until` stays
what it always was — fatal signals only — for the waits that are the
scheduler's or the boot task's own; the interruptible ones go through
`block_until_deadline`, which is also where a `VTIME` clock is read.

**`VMIN`/`VTIME` is an inter-byte clock, not a deadline on the read.** The man
page is ambiguous about what `VTIME` times; the table below is what a pty
answer was, on Linux, for the cases a text editor branches on (all reproduced
by `LineDiscipline::read_wait`'s host tests):

| mode | `VMIN`/`VTIME` | queued | Linux answered |
|---|---|---|---|
| raw | `0`/`0` | nothing | 0 bytes, 0 ms |
| raw | `0`/`0` | 3 bytes land at 100 ms | 0 bytes at 0 ms — the poll does not wait for them |
| raw | `1`/`0` | nothing | blocked (watchdog `EINTR` at 2 s) |
| raw | `1`/`0` | 1 byte at 100 ms | 1 byte at 100 ms |
| raw | `0`/`5` | nothing | 0 bytes at 503 ms |
| raw | `0`/`5` | 3 bytes at 100 ms | 3 bytes at 100 ms |
| raw | `0`/`2` | 1 byte at 100 ms | 1 byte at 100 ms, not at 300 |
| raw | `2`/`5` | 1 byte at 100 ms | 1 byte at 604 ms — `VTIME` after the **byte** |
| raw | `2`/`5` | bytes at 100 ms and 300 ms | 2 bytes at 304 ms |
| raw | `2`/`5` | bytes at 100 ms and 900 ms | first read 1 byte at 608 ms; second read 1 byte at 1300 ms |
| raw | `3`/`5` | 5 bytes at 100 ms | 5 bytes at 100 ms — `VMIN` is a floor |
| raw | `3`/`5` | 2 bytes at 100 ms | 2 bytes at 604 ms |
| raw | `5`/`0` | 2 bytes at 100 ms | blocked at 2 s with the 2 bytes still queued |
| canonical | `1`/`5` | "abc", no newline | blocked at 4 s |
| canonical | `1`/`5` | "abc" at 0, newline at 1200 ms | 4 bytes at 1200 ms |
| canonical | `0`/`5` | nothing, then `^D` | 0 bytes at the instant of the `^D`; the next read blocks again |
| raw + `O_NONBLOCK` | `0`/`5` | nothing | `-1/EAGAIN` at 0 ms — `O_NONBLOCK` outranks the clock |

The previous code got all three of these wrong. Readiness was
`out.len() >= VMIN.max(1)` — with the comment that `VMIN == 0` would "make every
read return nothing at all, which reads as end-of-file" — but `VMIN == 0` is not
an EOF: it is the poll, and the answer is 0 bytes without waiting. busybox `vi`
and `vim` set `VMIN=0/VTIME=1` to tell a lone `ESC` from the start of an escape
sequence, so pressing escape wedged them with the terminal still open. `VTIME` was
read by nothing anywhere in the kernel, so no read could ever answer a timeout.
And the wait was `block_until`, which woke for a signal and went back to sleep:
`vim`'s timeouts are `SIGALRM` and a resize is `SIGWINCH`, and both arrived, were
seen, and were thrown away.

Named gaps this leaves, and the three that have since closed:

- **`SA_RESTART` — closed.** The rewind is the frame's, in the shape this
  section predicted: `build_frame` stores `sc::RIP = regs.rcx - 2` (the
  `syscall` instruction is two bytes) and `sc::RAX = nr`, so `rt_sigreturn`
  resumes *on* the call with the caller's own arguments - they are in the frame
  the handler has not touched - and `rt_sigreturn` itself needed no change,
  because the frame *is* the record. Three things have to agree, and each is
  measured: the call came back `EINTR` (in this kernel: a wait ended with
  nothing consumed), the disposition being delivered carries `SA_RESTART`, and
  `abi::restartable(nr)` lists the call. Measured on Linux 7.2 (2026-10-01, a
  terminal read whose `SIGALRM` fired at 100 ms and whose byte arrived at
  400 ms): **with the flag** the handler runs at 100 ms and the *read* returns
  the byte at 400 ms; **without it** the same call is `-1/EINTR` at 100 ms and
  a retry gets the byte. `poll(2)`, `nanosleep(2)` and `epoll_wait(2)` answer
  `-1/EINTR` at 100 ms *with* the flag set, which is `signal(7)`'s
  never-restarted set and the reason `restartable` is an allow-list rather than
  "everything but those" — a rewind re-executes the call, so one that must not
  be run twice is wrong to include even if it never returns `EINTR` today.
  `t-sa-restart` pins both halves.
  - The flag is also what made `nanosleep`/`clock_nanosleep` become
    *interruptible*: a never-restarted call that cannot be interrupted is inert,
    and this one could not be - it slept through a signal and answered 0.
    Linux's answer is `-1/EINTR` with the time *left* in the second pointer, and
    that remainder is the whole reason the pointer exists: musl's `sleep()` is
    `nanosleep(&ts, &ts)` in a loop, so without it every signal doubles the
    sleep. Both are pinned, the remainder included.
- **`O_NONBLOCK` on a terminal — closed**, with the table below as its spec.
  Measured on Linux 7.2 (2026-10-01, a pty slave in raw mode with `O_NONBLOCK`
  set):

  | mode | queued | answered |
  |---|---|---|
  | `0`/`0` | nothing | **0 bytes** — it is the poll, and it never waited |
  | `1`/`0`, `0`/`5`, `2`/`5` | nothing | `-1/EAGAIN` **at 0 ms** — the clock is not consulted |
  | `2`/`5`, `5`/`0` | fewer than `VMIN` | **those bytes**, immediately: a minimum is a floor on a *wait* |
  | canonical | a partial line | `-1/EAGAIN` |
  | canonical | a complete line | the line |
  | canonical `0`/`0` | a partial line | `-1/EAGAIN` — canonical consults neither |
  The rule lives in `LineDiscipline::read_wait`, which is where the blocking
  table already lived and where a host test can hold both columns: a fourth
  input (`O_NONBLOCK`) and a fourth answer (`ReadWait::WouldBlock`). `Tty`
  carries the description's bit, `sync_nonblocking` is what puts it there, and
  the pty *master*'s bit goes to the pair rather than to the slave's terminal -
  the order matters, because `PtyMaster::as_tty` answers the slave's terminal
  (that is what makes `sudo`'s `TCGETS` on a master work). `t-tty-nonblock`
  pins the table, the `open`/`F_SETFL`/`FIONBIO` routes and the two ends.
- **The other bare `block_until` waits — closed**, and the list was longer than
  this section knew: `pipe.rs` (read, and write as a partial count),
  `eventfd.rs`, `signalfd.rs`, `inotify.rs`, `unix.rs` (the datagram queue, the
  connect-room wait and the accept queue), the pty slave's write back-pressure,
  and the AF_PACKET and route-netlink reads - the same wait behind two more
  descriptors, which `udhcpc`'s retry loop and `ip`'s own event loop are built
  on. `sched::block_until_interruptible` names the shape; a *write* that already
  stored bytes reports the partial count rather than `EINTR`, which is Linux's
  rule and the one that cannot duplicate bytes when the caller retries.
  `t-wait-eintr` pins each wait twice: the signal ends it with the handler
  having run, *and* the same call answers normally once the thing it waited for
  arrives, so a kernel that broke the data path to pass the first check fails
  the second. What is left of the family, each named rather than half-served:

  - **A pty write cut inside an expanded `CR LF`** re-offers that one input
    byte, so its carriage return lands twice. The count is in the writer's own
    units by construction (`input_prefix`), and the alternative - counting the
    byte - would lose the line feed; both render the same line.
  - **`wait4(2)`/`waitid(2)` — LANDED.** Their park (`block_until_child_event`
    and its siblings) was fatal-signal-only, so a caught signal woke the task and
    the park swallowed the wake. They now park on `block_until_on_interruptible`,
    which checks `table_has_deliverable` on the same borrow, and a catchable
    signal ends the wait with `EINTR`; the syscall gate restarts it under
    `SA_RESTART`, so `restartable` now lists `WAIT4`/`WAITID` — the exclusion it
    carried was precisely because the park never answered `EINTR`. Measured on the
    host (Linux 7.2.7: 2 s child, `SIGALRM` at 100 ms — `SA_RESTART` → the child's
    pid at 2000 ms, flag clear → `-1/EINTR` at 100 ms) and pinned in the guest by
    `t-wait-eintr` (a blocked `wait4` and `waitid` both end with `EINTR`, handler
    run, then the retried call still collects the child — the counterfactual that
    the wait path is not broken). The `SA_RESTART` rewind for the set that *does*
    restart stays in `t-sa-restart`.
  - **The console's serial transmit wait** (`console::write_bytes` parks on ring
    space), the same shape on the one wire the kernel writes from its own tasks.
  - **`ioctl(2)`'s per-request verdict — LANDED.** Linux rewinds an interrupted
    request only when its driver returned `-ERESTARTSYS`, so `SA_RESTART` restarts
    `DRM_IOCTL_SYNCOBJ_WAIT`/`..._TIMELINE_WAIT` and most other requests not at
    all. `abi::ioctl_restartable` now names exactly those two (magic `'d'`, command
    `0xc3`/`0xca` — the only requests this kernel's park answers `EINTR` for), and
    `restart_verdict` reads the request word (`regs.rsi`) for `nr::IOCTL` where it
    reads the number elsewhere; `build_frame`'s rewind re-runs the saved arguments
    unchanged, and the wait's `timeout_ns` is an absolute deadline, so a rewind can
    neither extend it nor lose it. Grounded in the driver's own return code (Linux
    `drm_syncobj.c` parks the fence and returns `-ERESTARTSYS` on a signal); pinned
    in the guest by `t-drm-sync`, which blocks a `WAIT_FOR_SUBMIT`, fires a
    `SIGALRM` into it from a `setitimer`, and has a forked child submit the fence
    afterwards — the wait answers `EINTR` without `SA_RESTART` and `0` with it, so
    a kernel missing the rewind (which gives `EINTR` in both) fails the second
    branch. The one remaining ioctl *wait* gap is `FBIO_WAITFORVSYNC`, which
    discards its `WaitEnd` (`ioctl.rs`) and so is not signal-interruptible at all —
    a separate §2.5 item, not a restart verdict. **Gated:** `cargo xtask test`
    ends `=== SUMMARY pass=141 fail=0 (141 built)` with QEMU exit 33 and
    `t-drm-sync === PASS` — the branch that proves the pin is load-bearing is B,
    which answers `0`; a kernel without the per-request verdict answers `EINTR`
    there too and fails it.
- **Canonical mode ignores both** — `VMIN` included. That is POSIX's wording
  ("non-canonical") and the measured 4-second block above, so nothing is
  missing here; it is recorded because the pair *looks* like it applies.

**And writing the pin for the pty's back-pressure found the bound unenforced.**
The pty master's buffer is capped at `TO_MASTER_MAX` (64 KiB) and a writer that
fills it is supposed to park - but the *guard* that chose between parking and
pushing straight through asked `crate::arch::interrupts_enabled()`, and that
question is false in an interrupt handler **and inside every syscall**
(`MSR_FMASK` masks `IF` on `syscall` entry). So a userland `write(2)` to a pty
slave never parked and never met the cap at all, while a kernel task's did:
measured by `t-wait-eintr`, a 96 KiB write returned 96 KiB and grew the buffer
past its 64 KiB bound — the worst-case rule (§4 item 4) unenforced on a queue a
user program fills. The context is a parameter now, and only a call site can
answer it: `Wire::Task` for `write(2)` and the terminal's own answers,
`Wire::Interrupt` for the echo of a byte that has just arrived (which must not
park - it runs in the handler that carried it). A full buffer then ends the
write with the errnos Linux gives it: the partial count, `EAGAIN` under
`O_NONBLOCK`, `EINTR` under a signal, and the count is in the writer's own units
(`input_prefix` maps the wire's bytes back, since `ONLCR` makes one `LF` two).

### 2.6 Who may signal whom — **DONE** (2026-10-21; `SIGCONT`'s session door 2026-10-27)

**Why**: the `kill` family delivered whatever it was asked to. There was no
uid rule and no `CAP_KILL` question anywhere in `src/sched/runtime/`, so a
process that had dropped to another user could signal a root process — and the
new `pidfd_send_signal` (item 92) would have been a second door onto the same
hole. Linux's rule is `kill_ok_by_cred` plus the capability, and this kernel
had neither half.

**Measured whole on a current host first** (`.workbuddy/probe_killperm.c`,
Linux 7.2.11, uid 1000): `kill(1, 0)` is `EPERM` (1), `kill(self, 0)` is 0,
`kill(child, 0)` is 0, and `kill(-1, 0)` is 0 — the last one because it reaches
at least one process the caller may signal, which is a *set* question rather
than a group one (§2.10 carries that set and its own measurement). The uid comparison is **not** against the target's effective id: Linux's
`kill_ok_by_cred` compares the sender's real/effective id against the target's
**real or saved-set** id, which is why a setuid program's effective root id is
not on the list.

The record door's shape was measured in the same pass
(`.workbuddy/probe_qinfo.c`): `si_code` `-1`, `-2`, `-7`, `-8` and `-100` are
accepted and arrive **verbatim**, with the caller's `si_pid`/`si_uid` (4242 and
4242 in the probe) and `si_value` unchanged; `si_code` 0, +1 and **`SI_TKILL`
(`-6`)** are `EPERM` for a signal to another process. The old door overwrote
`si_pid`/`si_uid` with the sender's and collapsed every non-`SI_QUEUE` code to
`SI_USER`, which is a record the caller did not write.

**The landing** is one rule and one place: `crates/cred::kill_ok` (the four uid
comparisons, host-tested and a doctest) and `sched::may_signal` (the same rule
over the task table, falling through to
`userns::Caller::capable_in(&target.user_ns, CAP_KILL)`). The capability is
asked of the **target's** namespace, not the caller's — the same
object-shaped question `setns` and `AF_PACKET` already ask. `CAP_KILL` is now
named in `crates/cred` (number 5). The kernel's own raises deliberately do not
go through `may_signal`: a timer, a child's death and a console handover have
no userspace sender to judge.

`kill`/`tkill` ask it for a positive target (with `kill`'s group branch going
through `send_signal_group_checked`, which skips members the caller may not
signal and answers `EPERM` only when none are permitted), `tgkill` asks it after
its own thread/group checks, and `rt_sigqueueinfo` asks it after the `si_code`
rule. The record door now builds `SigInfo::signed` from the caller's
`si_errno`/`si_code`/`si_pid`/`si_uid`/`si_value`, and `SI_TKILL` joined
`si_code >= 0` in the `EPERM` set.

**Pins**: `t-signal-permission` (new, default group, both machines). On a host
run (`MPOS_ON_HOST=1`) it checks uid 1000 against pid 1 and itself. In the guest
it forks three children:

* a child that drops all three ids to 65534 asks its root parent (and itself)
  through `kill`, `tgkill` and `rt_sigqueueinfo` — the three doors answer
  `EPERM`, `0`, `EPERM`, `EPERM`; the root parent's `kill(child, 0)` is 0,
  which can only be `CAP_KILL` because the uids match nowhere;
* a uid-0 child signals a 65534 grandchild (0), empties its capability sets
  with `capset`, and signals the same grandchild again (`EPERM`) — the
  capability's own counterfactual;
* a root child hands `rt_sigqueueinfo` an `SI_TKILL` record and an `SI_USER`
  record addressed to its parent, and both are `EPERM` (the code rule, not the
  uid rule).

Using `SIGCONT` for the delivered records is deliberate: the uid claims must
survive a counterfactual that removes the rule, and `SIGUSR1` would end the
test process instead of printing the red claim.

**Counterfactual** — `may_signal` made to return `true` (the rule removed),
every pin kept: the one red program is `t-signal-permission`, with exactly four
claims red — `a 65534 child's kill(root parent, 0) is EPERM`, the `tgkill`
claim, the `rt_sigqueueinfo` claim, and `without CAP_KILL the same signal is
EPERM` — while `root may signal the 65534 child (CAP_KILL)`, the grandchild's
capability control, and both `SI_TKILL`/`SI_USER` refusals stay green. The four
are the rule; the green ones are the controls that say it is not merely "every
signal is refused".

**Gate**: x86_64 `pass=193 fail=0`, `mpos-kunit 32 passed, 0 failed`; aarch64
`pass=192 fail=0` (192 staged, 14 left out by `tests.toml`), `mpos-kunit 32
passed, 0 failed`. Host unit suite 586, xtask 139.

**The `SIGCONT` session door landed on 2026-10-27**, the last named clause of
this item. Linux's `check_kill_permission` has a third door beside the ids and
the capability: a caller those both refuse may still send a **continuation** to
a process of its own **session**, and no other signal crosses that line. The
old named text said this kernel had "process groups standing in for sessions";
by the time the door was taken, the session field, `setsid` and
`become_session_leader` all existed (§13.12's getty is their user), so the
clause was served rather than deferred again.

**Measured on a current host first** (`.workbuddy/probe_sigcont_session.c`,
Linux 7.2.11, a container's root so the uid drop is reachable), four shapes
through `kill`, `tgkill` and `rt_sigqueueinfo`:

| sender → target | `kill(…,0)` | `kill(SIGCONT)` | `tgkill(SIGCONT)` | `rt_sigqueueinfo(SIGCONT)` | `kill(SIGUSR1)` |
|---|---|---|---|---|---|
| 65534 child, same session as root parent | `EPERM` | **0** | **0** | **0** | `EPERM` |
| 65534 child, own session (`setsid`) | `EPERM` | `EPERM` | `EPERM` | `EPERM` | `EPERM` |
| root child, either session | 0 | 0 | 0 | 0 | 0 |

So the session is not a second authority: it is the one bypass `SIGCONT` has,
and only when the ids *and* the capability refuse (`kill(…,0)` and `SIGUSR1`
stay `EPERM` in the same-session row).

**The landing** is `sched::may_signal(sig, target)` — the signal is the
question now, because every door asks the same rule — with
`capable || (sig == SIGCONT && caller.session == target.session)` behind the
uid and capability halves.

**Pin**: `t-signal-permission` grew from 12 to **23 claims**. The old program
already built the shape the measurement needs (a dropped child and its root
parent); the SIGCONT claim it carried was **corrected from `EPERM` to `0`** —
that expectation was itself the divergence, written before the session half
was measured. Added: the same-session continuation through all three doors,
the `SIGUSR1` control at the same session, a second child that `setsid`s first
(both the SIGCONT and the record door `EPERM`), and the `getsid` comparison
that says which session each child is in.

**Counterfactual** — the session clause removed (`capable` alone), the pins
kept: x86_64 `pass=210 fail=1`, the one program `t-signal-permission`, exactly
three claims red — `the same session lets a 65534 child continue its parent`,
`the record door takes the same session door` and `the tgkill door takes it
too`; every cross-session claim and the `SIGUSR1` control stay green.

**The pin found its own trap**: on aarch64 the delivered `SIGCONT` woke the
parent's parking `read` and the collector read an empty answer, so every claim
taken from `-1` was red while the kernel was right. The read/write helpers
retry `EINTR` now, and the note is here because a pin that *sends a signal*
must survive the signal it sends.

**Gate**: x86_64 `pass=211 fail=0`, aarch64 `pass=210 fail=0`, `mpos-kunit 34
passed, 0 failed` on both; host suite 640.

**Named**: *which* thread runs a handler is still the delivery path's choice
rather than Linux's `signal_wake_up` scan; the handler **table** itself is one
object per thread group now (§2.7), so a worker's install is the leader's.
`kill(-1)`
used to be named here too — the caller's own group rather than "every process
the caller may signal" — and it landed as §2.10 (2026-10-25), together with
the one copy per process that a *group* send owes.

**The door this item anticipated** — `pidfd_send_signal` — landed with the rest of
the pidfd family as §6.8. It asks this same `may_signal`, and its group scope goes
through `send_signal_group_info`, which applies the rule per member.

---

### 2.7 A child inherits its parent's dispositions — **LANDED** (2026-10-21)

Every process here used to start at `SIG_DFL`: `fork`/`clone` built the child's
`Actions` from `default()` whatever the parent held. POSIX and Linux copy the
table. The divergence was visible the day `clone3` landed (§6.10): its
`CLONE_CLEAR_SIGHAND` could not be served because the flagged and unflagged
child were identical, and the item named the inheritance as the prerequisite.

**Measured whole on a current host first** (`.workbuddy/probe_sig_inherit.c`
and `.workbuddy/probe_altstack_fork.c`, Linux 7.2.11):

| The door | Linux 7.2.11 |
|---|---|
| a `fork` child | the parent's handler, its `SIG_IGN`, the blocked mask **and the alternate stack** (`ss_sp` and size unchanged) |
| after an `exec` | a caught handler becomes `SIG_DFL`; `SIG_IGN` stays; the blocked mask crosses; a **pending blocked signal stays pending**; the old alternate stack is gone |
| `clone3` without the flag | the parent's handler runs in the child |
| `clone3` with `CLONE_CLEAR_SIGHAND` | the child reads `SIG_DFL` |
| `CLONE_CLEAR_SIGHAND` beside `CLONE_SIGHAND` | `EINVAL`, with `CLONE_VM` or without - and so beside `CLONE_THREAD`, whose chain requires `CLONE_SIGHAND` |
| a thread's `rt_sigaction` | the process's table (shared) |

**The landing.** The fork/clone child copies `parent.dispositions` unless
`CLONE_CLEAR_SIGHAND` asks for the `exec`-shaped table; the alternate stack is
copied with the rest. `clone3_verdict` serves the flag and refuses the
`CLONE_SIGHAND` pair. `exec` resets only the caught slots
(`Actions::reset_caught`) and no longer flushes the pending set, while still
dropping the alternate stack.

**Pins.** `t-signal-inherit` (new, default group, both machines), **12
claims**: the fork child's handler/ignore/mask/alt-stack, and a real `execve`
of the test's own image reporting DFL/IGN/mask/pending through a pipe.
`t-clone3` **37 → 40 claims**: a plain child's handler, the flagged child's
`SIG_DFL`, and the two `EINVAL` pairs. `t-signal-frame`'s fatal-fault child now
restores `SIG_DFL` itself: after the frame checks installed a `SIGSEGV` handler
the child inherited it, ran it, and exited 0 - Linux's fork rule, which the
first gate run of this change measured as `pass=196 fail=1` with exactly that
claim red (`status 0x0`).

**Counterfactual** (the fork fields put back to `default()`/`None` and the exec
reset back to "all default, pending cleared"), every pin kept: a guest
single-test run of `t-signal-inherit` is **4 of 12 red** - the fork
inheritance, "an ignored signal stays ignored", "a pending blocked signal is
still pending", and the whole after-exec word. The DFL and mask claims stay
green, which is what makes the four the change's own.

**The stack this found.** With the inheritance in place the aarch64 release
sweep crashed at the first user `fork`: `spawn_copy`'s frame disassembled to
~13.8 KiB of a 16 KiB release stack, and the fork path's callees no longer fit.
The isolation was measured, not guessed: with only the fork fields reverted the
machine booted; with only the 16-byte alternate-stack copy in place it crashed
two ways on two boots (a frame free of `0xe000`, then a branch to address 0).
The release stack is **32 KiB** now (`STACK_ORDER` 3; the debug figure stays
64 KiB), and `src/sched/runtime/mod.rs` carries the frame measurement. The
aarch64 gate then passed 196/196.

**Gate**: x86_64 stage 1 1074 tests / 0 failed, stage 2 `pass=197 fail=0`
(197 built, 6 lanes), `mpos-kunit 32 passed, 0 failed`, exit 0; aarch64 196
staged, `pass=196 fail=0`, `mpos-kunit 32 passed, 0 failed`, exit 0.

**The thread table became one object on 2026-10-27**, closing this section's
first named clause. `Task.dispositions` is `Arc<signal::SignalHand>` now - a
leaf `critical_section::Mutex<RefCell<Actions>>`, the shape `FsStruct` already
uses - and the doors are the ABI's: `CLONE_SIGHAND` (implied by `CLONE_THREAD`,
and legal beside `CLONE_VM` without it) hands the child **the same** handle, a
plain `fork`/`clone` gets `SignalHand::copy`, `CLONE_CLEAR_SIGHAND` gets a fresh
`SIG_DFL` table, and `exec` **unshares** before it resets (Linux's
`unshare_sighand`), so a sibling cannot see the new image's table.

**Measured on a current host first** (`.workbuddy/probe_thread_sighand.c`,
Linux 7.2.11): a `pthread`'s `sigaction(SIGUSR1, handler)` leaves the leader
reading that same address; a change the leader makes **after** the thread
exists is what the live thread reads back; and a `fork` child keeps its own
copy, so the parent's later change does not move it.

**Pin**: the new `t-thread-sighand` (7 claims, default group, both machines,
green on the host's Linux first): the two sharing directions, the live-thread
change (a copy made at spawn would already agree, so the thread is running when
the leader writes), one delivery running the shared handler exactly once, and
three fork-copy claims. The non-thread `clone(CLONE_SIGHAND)` door is named
rather than pinned - a raw child needs the per-machine `clone` argument order,
which this std-side program cannot spell without an architecture split of its
own.

**Counterfactual** (the thread path made `parent.dispositions.copy()` again),
the pins kept: x86_64 `pass=211 fail=1`, the one program `t-thread-sighand`,
exactly two claims red - `a handler installed by a thread is the leader's
handler` and `a disposition the leader set is what the live thread reads` -
while the delivery, both fork-copy claims and the parent's own change stay
green. The first draft of the pin let a wrong table **kill** the program (the
leader's `SIGUSR1` was still `SIG_DFL`, and the delivery claim fired it), so
the delivery now installs its own handler first; a pin for a rule must fail,
not hang or die, when the rule is absent.

**Gate**: x86_64 `pass=212 fail=0`, aarch64 `pass=211 fail=0`, `mpos-kunit 34
passed, 0 failed` on both; host suite 640; `test-host` 200/226 with the new
program green there.

**Named**: the kernel-spawned program path (`load.rs`, init and the boot's
services) still starts at `SIG_DFL`, which is what those programs want but is
not a user `exec` and is not claimed as one; and the post-exec alternate-stack
state is not pinned in the Rust pin because the hosted runtime installs one of
its own at startup - the C probe measures the reset and the kernel resets it in
`exec`.


### 2.8 The kernel itself generates `SIGPIPE` — **LANDED** (2026-10-21)

Every signal this file has described so far was sent by a *task* (`kill`,
`sigqueue`, a terminal's line discipline). `SIGPIPE` is the one the kernel
generates from inside a syscall, and the difference is observable: Linux raises
it during the `write`, so the handler runs on the way out of that same call and
sees the interrupted `write`'s `EPIPE` in its `ucontext` - measured, and the
one primitive is `sched::raise_sigpipe` (the same `send_signal` a `kill` uses,
so the record is `SI_USER`).

Which doors raise it, which answer `EPIPE` silently, and the partial-write
split are the networking surface's rules; the measurements and the landing are
§4.1.13 (item 97), and the pin is `t-sigpipe`'s 32 claims. This section records
only the signal-side fact: **generation is per-syscall, delivery is still the
syscall-exit path**, and a blocked `SIGPIPE` therefore waits pending while its
`write` returns `EPIPE` - both pinned.

### 2.9 The kernel sends the signal a dying parent's child asked for - `PR_SET_PDEATHSIG` (LANDED 2026-10-23)

The option was one of the ones the `prctl` catch-all refused with `EINVAL` (the
honest answer for an option nothing backs), and it is the last of that family with
real state behind it: the value is read by the **exit path**, not by a getter.

Measured on a current host first (`.workbuddy/probe_pdeathsig.c`), every claim
then re-run against a real kernel before it was pointed at this one:

* a fresh process can already have one - this sandbox's shell carries `SIGKILL`,
  which it inherited from the chain that started it, so the value **survives
  `execve`**. A `fork` child reads **0** whatever its creator asked for: the
  setting is cleared at the fork, which is why the claims pin the pair.
* `0` clears; any value up to `_NSIG` (64) is accepted - **`SIGKILL` and `SIGSTOP`
  included**, so nothing filters them, which is the opposite of what a reader
  expects and the reason it was measured rather than inferred from `kill(2)`'s
  rules; `100` and `-1` are `EINVAL` **with the previous value kept**.
* `PR_GET_PDEATHSIG` answers **0** and writes the signal through its pointer; a
  null pointer is `EFAULT`. (The first draft of the claim expected the signal
  back as the return value and failed on the host - the implementation was right
  and the expectation was not.)
* the delivery: a child that has asked is sent the signal when the task it names
  as its parent dies, and a **handler** is what the pin observes it with, because
  the orphan is reparented to init at the same moment and is no longer the
  test's child to `wait` for.

The substrate is a `u32` on `Task` plus one call: the walk that hands a dying
task's children to a new parent - `exit`'s `forget_original_parent` half - sends
each child its signal through `force_signal` (a kernel raise, `SEND_SIG_PRIV`'s
shape) and wakes it, because that primitive wakes nobody.

**The first gate run was red, and the fault was the pin**, which is the part
worth recording: the middle process exited before the victim had run `prctl`, so
the victim's field was still 0 when the parent died and the signal was never
queued. On the host the same race had been won by the other side. The fix is a
pipe, not a sleep: the victim writes one byte once it is armed and the middle
waits for it, so the ordering is the kernel's and the claim reads the delivery
rather than a race. A pin for "the kernel sends something at a moment" has to
order that moment itself.

**Pin**: `t-prctl`, grown to **43 claims** - the `EFAULT`, the round-trip, the
range refusal with the old value kept, the clearing, the fork inheritance and the
delivery through a handler - all of them green on the host where the two
`PR_CAPBSET_*` claims are the pre-existing unprivileged-host divergence, and all
of them green in the guest.

**Gate**: x86_64 `pass=205 fail=0`, stage 1 `1100 tests, 0 failed`,
`mpos-kunit 32 passed, 0 failed`, exit 0.

### 2.10 `kill(-1)` is every process, and a group send is one copy per process (LANDED 2026-10-25)

Two divergences lived in the same two doors, and one measurement found both.
`kill(-1)` answered the **caller's own process group** — a different set
entirely, so a sibling that had made itself a group leader of its own was never
reached — and `kill(-pgid)`/`kill(0)`/the tty's `Ctrl-C` delivered the signal to
every *thread* of a process, so a two-thread target ran its handler twice.

**Measured on a current host before anything was changed**
(`.workbuddy/probe_kill_neg1.c` and `.workbuddy/probe_kill_pgrp.c`, Linux
7.2.11), inside a fresh user+pid namespace so that "every process" was a set
small enough to watch — the probe stood as pid 1 of its own namespace and
nothing outside it could be touched:

* **a lone caller reads `ESRCH`** — `kill(-1, 0)` and `kill(-1, SIGUSR1)` alike
  — and its own handler never runs: the caller's own thread group is not in the
  set;
* **a sibling in its own process group receives it**, and the caller's own
  group does not: the set is every thread group, not one pgrp;
* **a process in a nested pid namespace is not named** (it has no number where
  the caller stands) and survives the call;
* **one copy per process**: a two-thread target's handler runs **once**, for
  `kill(-1)` and for `kill(-pgid)` alike — Linux's `group_send_sig_info` puts
  one copy in the thread group's shared pending set;
* the answer is `0` as soon as one process was a candidate, and `ESRCH` with no
  candidate at all; a candidate the caller may not signal is counted and
  skipped, never an `EPERM` for the whole call (Linux's `kill_something_info`
  keeps `retval` for every answer except `EPERM`).

**The landing.** `sched::send_signal_every_process` walks the thread groups,
keeps the leaders whose number **in the caller's pid namespace** exists and is
above 1, drops the caller's own group, and sends to the leader — or to a live
worker when the leader has already exited under one (`docs/notes/` §15.86).
`group_signal_targets` is the same "one task per thread group" walk for the
group doors, so `send_signal_group` (the tty's `Ctrl-C`/`SIGWINCH`),
`send_signal_group_checked` (`kill(-pgid)`) and `send_signal_group_info`
(`pidfd_send_signal`'s process-group scope) all deliver once per process.
`proc::handle_kill`'s `-1` arm calls it directly rather than through
`kill_group`, because the two shapes are different questions: a group is one
authority, the machine's process set is many.

Kernel tasks are not in the set: they have no userland to run a handler and no
`/proc` name, so only a count could tell them apart from user processes — the
one thing Linux's loop over `for_each_process` would do differently. Named, not
approximated: a `kill(-1)` whose only candidates are kernel threads is `ESRCH`
here and `0` there.

**Pin**: the new **`t-kill-all`**, 4 claims, all four measured against a real
host first by running the program itself (`MPOS_ON_HOST=1`,
`ok:` for `ok:`), and every claim runs inside a fresh user+pid namespace — which
is also why the program is safe to host-run: an unguarded `kill(-1)` on a
development host would signal every process of the runner's user.

**Both counterfactuals were run, one door at a time**, and neither is a
prediction:

* **the old per-thread fan-out restored** (the two group doors iterating
  `group_members` again), everything else this item's: `t-kill-all` is the one
  red program, claim 3 alone — `Some(42)`, which is `40 + 2`: the two-thread
  target ran its handler **twice**. The claim itself had to be sharpened to see
  it: its first version let the second thread **park**, and a copy left pending
  on a parked thread is never delivered before the process leaves, so the count
  read 1 with the bug present. The pin's worker now sleeps in 1 ms takes and the
  count is read after a 200 ms grace, which is what makes the claim fail without
  the rule;
* **the old `kill(-1)` restored** (read as the caller's group), everything else
  this item's: the program does not reach a claim line at all — it is **killed
  by `SIGUSR1`** ("User defined signal 1" on the wire), because that group
  contains the test process *outside* the fresh namespace, which has no
  handler. The old answer did not merely reach the wrong members of the
  caller's namespace: it reached out of it.

**Gate**: x86_64 `pass=209 fail=0`, aarch64 `pass=208 fail=0`, `mpos-kunit
32 passed, 0 failed` on both, host unit suite 1120 tests / 0 failed;
`t-kill-all`'s four `ok:` lines byte-identical on both machines.

**Named**: *which* thread runs the handler is still the delivery path's
choice rather than Linux's `signal_wake_up` scan. The table itself is the
thread group's one object now (§2.7), so a handler a worker installed is the
one that runs. `SIGCONT`'s session half, named beside it since §2.6, landed on
2026-10-27 there.

### 2.11 A POSIX timer belongs to the thread group (LANDED 2026-10-29)

The timer table lived on the **task**, and that one fact answered three
questions wrong. A timer a worker thread created died with the worker; its
expiry was queued on the task that created it rather than the process; and a
`fork`'s child could not have inherited one because there was nothing of the
group's to inherit. Linux keeps the object on the thread group - the timer is
the process's, addressable by any of its threads, and its signal is
process-directed.

**Measured on Linux 7.2.11 first** (`build/scratch/timerown/probe.c` and
`fork.c`, musl-static, raw `SYS_timer_*`; the probe children flush before
`_exit`):

| what the probe did | Linux 7.2.11 |
|---|---|
| a worker thread creates a periodic `SIGEV_SIGNAL`/`SIGUSR1` timer (100 ms), arms it, **exits** | `after-exit gettime rc=0`, `remaining=0.099987546s interval=0.100000000s` - the process kept it |
| the main thread's blocked `rt_sigtimedwait(SIGUSR1)` | two expiries, `sig=10`, `si_code = -2` (`SI_TIMER`) - the record is the process's |
| a *different* thread (the main one) calls `timer_settime`/`timer_gettime`/`timer_delete` | each answers 0 - the id is the process's, not its maker's |
| the process `fork`s after creating a timer | child: `gettime rc=-1 errno=22 (EINVAL)`, `delete rc=-1 errno=22`; parent: `gettime rc=0`, still armed (`remaining≈29 s`) |
| sixteen `timer_create`s in one process | `many created=16` - the bound is the process's own resource limit, not a fixed table |

**The landing** (`src/sched/runtime/task.rs`, uncommitted parent `d43790a`):

* `timer_alloc`, `timer_arm`, `timer_state`, `timer_overrun` and
  `timer_remove` resolve `let leader = t.task(t.current).tgid;` and act on that
  record. The leader is the one that holds `timers` (`Task::timers`,
  `TIMER_SLOTS`); a worker's own array is never filled, so its exit takes
  nothing with it. A `fork` child is a fresh leader with an empty table, which
  is what makes the `EINVAL` half of the measurement fall out.
* `check_posix_timers` walks **leaders only**: `let leader = task.tgid ==
  owner; if !leader { continue; }`. It does **not** skip a zombie leader (a
  group can outlive the thread that made the timer), and it requires
  `group_live` - at least one live member of the leader's group - before any
  delivery. The expiry's reload still happens before the target is chosen, so a
  `SIGEV_NONE` timer, or one whose group is already gone, keeps counting.
* A process-directed expiry (`TimerNotify::Signal`) picks a live member that
  does **not** block `signo`: leader first, then any unblocked member, then any
  live member (Linux's `signal_wake_up` chooses the same way among unblocked
  threads). `SIGEV_THREAD_ID` still needs the target's `tgid == owner` and
  skips a target that has exited.

**Pin** `user/tests/src/bin/t-timer-create.rs`, the "a timer belongs to the
thread group" family - 14 claims composed from the probe's own calls: block
`SIGUSR1` in the main thread so an expiry can be *taken*; a worker makes and
arms a 50 ms periodic timer and exits; the main thread reads it (`0`, interval
still there), re-arms it one-shot 30 ms, **polls** a zero-timeout
`rt_sigtimedwait` until the record arrives (2000 ms budget - a claim that
parked would turn the pin into a harness timeout, not a failed claim, and the
first version did exactly that: under the counterfactual it hit its 120 s
program budget), checks `si_code == -2` and deletes it; then a `fork` whose
child is refused both calls (`EINVAL`) while the parent's copy still answers 0.
Host reference first: `MPOS_ON_HOST=1 ./target/x86_64-unknown-linux-musl/debug/t-timer-create`
→ all fourteen `ok:`, `PASS`.

**Counterfactual** (`build/scratch/timerown-cf2.log`, `src/sched/runtime/task.rs`
reverted to the per-task table, the pin kept): `CF-EXIT=1`, `pass=228 fail=1
(229 built)`, the one red program `t-timer-create` failing exactly six claims -
`FAIL: the timer outlives the thread that made it (got EINVAL)`, `FAIL: and it
still carries the interval that thread armed (got 0)`, `FAIL: another thread of
the group may re-arm it (got EINVAL)`, `FAIL: the expiry lands on the process
once its maker is gone (got EAGAIN)`, `FAIL: and it is the timer's own record
(SI_TIMER) (got 0)`, `FAIL: any thread of the group may delete it (got
EINVAL)`. The fix was restored and `grep -c "let leader = t.task(t.current).tgid"`
answers 5.

**Gate** `build/scratch/timerown-gate2.log`: `EXIT=0` - x86_64 `pass=229
fail=0`, aarch64 `pass=228 fail=0`, `mpos-kunit 34 passed, 0 failed` on both
arms, stage 1 `1178 tests, 0 failed 24.51 s`.

**Named divergences.** The count is `TIMER_SLOTS = 8` per group: Linux's
sixteenth `timer_create` succeeds where this kernel's ninth is `EAGAIN`
(`timer_alloc`'s `find` over the leader's table), deliberately bounded rather
than approximated. The pending set is per task (§2.7) - a process-directed
expiry sits on the task the scan chose, so if every thread blocks the signal
it waits there, whereas Linux's shared group set lets the first thread that
unblocks take it. `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID` stay
`EOPNOTSUPP` (no per-task accounting) and `TIMER_ABSTIME` stays `EINVAL` (one
clock, no absolute domain), both as before, both in `docs/uapi/` item 128.

### 2.12 A process-directed signal goes to a thread that may take it (LANDED 2026-11-04)

Every process-directed door named one task and one task only, and it was the
*wrong* task whenever the leader happened to be blocking that signal. This is
the mechanism behind item 193's shutdown stall (`docs/notes/` §7.4h's still
open question (b)): weston's group is sent `SIGTERM`, the copy lands on a
thread whose mask blocks it, the one thread that would have taken it stays
parked in a futex, and nothing in the group ever runs a handler.

**Measured on a current host before anything was changed** (Linux 7.2.11;
`build/scratch/futexsig/p.c` and `p2.c`, `gcc -O1 -pthread`):

* `p.c` - the mask untouched, the handler installed, a worker parked in
  `syscall(SYS_futex, word, FUTEX_WAIT, 2, NULL, NULL, 0)`, then
  `kill(getpid(), SIGTERM)`: `kill(self, SIGTERM) = 0 errno=0`,
  `handler ran=1 (on tid 2)` - the **main** thread - and `worker is STILL
  parked after 3 s`. So the copy goes to the leader, and a probe that stops
  here learns nothing about the parked thread;
* `p2.c` - the main thread blocks `SIGTERM` with `pthread_sigmask` **before**
  `pthread_create` (the worker inherits the mask) and the worker unblocks it
  itself before parking: `kill(self, SIGTERM) = 0 errno=0`,
  `handler ran=1 (on tid 9)` - the **worker** - and
  `worker futex returned ret=-1 errno=4 (Interrupted system call)`.

Linux's answer, then: a process-directed signal is handed to a thread that
may take it, and that thread's park ends with `EINTR` (`complete_signal` /
`wants_signal` pick the member; the group's shared pending set means the
copy is not tied to the thread that was chosen first).

**Measured on this machine** (`build/scratch/futexsig/t-futexsig.c`, musl
static, booted with `MEM=3072`; `build/probe/oneshot/serial.log`):

```
t-futexsig
futex: kill(self) = 0 errno=0
  handler ran=0 on tid=0 (main is 5)
  park is STILL there after 3 s
```

and the stall dump that followed: `slot 5 … park 2 (block_until_nanos)
syscall 202 parks=316` (the leader, in the futex) beside `slot 6 … park 2
(block_until_nanos) syscall 202 parks=1` (the worker, also in 202). The
handler never ran at all: the copy sat pending on the masked leader, which
answered "I am not taking it" once, and the only thread that could take it
was never woken.

**The mechanism.** `handle_kill`'s positive arm resolved one task -
`named_process`, leader first, else `members.first()` - and sent to it;
`send_signal_info` never looks at `task.blocked`, because a mask is a
*delivery* question and it only answers the *enqueue* one. Every
process-directed door had the same shape: `kill`, `kill(-pgid)`/`kill(0)`
through `send_signal_group`, `kill(-1)` through `send_signal_every_process`,
`rt_sigqueueinfo`, `pidfd_send_signal`, and the tty's job control.

**The fix.** `sched::process_signal_target(tgid, sig)` (in
`src/sched/runtime/child_signal.rs`) is Linux's `wants_signal`: the leader
when `task.blocked & signal::bit(sig) == 0`, else the first live member that
does not block it, else any live member (a group whose threads all block it
still gets the copy, and drops it with the group, which is Linux's answer
too). `send_signal_process_info` sends to that member and answers the `Sent`
outcome; `send_signal_process` mints the `SI_USER` record and keeps its bool.
Every group-shaped door now loops its groups and calls the process door, so
one copy lands per thread group, on a thread that can take it - the fan-out
the old `send_signal_process` did would have run a handler once per thread.

**Pinned** by `user/tests/src/bin/t-signal-park.rs`'s third shape,
"whose park a process-directed signal ends": a leader that blocks `SIGTERM`
before forking its worker, a worker that unblocks it and parks in
`FUTEX_WAIT`, and the three claims "a process-directed signal reaches a
futex waiter whose sibling blocks it", "and that futex_wait call answers
EINTR", "and the handler ran on the parked thread, not the masked leader".
`MPOS_ON_HOST=1` on Linux 7.2.11: 10 of 10 claims `ok:`, `PASS`.

**Counterfactual** (only `handle_kill`'s positive arm reverted to
`send_signal(target, …)`, kernel *and* loader rebuilt): the six pre-existing
claims stay `ok:` and exactly the three new ones flip -
`FAIL:  a process-directed signal reaches a futex waiter whose sibling blocks
it`, `FAIL:  and that futex_wait call answers EINTR`,
`FAIL:  and the handler ran on the parked thread, not the masked leader`,
`=== t-signal-park exit=1`.

**Named remainders.** The copy still sits on the one task the pick chose, so
a thread that blocks the signal *after* being picked keeps it there until it
unblocks; Linux's shared group pending set would let another thread take it
first. And the pick is `wants_signal` without its "prefer a thread not
already handling it" half - one copy, one handler run is what the pin's
third claim reads, and nothing in the sweep depends on the finer choice yet.

### 2.13 A handled signal waits for a system call — **CLOSED ON AARCH64 2026-11-12,
STILL THE GAP ON X86_64**

**The question.** A task in ring 3 that computes and never makes a system call: does a signal with a
handler reach it? Linux says yes, at the next trap. This kernel says no: the signal is queued and
waits for the task's next kernel entry.

**Measured on Linux first** (`build/scratch/timerclock/iret.c`, gcc -O1, musl static, host 7.2.14).
The loop is `for (i = 0; i < limit && !got; i++) x += (double)i;` with a `volatile double`
accumulator and a `sig_atomic_t` the handler sets - **nothing in it is a syscall**, and it leaves
either because the handler ran or because the counter ran out:

```
create(PROCESS_CPUTIME, SIGUSR1) = 0 errno=0
settime(30 ms) = 0 errno=0
loop left by THE SIGNAL after 4257819 iteration(s) (limit 400000000)
```

So the handler ran while the task was still in its own ring-3 code, 4 257 819 iterations into a
400-million-iteration limit: the signal did not have to wait for a kernel entry.

**The same program, asked of this machine** (`build/scratch/timerclock/iret2.c` staged as `t-iret`,
budget 5 ms so the deadline is certainly reached, `build/boot-iret1.log`,
`build/probe/oneshot/serial.log:163-166`):

```
iret: create(PROCESS_CPUTIME, SIGUSR1) = 0 errno=0
iret: settime(5 ms) = 0 errno=0
iret: loop left by count exhaustion after 20000000 iteration(s) (limit 20000000)
iret: gettime = 0 errno=0 remaining 0 ms
```

The deadline was reached - `timer_gettime`'s remaining is 0 ms, so the CPU the loop burned was
charged and the expiry scan fired - and the handler still never ran: the loop ran to its iteration
limit. The report line after it is the next syscall the task makes, and that is where the signal
would have been handed over.

**Mechanism, in the code — as it stood before the fix.** `preempt_now`
(`src/sched/runtime/task.rs`) is the clock's hook, and it calls `check_itimer()`, then
`deliver_pending_signal()`, then `schedule`. But `deliver_pending_signal` only took a **fatal**
signal - `take_fatal_signal()` then `die_of_fatal_signal(sig)` - so a `SIGKILL` or a
default-disposition `SIGTERM` ended a spinning task at the next tick. A signal with a *handler* needs
the interrupted task's register file to build a frame on, and the clock's hook had no such image, so
the signal went back into the pending set, where `src/syscall/mod.rs`'s
`linux::deliver_signal(regs_ptr, nr, ret)` found it on the task's next syscall.
`deliver_pending_signal`'s own doc said so, in the sentence this section existed to falsify: *"the
syscall return path is the only place that has one"*. It is not - see **How it was closed** below.

Why the frame is the blocker, per machine - both read from the code:

- **x86_64**: `extern "x86-interrupt"` handlers receive an `InterruptStackFrame`, which is
  `RIP`/`CS`/`RFLAGS`/`RSP`/`SS` only. Entering a handler rewrites the task's `sigcontext` and
  resumes it at the handler's own `PC`, so the general-purpose registers the trap interrupted are
  needed, and the hardware saved none of them; the ABI relies on the handler preserving the
  registers it happens to use, which leaves no memory image to rewrite.
- **aarch64**: the IRQ stubs save `x0..x18`, `x29` and `x30` - "exactly the registers a Rust callee
  may clobber" (`arch/src/arch/aarch64/vectors.rs:16-19`) - and `x19..x28` are callee-saved, so
  `irq` restores them itself. The interrupted image on the stack is therefore not complete there
  either.

  **Re-read and corrected 2026-11-12**, because the sentence above was *reasoning from the call
  rule* rather than from the stub, and the stub does not quite bear it out - the difference matters
  to anyone doing the work. The EL0 IRQ entry (`__vec_el0_irq`) allocates `736` bytes and stores
  `x0..x18`, `x29`, `x30` **and the 528-byte FP half** (`q0..q31`, `FPCR`, `FPSR`) - the same shape
  the EL0 *sync* stub uses, and the sync stub already carries all thirty-one integer registers. So
  the EL0 interrupt path is much closer to a complete image than "the callee-saved set is the
  handler's problem" suggests: what is missing from it is exactly **`x19..x28`**, ten registers, and
  the FP state is already there (`TRAP_FPU_OFFSET`, 176).

  Two consequences for the plan, both from reading rather than from a boot:
  * the aarch64 half is *smaller* than the x86_64 half - draw the frame with ten more `stp` pairs
    and it is complete, where x86_64 needs a naked stub replacing an `extern "x86-interrupt"` one;
  * `docs/notes/` previously implied `irq`'s own preservation covered it. It does not cover the
    *interrupted program's* `x19..x28`: `irq` preserves them across its own Rust call, which
    restores whatever the handler left, not what the program had. A frame built from this stack
    image would restore the handler's values into the program's callee-saved registers - a subtler
    corruption than a missing image, and the reason the ten registers have to be *stored at trap
    time* rather than reconstructed later.

**The instrument now exists, and its first version was wrong.** The reading above claims the probe
was "staged as `t-iret`", and it was not: no such program was in the tree, so
`build/boot-iret1.log` could not be reproduced by anybody - a measurement whose instrument is missing
is a sentence, not evidence. `user/tests/src/bin/t-iret.rs` is that program now, in the
`diagnostic` group, and it reproduces both sides on demand (`build/boot-iret-gap{1,2}.log` after
`build/boot-iret-new1.log`):

```
host (Linux 7.2.14)   control: 400000000 of 400000000
                      armed:   left by THE SIGNAL after 188429911 iteration(s)
guest (this kernel)   control: 400000000 of 400000000
                      armed:   left by count exhaustion after 400000000 iteration(s)
```

Two things about writing it are worth keeping, because each is a trap this file's own subject set:

* **The flag has to be read before any system call.** The first version read `GOT` *after*
  `timer_delete`, and the guest answered "delivered at a trap" for a loop that had run to exhaustion -
  the disarm syscall delivered the pending signal, and the instrument then reported that as an
  interrupt. The question is whether the signal interrupted *the loop*, and the first syscall after
  it is one of the doors the signal can come through. A reading taken after the thing it measures is
  not a reading.
* **Two measurements, not one.** "The loop ended by itself" and "the loop ended because of the
  signal" are the same output from a single run, so the same burn runs twice - once armed, once as a
  control. The host's control reaching its limit is what makes its armed run's early exit mean
  something; without it, a loop that simply could not finish inside the budget would read as a
  machine that delivered the signal.

That the guest contradicts the host under this instrument is the *evidence the gap is real and
reproducible*, which the note did not previously have in a form anyone could re-take.

**What closing it would take.** Each machine's tick vector would need a full user-register image at
trap time - x86_64 a naked stub that pushes a `pt_regs` before calling the handler (the same shape
the syscall gate already uses), aarch64 saving `x19..x28` as well - and then a delivery path that
installs the frame on *that* image, obeying the restart and `sigcontext` rules `deliver_signal`
already encodes. That is a change to both machines' trap entry, not a field, which is why it is
named here instead of approximated.

**The seam the delivery path reuses already exists**, which is the part that makes this landable
rather than a rewrite of the ABI: `capture_frame`/`apply_frame` (`src/syscall/linux/proc.rs:913`,
`:923`) turn a machine frame into the ABI's 27-word register file and back through
`hal::capture_user_regs`/`hal::restore_user_regs`, and they are what `PTRACE_*REGS` and the
group-stop path already go through. A trap-side delivery is therefore "capture from the trap frame
instead of the gate frame, then run the same `build_frame`" - the frame *builder* is shared; only
the pointer handed to it is new. What is genuinely new is the x86_64 stub, because there is no
frame to capture until the general-purpose registers are on the stack.

**A delegated attempt at the x86_64 half failed 2026-11-12**, leaving neither a report nor a changed
line, which is worth recording as a fact about the *size* of the step rather than about the delegate:
the stub has to save a register file, enter the handler with a valid `gs` base and kernel stack, and
return through a path that either `iret`s to a rewritten user image or resumes a kernel context -
and a half-applied version of that is a machine that does not boot. The instrument above is what
came out of that round instead, and it is the thing the next attempt needs most: a reproducible
before/after. Nothing in that attempt is in the tree.

**Who this is visible to.** A program that computes without entering the kernel - a tensor loop, a
numeric solver, a spin-wait on a shared page - holds a `SIGINT`, `SIGALRM` or `SIGUSR1` until it
next calls anything. Ordinary programs rarely notice, because their loops poll something; a
compute-heavy one notices, and its operator sees a process that ignores `Ctrl-C` until it decides to
print. §6.123's pin hit this first from the other side: its CPU-time timer had to call `getpid()`
once per burn chunk for the handler to run at all (`docs/notes/` §6.123, "two traps were paid for
here"), which is the workaround a program would need and should not have to write.

**The rule.** **A signal that is only visible at a system call is a signal a computing program does
not get.** Linux's delivery point is "on the way back to ring 3", and every trap has that way back;
this kernel's is "on the way back from a syscall", which is a subset. The gap is not in the queue or
the handler - both are correct and pinned (§2.12) - it is in *where the frame comes from*.

#### How it was closed on aarch64 — 2026-11-12

**The decision the whole fix turns on.** The syscall return was never the only *moment* a whole
register file exists. It was the only moment this kernel had **built** one into the ABI's 27/34-word
form. An interrupt taken from a program lands with every register the program was using already on
the kernel stack - if the entry stub put them there - so the question stopped being "can a trap
frame serve as a handler's frame" and became "which machine's trap frame already *is* one".

Measured answer, from reading both stubs: aarch64's, once the EL0 IRQ entry saved the whole
`UserFrame`, and x86_64's does not and cannot today. That inverted the work order the earlier
paragraphs assumed - the machine whose interrupt ABI looks more primitive was the easy one.

**What changed, in five pieces.**

1. `__vec_el0_irq` (`arch/src/arch/aarch64/vectors.rs`) now builds the same 816-byte
   `context::UserFrame` the EL0 **sync** stub builds: `sub sp, sp, #816`; `x0..x30` at
   `[sp,#0]..[sp,#240]`; `SP_EL0`/`ELR_EL1`/`SPSR_EL1` at `[sp,#248]/[#264]`; the 528-byte FP half at
   `[sp,#272]..[sp,#752]`; `orig_x0` at `[sp,#800]`; then `mov x0, sp` and `bl {irq}` - the handoff
   the sync stub already used. The ten registers the old frame left to the handler (`x19..x28`) are
   stored at trap time now, which is the only way to keep what the **program** had rather than what
   the handler left. Cost: ten `stp` pairs, 80 bytes, and the three `TRAP_IRQ_*` constants are gone
   because `TRAP_FRAME_BYTES` (816) now serves both EL0 entries.
2. `extern "C" fn irq(frame: *mut ())` takes that address, and inside `if ticked {`, **before** the
   preempt split: `if in_user && !frame.is_null() { crate::hal::kernel::deliver_at_trap(frame) }`.
   Before the preempt, not after: a delivery rewrites the frame, and a task switched away first
   would resume into the frame without the handler in it. The kernel's own IRQ stub passes `mov x0,
   #0` - it interrupted the kernel, so there is no program context to build on.
3. `Gate::capture_trap_regs(frame, tls, out) -> bool` (`arch/src/hal/mod.rs`) is the new capability
   question. It is **not** `capture_user_regs` with a different argument: that op's `result` word is
   a write, and at a trap there is no result to write - every register the frame names is live.
   aarch64 answers the whole `UserFrame`; **x86_64 answers `false`** with the reason in the code: its
   `extern "x86-interrupt"` frame carries five words and the GPRs sit below it at an offset Rust
   cannot name, so building the ABI's file there would invent twenty-two words.
4. `proc::build_frame` split into the capture (the caller's) and `build_frame_at` (one policy, two
   doors, so they cannot drift). The trap door is `proc::deliver_at_trap`: null frame → no; not in a
   task → no; a traced task → no (a ptrace stop is a syscall-exit shape here and a stop off a trap
   frame is one the tracer cannot read); then the same `take_deliverable_siginfo` /
   `current_disposition` / `default_action` ladder the syscall door uses, with `restart = None`
   ("an interrupt restarts nothing") and the result slot left as the interrupted code had it.
5. `src/kernel_services.rs` routes `deliver_at_trap` to `syscall::linux::deliver_at_trap`, which
   calls `sched::check_itimer()` first - a tick is the moment a timer that came due is noticed, and
   a signal queued by this very interrupt has to be in the set this looks at.

**A bug the gate caught, and it is the interesting one.** The first version took the machine down on
every aarch64 lane before any test program ran:

```
KERNEL PANIC: panicked at src/sched/runtime/task.rs:1937:32: there is no task 255
```

The boot's own **EL0 check** runs a ring-3 program with no task behind it, so `current()` answers
`SCHED_ID` - reserved slot 255, no task in it - and the delivery's `has_tracer(current())` asked the
table about that slot. The fix is a predicate that names the absence: `sched::in_task()` =
`STARTED && current() != SCHED_ID`, and the trap door answers `false` when it is not true. A trap
path cannot assume a task exists; the boot takes traps of its own, and the only reason this was
found in minutes rather than by a user is that the gate boots the machine.

**The measurement** (`build/test-lane-aarch64-4/serial.log`, `cargo xtask test --arch aarch64
--groups diagnostic`, `build/gate-iret-aarch64b.log` `GATE_EXIT=0`):

```
ok:    control: with no signal, the loop reaches its limit (400000000 of 400000000 iteration(s))
ok:    with a 30 ms CPU-time signal armed, nothing in the loop but arithmetic: it left by THE SIGNAL after 11337551 iteration(s)
ok:    the reading: delivered at a trap = true, delivered only at a system call = false
t-iret: the signal was handed over at a trap (Linux's shape, §2.13 closed on this machine)
```

Against the same program before the change: `left by count exhaustion after 400000000`. The signal
arrived **11 337 551** iterations into a 400-million-iteration limit - the handler ran while the task
was still in its own ring-3 code, which is Linux's shape. On the host the same binary leaves at
7 086 584 of 400 000 000, so the ordering of magnitude matches too.

`t-iret` therefore stopped being a reading and became a claim, with an expectation per *kernel*
rather than per architecture (`host_run() || cfg!(target_arch = "aarch64")`): aarch64 and any host
run assert "leaves at the signal", x86_64's guest asserts "runs to its limit, because this kernel
hands a handled signal over only at a system call". That second half is what makes the x86_64 gap
**pinned as current behaviour** - the day the naked tick stub lands, this program reddens and names
the line to change. It is no longer in `tests.toml`'s `diagnostic` group: a reading with no claim to
fail on was why it was there.

**What remains, and why it is a different change.** On x86_64 the tick handler is
`extern "x86-interrupt" fn timer(frame: InterruptStackFrame)` (`arch/src/arch/x86_64/idt.rs`). That
ABI's prologue saves the general-purpose registers *below* the frame Rust is handed, at an offset it
has no name for. Closing the gap means replacing that entry with a hand-written stub that pushes a
`Regs`-shaped block (the same shape `syscall_entry` already uses) before calling a Rust fn - new
assembly on the one path that preempts, which is why it is a change of its own with its own
before/after rather than a second edit here. `Gate::capture_trap_regs` answering `false` is the
honest placeholder: it makes the ABI layer behave exactly as it did before this round on that
machine, instead of inventing twenty-two registers.
