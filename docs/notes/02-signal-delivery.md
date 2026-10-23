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

### 2.6 Who may signal whom — **DONE** (2026-10-21)

**Why**: the `kill` family delivered whatever it was asked to. There was no
uid rule and no `CAP_KILL` question anywhere in `src/sched/runtime/`, so a
process that had dropped to another user could signal a root process — and the
new `pidfd_send_signal` (item 92) would have been a second door onto the same
hole. Linux's rule is `kill_ok_by_cred` plus the capability, and this kernel
had neither half.

**Measured whole on a current host first** (`.workbuddy/probe_killperm.c`,
Linux 7.2.11, uid 1000): `kill(1, 0)` is `EPERM` (1), `kill(self, 0)` is 0,
`kill(child, 0)` is 0, and `kill(-1, 0)` is 0 — the last one because it reaches
at least one process the caller may signal, which is the group half of the same
rule. The uid comparison is **not** against the target's effective id: Linux's
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

**Named**: `SIGCONT`'s session half (Linux additionally requires the sender and
target to share a session for a `SIGCONT` that crosses a thread group; this
kernel has process groups standing in for sessions), and `kill(-1)` remains the
caller's own group rather than "every process the caller may signal" — the
interpretation the door already carried, now filtered by the same rule.

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

**Named**: a thread's table is still a copy, not Linux's shared
`CLONE_SIGHAND` table (the thread path says why: nothing in this system's
userland installs a handler from a thread yet); the kernel-spawned program
path (`load.rs`, init and the boot's services) still starts at `SIG_DFL`,
which is what those programs want but is not a user `exec` and is not claimed
as one; and the post-exec alternate-stack state is not pinned in the Rust pin
because the hosted runtime installs one of its own at startup - the C probe
measures the reset and the kernel resets it in `exec`.


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
