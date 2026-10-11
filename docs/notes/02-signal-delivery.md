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
`src/arch/x86_64/signal.rs` and aarch64's in `src/arch/aarch64/signal.rs`. One divergence is
named rather than shared, and `t-signal-frame` measures it: **x86_64's frame carries no
floating-point image** - its `uc_mcontext.fpstate` pointer is left null - so a handler that
clobbers `xmm8..xmm15` leaves the interrupted code with them clobbered, where Linux's
`rt_sigreturn` restores them from the `_fpstate` the frame points at. The aarch64 frame writes
and restores its `fpsimd_context` record, and the pin's vector-register half runs there; the
x86_64 half of that record is its own piece of work.

`build_frame` writes a `rt_sigframe` (restorer, `sigcontext`, `siginfo_t`,
saved mask), points the return at the handler and its three arguments, and
honours `SA_ONSTACK`, `SA_RESTORER` and `SA_RESETHAND`. The frame's *bytes*
are the machine's since §13.2's Stage 2: `hal::SignalFrame`'s `enter`/`exit`
lay out and read back the x86_64 `rt_sigframe`/`sigcontext` in
`src/arch/x86_64/signal.rs`, and `proc.rs` keeps only the policy (which
stack, which mask, which restorer) plus the two scheduler calls the machine
cannot make. `deliver_signal` runs on every syscall return
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
accept the range (only `0`, `>64`, `SIGKILL`/`SIGSTOP` refused, as Linux), and
real-time signals are **queued** — a per-task bounded FIFO ring carries every copy
of `SIGRTMIN..=SIGRTMAX` in order with its own `si_value`, whether raised by
`sigqueue` or `kill`, while standard `1..=31` still coalesce. This closes the
"`rt_sigaction` answers `EINVAL` for `32..=64`" and "`33..=64` have no queueable
number" gaps. One bounded divergence remains and is named, not hidden: the ring is
fixed-length per task, so a flood beyond `RT_QUEUE_LEN` drops the extra copy
silently, where Linux's limit is a per-user `RLIMIT_SIGPENDING` and a full
`sigqueue` returns `EAGAIN`. Pinned by `t-rt-signal-queue` (three `sigqueue`s
drained FIFO with distinct values, two `kill`s yielding two copies, a handler run
once per queued copy) and the `RtQueue` host unit tests.

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
rule: `t-sa-restart.rs` (the rewind, its counterfactual, and the calls Linux
never restarts — `poll` and `nanosleep` among them), `t-tty-nonblock.rs` (the
`O_NONBLOCK` table, and which end of a pty pair each bit belongs to), and
`t-wait-eintr.rs` (every other wait, each checked twice: the signal ends it with
the handler having run, and the same call then answers normally when the thing
it waited for arrives).

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

Three consequences the previous code got wrong, all of them the same freeze:

- readiness was `out.len() >= VMIN.max(1)`, with the comment that `VMIN == 0`
  would "make every read return nothing at all, which reads as end-of-file".
  `VMIN == 0` is not an EOF: it is the poll, and the answer is 0 bytes without
  waiting. busybox `vi` and `vim` set `VMIN=0/VTIME=1` to tell a lone `ESC`
  from the start of an escape sequence, so pressing escape wedged them with the
  terminal still open.
- `VTIME` was read by nothing anywhere in the kernel, so no read could ever
  answer a timeout.
- the wait was `block_until`, which wakes for a signal and then goes back to
  sleep. `vim`'s timeouts are `SIGALRM`, and a resize is `SIGWINCH`; both
  arrived, were seen, and were thrown away.

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


---

