# MPOS Notes and Evidence

The record behind the roadmap: every measurement, gate run, lesson and trap this
project has paid for, in the words it was learned in. Nothing here is a plan - the
plan is [`ROADMAP.md`](ROADMAP.md) - and nothing here is decoration either:
a claim in the code or in the roadmap that reads arbitrary should have a number here
beside it, and section numbers (`§13.29`, `§7.4`) are what the source comments cite.

Layout: the phase-by-phase evidence (§1-§15) is the body; the appendix at the end
mirrors the roadmap checklist with the evidence that sat under it in the original
status board.

---

## Phase 1: Syscall Completion — **DONE**

The POSIX completion set landed: epoll, timerfd, the `*at` relatives and
modern file forms, the identity/capability family, scheduling, memory
advice, and the machine-level calls — every one implemented per its man
page, with flag values, struct layouts and errnos pinned by host-side
tests. What remains of the phase is listed at the end.

### 1.1 epoll (Critical) — implemented in `src/vfs/epoll.rs`

| Syscall | Nr | Status |
|---------|-----|--------|
| `epoll_create` | 213 | Done (size ignored, must be positive) |
| `epoll_create1` | 291 | Done (EPOLL_CLOEXEC) |
| `epoll_ctl` | 233 | Done (ADD/MOD/DEL, EXCLUSIVE rules, ELOOP nesting ≤ 5) |
| `epoll_wait` | 232 | Done (ms timeout; -1 forever, 0 immediate) |
| `epoll_pwait` | 281 | Done (sigmask validated) |
| `epoll_pwait2` | 441 | Done (timespec timeout, sigsetsize == 8) |

Beyond the plan: level and edge trigger (`EPOLLET` with dip tracking),
one-shot (`EPOLLONESHOT` with MOD re-arm), `EPOLLEXCLUSIVE` vocabulary,
`ELOOP` cycle detection for epoll-on-epoll, `EPERM` for regular files,
close-time entry reaping, and readiness from the same `poll_ready` the
poll(2) family consults — so a wait hears about data the instant it
arrives.

The hang-up and error conditions are the pair that is *not* an interest:
`Inode::poll_hup`/`poll_err` answer them, and `poll`, `select` and `epoll`
report them to every waiter whether or not it asked, because they are often
the last report a descriptor will ever give — udev's `spawn_wait` ends on
`EPOLLHUP` and nothing else (§7.2), so a pipe that stayed quiet about its
writer's exit parked the device manager until its own timeout expired.
`t-epoll` pins the shape in the guest and `vfs/pipe`'s tests pin the answers
on both ends of a pipe and of a socketpair.

### 1.2 timerfd (High) — implemented in `src/vfs/timerfd.rs`

| Syscall | Nr | Status |
|---------|-----|--------|
| `timerfd_create` | 283 | Done (5 clockids, TFD_NONBLOCK/CLOEXEC) |
| `timerfd_settime` | 286 | Done (ABSTIME, CANCEL_ON_SET, old_value) |
| `timerfd_gettime` | 287 | Done (remaining + interval as itimerspec) |

Beyond the plan: an O(1) interval expiry (division, not a loop), a
saturating expiration counter, and the `TFD_TIMER_CANCEL_ON_SET`
registry the `clock_settime` handler walks — the read answers `ECANCELED`
until re-arm.

### 1.3 Other Missing Syscalls — done, 67 of 68

| Syscall | Nr | Status | Notes |
|---------|-----|--------|-------|
| `copy_file_range` | 326 | Done | EXDEV across mounts, in-out offsets, overlap EINVAL |
| `preadv2` / `pwritev2` | 327/328 | Done | RWF_APPEND honoured, -1 = fd cursor, unknown flag EOPNOTSUPP |
| `fallocate` | 285 | Done | mode 0 extends for real; KEEP_SIZE accepted; interior ops EOPNOTSUPP |
| `openat2` | 437 | Done | open_how by size, mode-without-O_CREAT EINVAL, RESOLVE_* EOPNOTSUPP |
| `faccessat2` | 439 | Done | X_OK from stored mode bits, NOFOLLOW honoured |
| `creat`, `utime`, `fchmodat`, `mknodat`, `futimesat`, `readahead`, `splice`, `syncfs`, `syslog` | — | Done | `src/syscall/linux/fs.rs` |
| identity (setreuid…setfsgid, capget/capset), scheduling (sched_setparam…sched_getattr, getpriority/setpriority), memory (msync, mincore, mlock family), machine (personality, getcpu, settimeofday, clock_settime, unshare, setns, seccomp, setdomainname), signals (tgkill, rt_sigqueueinfo, rt_sigsuspend, pause) | — | Done | `src/syscall/linux/posix.rs` |
| `vhangup` | — | Done | dispatched, both cases honest. With no controlling terminal it answers `ENOTTY` (Linux's answer, gated by the corrected `t-posix`); with a controlling terminal it answers `EOPNOTSUPP` — the hang-up itself (tty re-init + `EIO` on open fds) is `NOTES.md` §12.2 B4, a tty-layer feature the kernel does not perform, so it refuses rather than fakes the success. The nice round-trip that used to sit on this row (`getpriority`/`setpriority`) landed in Phase 12 §12.2 B5 and moved back to `Done`. `src/syscall/linux/posix.rs` |
| `accept4`, `sendmmsg`, `recvmmsg` | 288/307/299 | Done | `src/net/socket.rs` |
| `rt_sigpending` | 127 | Done | `src/syscall/linux/posix.rs`; reads `current_pending()` |
| `io_uring_setup/enter/register` | 425-427 | Deferred | per the original phase's own call |


---

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

## Phase 3: Filesystem Enhancements (Medium Priority)

### 3.1 Hard Links — **DONE**

`linkat` creates a real second name (ext4 adds a dirent and bumps `i_links_count`;
ramfs shares the `Arc<dyn Inode>`), `unlink` decrements and frees only at zero,
and `stat`/`statx` now report the true `st_nlink` (ext4 via `Inode::nlink()`,
other kinds falling back to the `.`/`..` convention).

**Changes**:
- Add `link_count()` and `add_link()` / `rm_link()` to `Inode` trait
- ext2/ext4: implement `i_links_count` in on-disk inode, decrement on unlink, free when zero
- ramfs: share `Arc<dyn Inode>` across directory entries
- Test: `ln file1 file2 && echo test >> file2 && cat file1` shows same content

### 3.2 File Locking (fcntl F_SETLK) — **DONE**

POSIX record locks are real: a global table keyed by `(dev, ino)`, each entry
`{owner, pid, class, kind, [start,end)}`, in `src/vfs/locks.rs` (the range math,
conflict rule and split-on-relock are host-tested). Both lock families live in
that one table but never see each other - a `class` of `Record` (`fcntl`
`F_SETLK`/`F_GETLK`/`F_SETLKW`) and `Whole` (`flock`) conflict only within the
class, as Linux defines the two to be independent. `F_SETLK` takes or drops a
byte range and refuses a blocked request with `EAGAIN`; `F_GETLK` reports the
first conflicting lock (its kind, range and holder pid) or clears to `F_UNLCK`;
`F_SETLKW` blocks and wakes on release. `flock(LOCK_SH|LOCK_EX|LOCK_UN)` covers
the whole file (`[0, infinity)`), shared/exclusive arbitration identical to the
record locks, and blocks unless `LOCK_NB`. A holder that dies is pruned on the
next grant/query.

`owner` is deliberately *not* the same thing in the two classes, because Linux
draws the line in two places: a record lock is owned by the thread group, a
`flock` by the open file description (the `Arc<File>` the fd table shares, so a
`dup` shares the lock and a second `open()` does not). `release_on_close`
applies the two different rules - POSIX's "closing any descriptor drops the
process's record locks", and "the flock dies with its own description" - and
`user/tests/src/bin/t-filelock.rs` (`user/abitest/src/bin/t-filelock.rs` before
S6 batch 11, its own flock half since corrected there for the second half of the
second rule: see §15.16) pins both, with the contending-second-`open`
and the sharing-`dup` cases side by side.

*Not a gap:* `read`/`write` do not consult the lock table, which is what Linux
does. These are **advisory** locks: nothing refuses a `read` that walks over
another process's `F_WRLCK`. Linux's mandatory mode is a per-file opt-in -
setgid with the group-exec bit cleared, on a mount that allows it - and is
documented as broken and deprecated; adopting it here would be a divergence
rather than a completion. What the two families *are* used for here is
cooperation: `apk` and `fish` take the locks, and callers that hold one refuse
the other.

### 3.3 File Timestamps — **DONE** (ext4; ramfs legacy calls remain)

`utimensat`/`futimens` now store the access and modification clocks on ext4
(`Inode::set_times`), stamping `ctime` to the current time as the metadata
change it is; `stat`/`statx` report the three clocks separately (ext4 keeps
`i_atime`/`i_mtime`/`i_ctime` on disk) instead of collapsing them to mtime.
`open`/`write`/`truncate` bump mtime/ctime; a fresh inode is created with all
three set.

*Remaining:* `atime` is not bumped on read (ext4 carries it but no read path
updates it). ramfs/tmpfs now keeps one clock - stamped at creation and by
writes, truncates and directory-entry changes - which is what stopped
OpenRC's deptree freshness check from reading every `/run` file as 1970.
The legacy `utime` and `futimesat` now store
through the same `Inode::set_times` path.

**Changes**:
- Add `atime`, `mtime`, `ctime` fields to `Inode` trait (or a `Metadata` struct)
- ext2/ext4: store in `i_atime`, `i_mtime`, `i_ctime` on disk
- ramfs: store in `RamInode` behind mutex
- `stat` returns real timestamps
- `open`/`write`/`read`/`mkdir`/`unlink` update mtime/ctime

### 3.4 Permission and Ownership Model — **DONE** (fully Linux-aligned)

`chmod`/`chown` store real values and every access path enforces Linux's model:
- ext4 persists `i_uid`/`i_gid`/`i_mode` (read into `Inode::mode`/`owner`, written by `set_owner`/`set_mode`); ramfs keeps them in memory.
- `stat`/`fstat` return the stored uid/gid/mode.
- `open()`/`truncate` and the create/remove/`link`/`rename` paths call `vfs::may_access` → `may_as(current_owner, ..)`: root (euid 0) gets Linux's `CAP_DAC_OVERRIDE` — every check passes **except** execute on a non-directory with no x-bit for anyone (so even root gets `EACCES` running a `0644` file, and `access(X_OK)` on one is `EACCES`); otherwise the owner/group/other `rwx` triads gate `R_OK`/`W_OK`/`X_OK`, with the group triad matched against the effective gid **and the supplementary groups**, exactly like Linux.
- Directory **search** (`may_search`, `X_OK` on each path component) is enforced during resolution (`mount.rs`, `open`), not just on the final node.
- The sticky bit gates name removal; `access`/`faccessat` use real ids unless `AT_EACCESS`; `setuid`/`setgid` execution and `chmod`'s clearing of those bits by a non-owner follow Linux.
- A filesystem that keeps no modes (`keeps_modes() == false`) is not policed, so invented permissions never refuse.
- **devfs nodes keep modes and owners now.** Before this a `chmod` on any device
  node answered success and stored nothing — udev's `MODE=`/`GROUP=` rules were
  logs, and `stat` showed a number no check ever read. Every `/dev` inode class
  (the generic stream node, the ttys, the DRM card and its leases, evdev,
  `uinput`, `fb0`, `ptmx`) carries a `devfs::NodePerms` and answers
  `keeps_modes() == true`, so the triads below decide what the device really
  means. Devices are born `0666 root:root` — the number `stat` already gave them —
  where Linux's devtmpfs births `0600` and lets udev widen it; that difference is
  the named remainder, and it needs per-class birth modes and group-name
  resolution, not another mechanism.
- Host-tested: `a_triad_decides_who_may_do_what` (incl. supplementary groups), `root_passes_everything_but_execute_on_a_file_nobody_made_runnable`, `may_as_answers_for_the_node_and_the_identity_it_is_given`.

### 3.5 /proc Enhancements — **DONE**

procfs also carries the top-level `uptime`, `loadavg`, `cpuinfo`, `version`,
`filesystems`, `partitions`, `devices`, `stat` and the `/proc/sys` knob tree
(`kernel.ostype/osrelease/hostname/pid_max/...`), per-pid `statm` and `limits`,
and `/proc/net/tcp` + `/proc/net/udp` - the live socket table `ss`/`netstat`
read. The `/proc/net` rows come from `net::collect_net_sockets` walking the
registry under the stack lock (local/remote endpoints, the numeric TCP state,
inode) and a pure `format_net` renders Linux's exact column layout (address as
little-endian hex, `:port`, state byte) - the formatter is host-tested
(`the_net_table_prints_addresses_and_state_as_linux_does`).

---

## Phase 4: Networking Enhancements (Medium Priority)

### 4.1 AF_UNIX Domain Sockets — **ANSWERED** (SOCK_STREAM)

The path family is real: `src/net/unix.rs` carries `socket(AF_UNIX,
SOCK_STREAM)` through `bind` (which creates a genuine `S_IFSOCK` node in
ramfs, visible to `stat`/`ls -l`/`find`, and `open` on which answers
`ENXIO`), `listen` with a clamped backlog, `connect` by resolved path
(symlinks and mounts ride the normal resolution), `accept`/`accept4`,
`getsockname` echoing the bound path, `shutdown`, and the full data path -
a connected unix socket *is* a `pipe::DuplexEnd` half, so `read`/`write`/
`sendmsg`/`recvmsg`/`poll`/`epoll` all speak to it unchanged. The bind
table is keyed by `(fs_id, ino)` like locks and inotify watches, so
`unlink` of the name retires the binding (hooked in `unlinkat`) while the
listener itself survives - exactly how a daemon outlives its own stale
socket file. Permissions: bind needs write+search on the parent (the
mkdirat rule), connect needs `W_OK` on the node.

Deliberate divergences from Linux (each asserted in `t-unix`/
`t-unix-rustix`): the abstract namespace answers `EINVAL`; a connect to a
bound-but-never-listened name is `ECONNREFUSED` rather than Linux's
implicit rendezvous at a socket nobody listened on; ext4 refuses socket
nodes (`EOPNOTSUPP` - `/run` is ramfs); `mknod(S_IFSOCK)` still refuses, so
only `bind` creates a socket name; `getpeername` answers `AF_UNIX` with an
empty path (accepted peers carry no name). `SOCK_DGRAM` unix sockets are
served whole-message (`t-unix-dgram`), and so is a `SOCK_SEQPACKET` *pair*;
a *named* `SOCK_SEQPACKET` connection is served as the byte stream this
family has, which is the one divergence left in the family.

**`SCM_RIGHTS` / `SCM_CREDENTIALS` / `SO_PEERCRED` are now answered** (see
`t-unix-scm`): a unix (or socketpair) `sendmsg` can hand live descriptors
across, `recvmsg` installs them into the reader's table (respecting
`RLIMIT_NOFILE`, and `MSG_CMSG_CLOEXEC`), `SO_PASSCRED` makes every read
carry an `SCM_CREDENTIALS`, and `getsockopt(SO_PEERCRED)` returns the peer's
pid/uid/gid - the identity a dbus broker authenticates on. The handover rides
the connection's `PipeState` beside the bytes it travels with, so a descriptor
never taken is closed when the queue drops, and the peer's credentials are
captured at `connect`/`accept`/`socketpair`. v1 divergences: on `SOCK_STREAM`
the kernel keeps one cmsg boundary per send rather than per skb, a sender's
`SCM_CREDENTIALS` opt-in is accepted but the kernel always fills the peer's
real ids (no impersonation), and `SO_PEERCRED`/`SO_PASSCRED` are the unix
`connect` path's - a `socketpair` carries descriptors but not credentials.

A *data-less* `sendmsg` used to be the one deadlock left in the family: the
record rode a byte boundary and there was no byte, so the read parked on a
descriptor it already had. `PipeEnd::scm_due` now lets the read see that the
front record is due before it decides to wait, and the read returns the record
with zero bytes rather than waiting for the stream to advance - which is the
one case where a `recvmsg` on a stream can answer 0 and not mean end-of-stream,
so a receiver that takes a short read has to look at its control buffer before
concluding the peer is gone. That is the price of the record, and the
alternative was a hang; `t-unix-scm` pins both the delivery and the
non-blocking second read that follows it.

**A message may be zero bytes long, and receiving one may offer no room.** The
rule sounds like a corner and is not: eudev's worker channel is a
`socketpair(AF_LOCAL, SOCK_DGRAM|SOCK_CLOEXEC, 0, ...)` whose whole message is
zero bytes - `struct worker_message` has no members, so the worker's
`loop_write` is a zero-length `write(2)`, and the credentials that arrive with
it *are* the payload the manager looks its worker up by - and the manager's
`recvmsg` therefore offers an iovec whose length is zero. That shape answered
`EINVAL`, which left udevd's loop stuck on the failing read and every device on
the machine untagged (the board's `t-udev-worker` entry carries the whole
story). What the family now does is Linux's, measured one call at a time before
it was written: a `recv`/`recvfrom`/`recvmsg` with a zero-byte data buffer
**consumes** the front message and answers 0, still delivering
`SCM_CREDENTIALS` when the reader asked and there is room (`MSG_CTRUNC` when
there is not), and `MSG_TRUNC` when the message was longer than the buffer -
the message's own length is its boundary, not the caller's buffer. `read(2)` is
the exception, and it is a *file-level* one rather than a socket rule:
`vfs_read` short-circuits a zero-length count, so `read(fd, buf, 0)` touches
nothing at all while `recv(fd, buf, 0)` takes a message. `netlink` follows the
same rule (a `NETLINK_USERSOCK` socket that sent itself a message was the
measurement).

**And a `listen` backlog is one deeper than the number it names.** Linux's
admission test is `queue_len > backlog`, so `listen(fd, 0)` is a queue of one -
which this kernel read as "never admits" until eudev's control socket turned up
created with exactly `listen(uctrl->sock, 0)` (`udev-ctrl.c`), leaving `udevadm`
unable to reach the daemon at all. Measured on the host: backlogs 0..=3
admitted 1, 2, 3 and 4 connections before the next `connect` answered `EAGAIN`.
`t-unix` pins the depth and the room an `accept` returns; the divergence that
remains is the *unlistened* name, which is still `ECONNREFUSED`.

**And a *negative* backlog is `SOMAXCONN`, not a refusal.** `clamp_backlog` used
to answer `EINVAL` for one, which reads as the man page's rule and is not
Linux's: `__sys_listen` compares the argument to `SOMAXCONN` as an *unsigned*
int, so every negative value lands on the ceiling rather than being refused.
Measured on the host before the rule was written: `listen(fd, -1)` returns 0 for
`AF_UNIX` and `AF_INET` alike, and `ss -lx`/`ss -ltn` then report the backlog as
4096 - `net.core.somaxconn`, not the zero a "negative means a closed queue"
reading would leave. Nor is it a number anybody has to go looking for: `-1` is
what Rust's std passes on Linux from `UnixListener::bind` *and*
`TcpListener::bind`, so every Rust server and compositor asks for it on every
bind - and the `EINVAL` it got here is where niri stopped (§7.4). The rule lives
in one place, `net::unix::clamp_backlog`, which the IPv4 pool reads as well: that
arm had read `-1` as **zero**, a queue one deep, which is the same defect in the
other family's clothes. Pinned from both sides, `t-unix` and `t-tcp-listen`.

### 4.2 ICMP Improvements — **DONE**

`socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` is served by a raw smoltcp socket
(`alloc_socket` → `add_raw` + `ip_to_smoltcp_protocol`), giving `ping` its echo
request/reply path; the `t-udp` resolver test exercises the raw/UDP read path.
The socket now speaks Linux's exact shape both ways: `sendto` takes the
protocol payload and the kernel builds the IPv4 header (no `IP_HDRINCL`
needed), and receives deliver the **complete IP packet** - the earlier
header-stripped delivery is what made busybox's `ping` print a ttl wandering
0..255 (it reads `iphdr->ttl` at offset 8, which fell inside the echo data).
`IP_RECVTTL` is honoured too (setsockopt stored, the answer attached as a
24-byte cmsg, and `msg_controllen` zeroed when a message carries no control
data). `t-rawicmp` pins the delivered bytes; `t-dns`/`t-stdresolve` pin the
resolver's UDP path (12/12 raw queries, 20/20 musl `getaddrinfo`).

Current: auto echo reply only. No user-facing ICMP socket.

**Changes**:
- `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` creates raw ICMP socket
- `sendto`/`recvfrom` on raw ICMP socket
- Enable `ping` command to work

### 4.3 TCP: the loopback, the listener, and the options — **PARTIAL**

Three things live in this section, and the **loopback** (4.3.1) and
**`listen`/`accept`** (4.3.2) are done. The **socket options** (4.3.3) are the
part still open, and of the six, four now steer the stack - `TCP_NODELAY`,
`SO_KEEPALIVE`, `SO_REUSEADDR`'s bind arbitration - while two are stored and read
back without changing a packet.

#### 4.3.1 The loopback is a real wire — **DONE**

`lo` used to be a name the link dump filled in: `rtnl` reported `127.0.0.1/8`
because a reader expects it there, and a segment sent to that address left by the
NIC and was never seen again. It is now an interface over **smoltcp's own
`Loopback` device** (gated on `alloc`, which this crate already enables)
carrying `127.0.0.0/8`, with no routes and its own up flag - so the device this
section kept promising as "fifty lines" turned out to be zero lines, and what was
left was the shape.

The shape is that **smoltcp cannot route it with one socket set.** Its route
lookup is *destination-only* - `Interface::route` answers `in_same_network(dst)`
or the best matching route (`iface/interface/mod.rs:1010,1014`) - and
`Interface::poll` walks every socket in the set it is handed, so a shared set
would have eth0 claim a `127.0.0.1` socket and put the segment on the wire,
whether or not `127.0.0.0/8` was added to eth0. Two interfaces therefore mean two
socket sets, and smoltcp has no operation that moves a socket between them.

So `Stack` is two `Fabric`s (interface + device + set + its own up flag, with
`sockets_mut(home)`/`poll_home(home)` resolving a handle), a socket's channel
carries a `home`, and the home is decided by the first call that names an
address - `bind`, `connect`, `sendto` - through one rule: `127.0.0.0/8` is the
loopback's, everything else (the wildcard included) is eth0's. A socket created
before then is *relocated* by swapping it for a fresh one in the right set, which
is only ever done to a socket nothing has used yet (`Closed` for TCP, `!bound`
for UDP) and always before the call that would consume it.

Two boundaries, recorded rather than hidden. A **raw socket never moves**: its
destination is a per-call argument, so one socket cannot serve two sets - which
is what keeps `ping 127.0.0.1` behaving exactly as it did, travelling the virtual
wire where slirp answers it, as `t-netlat` measures. And a **wildcard listener**
lives on eth0's fabric, so a loopback client does not reach it; the bound
`127.0.0.1` case is the supported one.

`t-udp-loopback` is the instrument: two datagram sockets in one process
exchanging a payload over `127.0.0.1`, an unbound sender relocated by its
destination (with the sender named back *on the loopback*, which is the assertion
a stack that forgot the move fails), and a datagram to a port nobody holds
answered by nothing at all.

#### 4.3.2 `listen` and `accept` — **DONE**

smoltcp has no listening socket: a `tcp::Socket` is *one* connection, and one
that completes a handshake stops listening. Its own `listen` doc points at the
answer - a pool, re-listed as each connection completes - so a listener here is a
**pool** of sockets in `Listen` (at most `LISTEN_POOL_MAX = 4`, because each is
80 KiB of buffers) plus the **queue** of the ones that completed, kept in
`Stack::listeners` and serviced after every poll: a socket that left `Listen` for
`Established` or `CloseWait` becomes a queued connection, one whose handshake
died is dropped and replaced, and the pool is refilled from what the queue leaves
room for. A full queue refills nothing, so the next `SYN` finds no listener and
is dropped - Linux's own answer to a full backlog - rather than being answered
and then abruptly refused.

The contract is Linux's, and each rule was measured on this host's kernel before
it was written:

| Call | Answer |
|---|---|
| `listen` on an unbound socket | succeeds: it implicitly binds `0.0.0.0` and an ephemeral port, and `getsockname` reports `0.0.0.0:port` |
| `listen` on a bound socket | succeeds; `getsockname` reports the bound name |
| `listen` again | succeeds, re-setting the backlog |
| `listen` on a connected socket | `EINVAL` |
| `bind` on a listening socket | `EINVAL` |
| `accept` with nothing queued, blocking | waits, woken by the poll that fills the queue |
| `accept` with nothing queued, non-blocking | `EAGAIN` |
| `accept` on a bound-but-not-listening socket | `EINVAL` |
| `recv` on a listener | `ENOTCONN` |
| `send` on a listener | `EPIPE` (recorded, not pinned: Linux also raises `SIGPIPE` here, and this kernel raises no signal for a broken channel at all) |
| `getpeername` on a listener | `ENOTCONN` |
| `shutdown` on a listener | succeeds for either half, and the queue is left alone |
| accepted `getsockname`/`getpeername` | the connection's own and its peer's names |
| `connect` where nothing listens | `ECONNREFUSED` - smoltcp resets an unclaimed segment, so the loopback answers rather than timing out |
| backlog admission | `queue_len > backlog`, one deeper than the number it names - the rule §4.1 measured on the unix family |

`accept4`'s `SOCK_CLOEXEC`/`SOCK_NONBLOCK` are honoured for this family too; they
had been accepted and dropped while the refusal was the whole implementation.
`/proc/net/tcp` reports a listener as one `LISTEN` row on its bound address (the
pool behind it is not a connection and has no row), and `poll`/`select`/`epoll`
report a listener readable exactly when the queue is non-empty - the same
question `accept` parks on, so a waiter that is woken cannot find the queue empty.

What closing a listener does with the connections still in its queue is a
recorded divergence: they are closed, which gets a `FIN` out through the ordinary
teardown discipline, where Linux sends `RST`. That is the same limitation
`SO_LINGER{on,0}` has below - smoltcp's resets are emitted in reply to an
arriving packet, so an unsolicited reset is not something this stack can send.

`t-tcp-listen` pins the whole shape: a server bound to `127.0.0.1:0`, a client
that connects over the loopback, the accept with both names read back, bytes both
ways, the client's *half*-close read as end-of-stream while the other direction
still carries, a second sequential connection (which is what fails if the pool
never refills), and every refusal in the table above.

#### 4.3.3 The options — **PARTIAL** (two still record-only)

`setsockopt`/`getsockopt` are no longer a no-op: each socket's registry `Entry`
carries a `SockOpts`, and the common options round-trip with the correct ABI —
`SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_SNDBUF`, `SO_RCVBUF`, `TCP_NODELAY` (`SOL_TCP`)
as 4-byte ints, `SO_LINGER` as an 8-byte `struct linger` (in-out length honoured),
plus `SO_TYPE`/`SO_ERROR`. `SO_RCVBUF`/`SO_SNDBUF` report the stack's default
(64 KiB) until changed; unknown options are accepted and read back as 0.

**`SO_REUSEADDR` is applied at `bind`/`listen`, and the rule it gates is not the
one the man page suggests.** That is the finding, so the measurement comes
first. On this host's kernel, `127.0.0.1`, with the holder's own
`SO_REUSEADDR` and the new socket's varied independently, and with the holder in
each of the states a name can be in:

| the name is held by | holder's `SO_REUSEADDR` | new socket's | `bind` |
|---|---|---|---|
| a listening socket, open | either | either | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | no | **yes** | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | yes | no | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | yes | yes | **succeeds** |
| an established connection, listener closed | either | not both | `EADDRINUSE` |
| an established connection, listener closed | yes | yes | **succeeds** |
| a bound, never-connected socket | yes | yes | **succeeds** |

Three things fall out of that, and two of them are corrections to what this
option is usually described as doing:

* **The new socket's word is not enough.** The row where the holder did not set
  the option is `EADDRINUSE` *however loudly the new socket asks*. What makes a
  restart work is the other direction: Linux copies `SO_REUSEADDR` onto a socket
  when it is **accepted** (measured, `inet_csk_clone_lock`), so the listener's
  option is inherited by the connections that will later sit in `TIME-WAIT` on
  its port. A restart comes back because the *dead connections* had the option,
  not because the new socket does.
* **The exemption is not about `TIME-WAIT`.** A bound-but-unconnected socket and
  a live established connection both yield their name to a caller when both
  sides asked - so a port can be rebound while a connection is still open on it,
  and a listener can then *share* that name with it (measured: the new listener
  answers a fresh client while the old connection keeps its own). `TIME-WAIT` is
  simply the state this is usually noticed in.
* **A listener never yields.** Two live listeners on one name is what
  `SO_REUSEPORT` is for - it is not implemented here, and setting `SO_REUSEADDR`
  on *both* is not a substitute on Linux either (measured).

So `net::Stack::tcp_name_taken` is one clause over one table of holders: a
listening holder always conflicts; every other holder conflicts unless both it
and the caller set the option. The names it compares are the registry's recorded
ones plus, for a connection that recorded none (an accepted socket answers
`getsockname` from its own 4-tuple), the socket's own endpoint - and
`names_overlap` is the address half of the rule: the same port and either the
same address or the wildcard on one side, so `127.0.0.1:80` and `127.0.0.2:80`
coexist while either is refused beside `0.0.0.0:80` (measured). The holder's own
flag has to outlive its descriptor, so `Stack::teardown` carries it beside the
socket it named. The ephemeral pick behind a `bind(port 0)` (and behind
`listen`'s implicit bind) goes through the same predicate, which is what keeps an
implicit bind from landing on a `TIME-WAIT` port it would then have to refuse for
a name the caller never chose.

`t-tcp-listen` carries all of it - thirteen checks, every one of which is a line
of the table, plus "the restarted listener is *usable*" (a fresh client connects
to the taken-back name and its bytes arrive) and "a listener can share a name
with an open connection, and both serve". The whole test also runs on a Linux
host unchanged, which is the instrument: the same assertions that the guest must
satisfy are the ones a real kernel satisfies.

*Remaining (real effect, into smoltcp):* two. `SO_LINGER` is not honoured at
`close` - see 4.3.4 for why the reset it asks for is not something this stack can
send. `SO_RCVBUF`/`SO_SNDBUF` cannot be honoured per socket at all until
smoltcp's buffers stop being sized once at set construction. Three that used to
be on this list are not: `TCP_NODELAY` reaches the live socket
(`set_nagle_enabled`), `SO_KEEPALIVE` arms smoltcp's probe timer at Linux's
default interval, and `SO_REUSEADDR` is the rule above.

Two named gaps ride with the rule rather than the option. The predicate lives in
`net/mod.rs`, which is gated `target_os = "none"` (`src/lib.rs:48`), so it has no
host unit test - the guest program is the pin, the same situation `AF_PACKET`'s
parsers are in (§4.5). And **smoltcp's `TIME-WAIT` is ten seconds**
(`CLOSE_DELAY`) where Linux's is sixty (`TCP_TIMEWAIT_LEN`), so this machine
gives a name back six times sooner. The rule about *who* may take it is
identical; only the wait is short.

#### 4.3.4 `close(2)`, and the teardown the descriptor could not wait for — **DONE**

Measured rather than assumed: a plain `close(2)` reached `net::free_entry`, which
called `sockets.remove(handle)` on the spot, while smoltcp only puts the `FIN` out
from its poll loop after the socket is marked closing (`shutdown(2)` did call
`close()`; the last close did not). A closing socket is now marked and parked in
`Stack::teardown` - bounded, because a peer that stops answering may cost a slot
but not the set - and `reap_teardown` frees it after the poll that carries its
last segment, so the descriptor is gone when the caller asks and the wire still
gets its four packets. `t-tcp-close` + a host-side listener through slirp
(`10.0.2.2:4599` → host `127.0.0.1:4599`) shows the peer receiving the bytes
*and* end-of-stream instead of sitting until it gives up. (The socket to reap
lives in one of two sets now, so the list carries its home - see 4.3.1.)

That producer went missing once, with the consumer, the bounded list and this
paragraph all still in place - `TEARDOWN_MAX` sitting unread was the only clue,
and a suite that passes says nothing, because `t-tcp-close` reports `skip:` and
passes whenever no host listener is up. Re-measured with the listener running:
both of its connections (the orderly close and the `SO_LINGER{on,0}` one) reach
`GOT-FIN`. The lesson is that the probe is only an instrument when somebody
holds it: run it with the listener up before believing this section.

`SO_LINGER{l_onoff=1, l_linger=0}` - "reset, discard unread" - is the one close
shape this stack still cannot do, and it is left orderly on purpose rather than
faked: smoltcp 0.13's `abort()` only moves the socket to `Closed` and transmits
nothing (its resets are reply-driven, emitted when a packet *arrives* for a
closed socket), so calling it would make the close *silent*, which is worse than
the `FIN` the peer gets now. Honouring it needs an actual reset on egress - the
same missing primitive that makes closing a listener `FIN` its unaccepted queue
instead of resetting it (4.3.2).

*Remaining after those:* `SO_KEEPALIVE` probe packets; and a reset on egress for
`SO_LINGER{on, 0}`. `SO_RCVBUF`/`SO_SNDBUF` cannot be honoured per
socket as things stand: smoltcp sizes its buffers when the socket set is built,
so a real answer needs either a set rebuilt per requested size or a limit applied
in the mpos read/write queue - and until one of those exists the options should
say so rather than round-trip a number that changes nothing. `SO_REUSEADDR` used
to be on this line and is not any more: it is applied at `bind`/`listen` now
(§3.3.3), and the rebind allowance it gates turned out to be a rule about the
*holder's* option as much as the caller's.

### 4.4 Netlink Write Operations — **DONE**

The write side is real: `ip addr add/del`, `ip route add/del/replace`,
`ip link set eth0 up|down|mtu N|name X` are parsed (`ifaddrmsg`/`rtmsg`/
`ifinfomsg` + `rtattr` walks) and applied to the live stack - smoltcp's own
address and route tables, so a route added is a route the packet path
follows. `ip link set eth0 down` stops the interface from being polled:
frames neither leave nor arrive, and a `ping` through it loses 100% until
`up` restores it. Dumps read the same live state, so what was written is
what `ip addr`/`ip route`/`ip link` print - with the Linux shapes filled in
(`IFLA_BROADCAST`, `IFLA_MTU`, `IFLA_QDISC "noqueue"`, `IFLA_TXQLEN`,
`IFA_F_PERMANENT`, secondary-address labels `eth0:1`).

Errnos match Linux: `EEXIST` on a duplicate, `EADDRNOTAVAIL`/`ESRCH` on a
delete of something absent, `ENODEV` on an unknown index, `EAFNOSUPPORT`
off-family, `ENOBUFS` when the fixed route table (four entries) is full.
The error rides back in `nlmsgerr.error` **negated** - the kernel's own
convention; writing it positive made busybox print the message for an errno
no table knows ("No error information") instead of "File exists".

Deliberate v1 divergences (each asserted in `t-netlink`): an exact-
duplicate `ip addr add` succeeds instead of `EEXIST` (the boot's own
idempotent re-configuration stays quiet); a route with no gateway is
`EINVAL` (smoltcp delivers the interface's own subnets implicitly, so an
on-link prefix has no second expression); `RTM_DELLINK` and `ip link add`
are `EOPNOTSUPP` (no dynamic interfaces); `lo`'s address and MTU are fixed
(flags and read-back work, the stack has no loopback device behind them).
The `ip link set` flags path also works through the **ioctls** busybox
reaches it by - `SIOCGIFFLAGS`/`SIOCSIFFLAGS`/`SIOCSIFMTU`/`SIOCGIFINDEX`
(the one musl's `if_nametoindex` needs), `SIOCGIFMTU`, `SIOCGIFTXQLEN`/
`SIOCGIFHWADDR` (whose numbers 0x8942/0x8927 had been swapped, answering
the MAC for a txqueue-length request). Two helper gaps this opened: AF_UNIX
`SOCK_DGRAM` had to *open* (musl's `if_nametoindex` asks for it as the bare
handle for its own `SIOCGIFINDEX`; the datagram transfer itself has since
landed - see §4.1); and `IFLA_TXQLEN`/`IFLA_MTU` are native-order words, the
endianness addresses are not.

**Changes**:
- `RTM_NEWADDR`/`RTM_DELADDR`: `ip addr add`/`del`, applied to the stack
- `RTM_NEWROUTE`/`RTM_DELROUTE`: `ip route add`/`del`/`replace`
- `RTM_NEWLINK`/`RTM_SETLINK`: link up/down (a real poll gate), MTU, name
- `ip addr`/`ip route`/`ip link` dumps read the live tables, not constants
- The `ifreq` ioctl set `ip link set` and `if_nametoindex` ride on

**The other netlink protocol has no write side. That was the last blocker on the
udevd path, and it is written now.** `NETLINK_ROUTE` was already applied to the
live stack; `NETLINK_KOBJECT_UEVENT` was a read-only channel the kernel
publishes into, and udev needs the userspace half of it: the device manager
hands an event to one of its workers with `udev_monitor_send_device(monitor,
worker->monitor, dev)` - a `sendmsg(2)` on a uevent socket carrying a 12-byte
`struct sockaddr_nl` - and publishes the processed device to the group every
listener joined. The send path here read that address as a `sockaddr_in`
(`len < SOCKADDR_IN_LEN`) and answered `EINVAL`, so the handoff failed and udevd
killed the worker that refused it: `worker [11] did not accept message -22
(Invalid argument), kill it`, with no device tagged while it happened.

What the family grew, and it is measured rather than recalled - `libudev-monitor.c`
for the shape udev uses, this host's kernel for the semantics:

| Question | Measured answer |
|---|---|
| where a port id comes from | the process's pid when it is free, a hash-derived id when it is not (libudev's own comment: "usually, but not necessarily the pid"); an unbound socket is given one by its first send |
| binding a port id another socket holds | `EADDRINUSE`; binding twice (even to add a group) is `EINVAL` |
| a unicast | delivered to the port id; the receiver's `msg_name` is the *sender's* `nl_pid` with `nl_groups == 0`; an unknown or closed port id is `ECONNREFUSED`; the sender never receives its own |
| a group send | goes to the **lowest** group in the mask (Linux computes `ffs` of it: `nl_groups = 6` reaches group 2 and not group 4); the receiver is told the *destination group* as `msg_name.nl_groups`; the sender is not told; an empty group is not an error - the byte count comes back anyway, which is what libudev's "a multicast `ECONNREFUSED` would be expected" tolerates |
| a send with no port id and no group | accepted, and addressed to the kernel |
| a 12-byte `sockaddr_nl` | the shape that works; a short address or a family that is not `AF_NETLINK` is `EINVAL` (not `EAFNOSUPPORT`) |
| credentials | with `SO_PASSCRED` every received message carries `SCM_CREDENTIALS` with the sender's real ids; without it there is no control data at all |
| `getsockname` before bind | the all-zero address, not an error |

The policy is `uevent`'s and is host-tested - nine tests over port ids, unicast,
groups (including the `ffs` artifact), the full-queue `ENOBUFS` a handoff must
hear rather than lose, and the autobind - while `t-uevent` pins the shape
end-to-end in the guest: two sockets, `SO_PASSCRED`, a `sendmsg` to the port id
`getsockname` reported, the payload, the source address and the credentials, and
the three refusals. **And the boot says the same:** `/dev/input/event0` carries
`ID_INPUT=1`, `ID_INPUT_KEY=1` and `ID_INPUT_KEYBOARD=1`, and
`/run/udev/data/c13:64` holds them - which is the tag libinput classifies a
keyboard by, and the end of the chain that began with a worker's zero-byte
message.

---

### 4.5 `AF_PACKET` — the link layer as a socket — **PARTIAL**, two gaps named

The kernel had been the last word on the machine's address with no door for
userland to overrule it: `10.0.2.15` was a boot-time constant and `EAFNOSUPPORT`
was the answer to every client that wanted to negotiate one. `src/net/packet.rs`
opens that door in the shape Linux spells: `socket(AF_PACKET, SOCK_DGRAM,
htons(proto))`, a tap hung on the NIC seam (`NicDevice::receive` hands every
frame to the sockets *before* the stack decides whose business it is — which is
the point: a BOOTP reply for an addressless machine is nobody's business in the
IP sense), `sendto` with a `sockaddr_ll` whose Ethernet header this kernel
builds from the device's own MAC, per-socket bounded rings with the oldest-frame
drop, and `bind`/`getsockname`/`recvfrom` answering through the same
`sockaddr_ll` (`linux/if_packet.h`'s real offsets: 20 bytes, address at 12).
`SOL_PACKET` membership requests succeed because the tap already sees every
frame of the socket's type; every other level is `ENOPROTOOPT`, Linux's own
answer.

The named gaps, each with what it waits on:

* `SOCK_RAW` — frames *with* their header on both sides. It is `EOPNOTSUPP`
  because a caller that writes its own header writes a source MAC this kernel
  cannot vouch for; the honest shape needs the header passed through untouched
  and the length checked, not invented.
* The boot still has no client. The *mechanism* is what `t-dhcp` pins — a
  hand-built DHCPDISCOVER to the broadcast and slirp's BOOTREPLY read back with
  its transaction id — but wiring `busybox udhcpc -s` into OpenRC (and giving
  the selftest initramfs one too, or its network tests ride the static default)
  is the remaining move before the static bring-up can be retired. Until it
  happens the static 10.0.2.15 is a boot *default* over a live interface, not
  a decision the kernel reserves: `RTM_NEWADDR` and now `AF_PACKET` both
  overrule it.
* The parsers over `sockaddr_ll` and the Ethernet frame have no host-side pin.
  `AGENTS.md` §3 asks every new parser for a fuzz target, and `net/packet.rs`
  has none — not from oversight but from the §6 trap that gates the whole `net`
  module on `target_os = "none"` (`src/lib.rs:48`), so neither a host unit test
  nor a `fuzz/` target can even reach it, and `net/socket.rs`'s annotated test
  module stands as the standing proof of that (V-11). The only pin today is
  `t-dhcp` on the machine. The gap closes when the wire-format halves —
  `sockaddr_ll` decode, header parse, ring accounting — move to an ungated pure
  module the way `evdev` and `uinput` already do, and gain a fuzz target there.
  Until then this line is the recorded reason, not a silent exemption.

---

## Phase 5: Memory Management Enhancements (Medium Priority)

### 5.1 Shared Anonymous mmap — **DONE**

`mmap(MAP_SHARED | MAP_ANONYMOUS)` is a *shared object*, not a private range:
the mapping is a frame-backed VMA over a fresh unnamed `Memfd` - the same frame
store `memfd_create` hands out, with real pages grown on demand, refcounted and
released when the last mapping of it goes. Nobody holds a descriptor for it and
nobody needs to: the mapping's VMA is its only name, and `fork` copies that.

That is what makes the aliasing survive a plain `fork`. Both processes keep the
same object, and the one rule that would have broken it is stated where the walk
is: **`fork` does not mark a frame-backed range's pages copy-on-write**, and it
does not harden them in the parent either. A copy-on-write flag there would turn
the first store on either side into a private frame, which is the sharing the
mechanism exists to keep. The `memfd` and dma-buf mappings get the same rule, so
"aliased frames don't cross a plain `fork`" is no longer a documented divergence
anywhere. `t-mmap-shared-fork` pins both directions - the child's write read by
the parent after a `wait4`, and the parent's write after the `fork` read by a
child the pipe held back - because only the second fails for a mapping that was
copied before the fork.

### 5.2 madvise — **DONE**

`madvise` answers `EINVAL` for a range whose start is not page-aligned (Linux's
rule) and recognises the `MADV_*` advisories, of which one now does something:
`MADV_DONTNEED` walks the range and drops the pages a VMA covers, leaving the
mapping in place so the next access is an ordinary fault. What that fault
produces is what the backing decides, which is Linux's rule in Linux's words -
"repopulating the memory contents from ... the underlying mapped file ... or
zero-fill-on-demand pages for anonymous private mappings". A private anonymous
page comes back freshly zeroed (its frame is gone, so a new one is allocated);
a page of a frame-backed range comes back from the object behind it, which still
holds it. `t-madvise` pins both, and `t-mmap-demand` shows the release and the
refill across a 64 MiB arena.

The other advisories stay the recognised no-ops they are, and two of them are
deliberate rather than unfinished:

* **`MADV_FREE`** keeps its no-op. Its contract is that the pages *may* be
  reclaimed but their contents are still there until memory pressure arrives,
  and this kernel has no pressure model - returning the frames immediately would
  be wrong rather than merely different.
* **`MADV_DONTNEED` on a shared mapping** does the per-mapping half of what Linux
  does. Linux drops the pages for everyone mapped to the object, which needs the
  object's reverse map of mappings (`i_mmap`) to find them; this kernel's object
  has no such index, and dropping the frames while another mapping still points
  at them would be a use-after-free rather than a saving. So the pages here are
  dropped for the caller's mapping and the object keeps its own copy, which the
  next fault finds - the "repopulate from the underlying mapped file" half of
  the promise, which is the half that is observable without the index. Nothing
  beyond that is claimed; `t-madvise` pins the single-process case and
  `t-mmap-shared-fork` the fork case.

### 5.3 Process_vm_readv / Process_vm_writev — **DONE**

`process_vm_readv(310)` and `process_vm_writev(311)` are implemented in
`proc.rs` (`handle_process_vm_readv`/`_writev`), walking two iovec arrays in
step to move bytes between the caller and a remote task's address space.
Used by debuggers and checkpoint/restore tools.

---

## Phase 6: Process Management Enhancements (Low-Medium Priority)

### 6.1 ptrace (Debugger Support) — **DONE**

`PTRACE_DETACH` used to return 0 and leave the tracee stopped. The cause was not
the stop it was in but the one it had not reached: `PTRACE_ATTACH` injects a
`SIGSTOP`, and while the tracee is still inside its current syscall that signal
is merely *pending*. A detach that only ends an in-progress stop therefore hands
back a process which stops itself on the way out of that syscall - with no tracer
left, and nothing on the machine but a `SIGCONT` that could end it. The test
demonstrated it by hanging the machine for ten minutes with every earlier check
printing `ok:`, until a bounded `WNOHANG` poll replaced the blocking `waitpid`.

The three cases are Linux's, and were measured against it before being matched
(`t-ptrace-attach` pins all three):

| Sequence | Linux | Now |
|---|---|---|
| `attach`, `detach(0)`, never `wait`ing | target runs on | the pending stop signal is dropped by the detach |
| `attach`, report the stop, `detach(0)` | target runs on | the stop parked with `delivering` set is ended |
| `kill -STOP`, `attach`, `detach(0)` - or `detach(SIGCONT)` | stays stopped | the stop is left alone, and no wake is sent |

The third row is the one that decides the shape of the code: the signal a detach
injects goes to the *tracee*, not to its stop, so naming `SIGCONT` does not end a
group stop the user asked for - and a `wait` on that child stays empty. Which is
also why the wake is now conditional everywhere: a task parked in a group stop is
held there by the scheduler's sleep, so a detach that woke it would release the
very stop it had just decided to leave. The companion rule is in the signal
sender - a stop signal arriving for a task already in a group stop has nothing to
say to it, and must not wake it either.

Dropping the `SIGSTOP` *pending* bit in the third case is not an invention: after
`kill -STOP` + attach + `detach(0)`, one outside `kill -CONT` returns Linux's
process to running rather than re-stopping it, which is only possible if the
attach's own signal did not outlive the detach.

What is *not* modelled: Linux reports a `PTRACE_EVENT_STOP` to a tracer that
attaches to an already-stopped task, so that the tracer's first `wait` returns
before anything is detached. Here that stop stays the parent's news, which is why
the test attaches without waiting - the same thing `strace -p` on a suspended job
would see.

`handle_ptrace` (proc.rs) implements the debugger set: `TRACEME`, `ATTACH`,
`DETACH`, `CONT`, `SYSCALL`, `SINGLESTEP`, `KILL`, `INTERRUPT`,
`PEEK`/`POKE` TEXT/DATA/USER, `GET`/`SETREGS`, `GET`/`SETREGSET`,
`GET`/`SETFPREGS`, `GET`/`SETSIGINFO`, `SETOPTIONS`, `GETEVENTMSG`, wired to the
task's `Tracing` state and the `waitid` stop reports. Required for gdb/strace.

| Syscall | Nr | Description |
|---------|-----|-------------|
| `ptrace` | 101 | Process trace |

**Required for**: gdb, strace, ltrace

**Implementation plan**:
- `PTRACE_TRACEME`: child sets `traced = true`, sends `SIGSTOP` to self
- `PTRACE_ATTACH`/`PTRACE_SEIZE`: attach to target process
- `PTRACE_PEEKDATA`/`PTRACE_POKEDATA`: read/write target memory
- `PTRACE_GETREGS`/`PTRACE_SETREGS`: read/write target registers
- `PTRACE_SINGLESTEP`: set TF (trap flag) in RFLAGS, deliver `SIGTRAP` on next instruction
- `PTRACE_CONT`: resume execution, optionally delivering a signal
- On any stop, target enters `TASK_TRACED` state and parent is notified via `waitid`

### 6.2 Process Resource Limits (Real) — **DONE**

`getrlimit`/`setrlimit`/`prlimit64` now operate on a real per-process table
rather than returning constants:
- A `[Rlimit; RLIM_NLIMITS]` table is stored on `Task`, seeded by
  `default_rlimits()` (8 MiB stack, 1024/1M `NOFILE`, 4096 `NPROC`, infinity
  elsewhere) and **inherited whole across fork and clone** (exec leaves it).
- `setrlimit`'s rule is real: `rlimit_admit` rejects soft-above-hard with
  `EINVAL` and an unprivileged raise of the hard ceiling with `EPERM` (root may);
  the accepted pair is stored. `RLIM_INFINITY` is `u64::MAX`.
- `getrlimit`/`prlimit64(pid 0)` read the caller's own table; `prlimit64` on
  another live pid can read it (else `ESRCH`) but not write it (`EPERM`).
- `/proc/<pid>/limits` renders the table in the `ulimit` layout.
- The pure rule + defaults are host-tested (`setrlimit_admits_by_privilege`).

*Both remaining resources are now enforced:*
- **`RLIMIT_NOFILE`**: the descriptor table carries the soft ceiling (seeded 1024,
  synced from `setrlimit(7)` - 7 is `asm-generic/resource.h`'s number, and this
  said 5 until §15.13, which is `RLIMIT_RSS`'s; copied on fork);
  `install`/`install_at_least`
  refuse a descriptor at or above it with `EMFILE` instead of growing unbounded.
  Every fd-creating syscall propagates it, and `pipe`/`socketpair` return the
  first descriptor if the second is refused. Host-tested by
  `the_descriptor_table_honours_its_ceiling`.
- **`RLIMIT_STACK`**: `resolve_stack_growth` reads the faulting task's soft limit
  as the stack floor (SIGSEGV beyond it; capped at the reserved region for a
  raised/infinite one).

### 6.3 Namespace Support — **UTS BUILT; THE OTHER THREE DESIGNED**

Two calls (`unshare`, `setns`), eight kinds of namespace, and one of each kind
until this pass. Most of this section is still the *decision record* rather than
a to-do list, because the decisions are what §6.3 kept being deferred for: which
of the eight this kernel can isolate observably, in what order they are worth
building, and what each one costs the layers that would have to grow a
per-namespace answer. **The first of the four decisions has since been built** -
`CLONE_NEWUTS`, in `src/uts.rs` and the `Task` field that holds it - so the
table below reads "build first" as done and the rest as designed. "Step 1,
landed", at the end of this section, is what the step actually cost, what it
measured, and what it deliberately did not do.

**The rule that decides the list is §4's rule 4** - refuse rather than
approximate. A namespace that isolates nothing is not a namespace, it is a
promise, and `EOPNOTSUPP` is what a promise is worth: `unshare(CLONE_NEWIPC)` on
a machine with no SysV IPC objects and no POSIX message queues would hand back a
"new IPC namespace" whose contents are exactly the old one's, and a caller that
believes it stops looking. So each of the eight is decided by *what a program
could observe*, and only the four with something observable behind them are on
the build list.

| flag | what it must observably isolate | verdict |
|---|---|---|
| `CLONE_NEWUTS` | `uname`'s `nodename`/`domainname`: `sethostname` in one namespace must not be visible in another, through **all three** doors | **built** - "Step 1, landed" below |
| `CLONE_NEWNS` | the mount table *and the root*: a `mount` in one namespace invisible in another, and `chroot`/`pivot_root` giving a container its own `/` | **build second** |
| `CLONE_NEWPID` | the numbers a process *sees*: a child of the new namespace is pid 1 to itself, and `/proc` is numbered per reader | **build third - the expensive one** |
| `CLONE_NEWNET` | interfaces, addresses, routes and the socket table: a socket bound in one namespace is not reachable from another | **build fourth** |
| `CLONE_NEWUSER` | uid/gid mapping, and the capability set *within* the namespace | **refused** until there is a mapping to store |
| `CLONE_NEWIPC` | SysV IPC objects and POSIX message queues | **refused**: this kernel has neither, so it isolates nothing |
| `CLONE_NEWCGROUP` | the cgroup tree a process sees | **refused**: there is no cgroup tree here to see |
| `CLONE_NEWTIME` | `CLOCK_MONOTONIC`/`BOOTTIME` offsets per namespace | **refused**: one tick clock, and no per-namespace offset to store |

**UTS: the state is two strings, and the cost is that they have three doors.**
`nodename` and `domainname` are one process-wide pair today (`vfs::procfs`'s
`HOSTNAME`/`DOMAINNAME` statics), read by `sethostname`/`setdomainname`,
`uname`'s `nodename`, and `/proc/sys/kernel/{hostname,domainname}` - kept in one
home precisely so the three cannot disagree. A namespace makes that home *the
caller's*: a `Uts { nodename, domainname }` the task owns, with `fork`/`clone`
inheriting the reference (a child shares its parent's namespace until it
unshares) and `unshare` **copying** it, which is Linux's `copy_utsname` - the
new namespace starts with the old one's contents, so a `sethostname` afterwards
is invisible to the parent. This is the one whose whole cost is the plumbing;
it is first because it is small, and because it is the shape every other
namespace repeats.

**MNT: the one that costs the VFS — the root half has landed, the table has not.**
The per-process *root* — the prerequisite this whole item was deferred on — is
done. A task carries its own root (`sched::Task.root`, inherited on `fork`/`clone`,
set by `chroot(2)` through `sched::set_root`), and the one absolute-path walk
(`Vfs::resolve_walk`) begins, clamps `..`, and restarts absolute symlinks at *the
caller's* root rather than the machine's. A `chroot` now re-homes the calling
process and its future children — `t-chroot` has a child chroot, confirm its jail,
and prove the parent's view of the same `/tmp` is untouched, the `t-ns-uts` shape
for a per-task view. `set_global_root` is gone; what the boot's `switch_root` used
it for it now gets by inheritance (pid 1 is every process's ancestor). **The mount
table is still a single global**, and `pivot_root(2)`/`MS_MOVE` act on it
machine-wide — which *is* Linux's answer with one mount namespace, and stays
named until the next step. What is left: move the table onto the task too (an
`Arc<MountNamespace>` holding the entries, the root having already moved), have
resolution ask *the caller's* namespace for its mounts, and let
`unshare(CLONE_NEWNS)` clone the tree the way `copy_mnt_ns` does — a `mount` in the
child then invisible to the parent, and `pivot_root` per-namespace, which is what
gives a container its own `/`. Two things stay named refusals rather than silent half-answers: **mount
propagation** (`MS_SHARED`/`MS_PRIVATE`/`MS_SLAVE` - a mount event crossing
namespaces is a second mechanism, and ignoring the flag would silently change
what "unshare" meant), and **`/proc/mounts`** having to render the reader's
namespace rather than the machine's.

**PID: expensive because of the vocabulary, not the feature.** A pid here *is*
the task slot - `sched::policy::TaskId(pub usize)`, the number `getpid` returns,
the number `/proc/<pid>` is named after - and the slot is **reused**. A pid
namespace needs the thing Linux built for exactly this and this kernel does not
have: a `struct pid` whose lifetime is its own, a per-namespace number mapped to
it, and `/proc` rendering the numbers the *reader's* namespace gives. It is also
where the reusable-slot problem §7.1 already records for `VT_SETMODE`'s promise
("Linux holds a refcounted `struct pid`, so a promise can never be inherited by
an unrelated process later given the same number") would have to be solved once
and properly, rather than twice. The semantics to hold, none of them optional:
`unshare(CLONE_NEWPID)` renumbers the caller's *children* and not the caller
(the process that asked keeps its own pid - it is the first child that is 1);
`setns` into a pid namespace has the same "children only" effect; and `/proc`
must be per-namespace, which means the pid directory for a process is a
different name in two namespaces that both can see it.

**NET: one `Stack`, and the device is the reason the list is short.** `net::NET`
is one interface pair (`eth0` + `lo`), one address table, one route table and one
socket registry. The record is already shaped like something a second copy would
work (`Stack` owns its fabrics, and the two-fabric split landed for the
loopback), so a net namespace is a second instance with the socket registry
keyed to it - but the *wire* is one virtio NIC with one MAC and one queue, and a
namespace cannot take it from the machine. So the first target is a namespace
whose `lo` is its own and which has no wire, which is what a container's private
network is, and what makes a socket in it unreachable from the parent. Moving a
physical device between namespaces is a later question and is refused until
there is a place to move it to.

**`setns`, and how a namespace is named.** Linux reaches a namespace through a
descriptor opened on `/proc/<pid>/ns/<kind>`; no namespace here can be named
that way, so the call answers `EINVAL` for an open descriptor and `EBADF` for a
closed number - all it can learn. The design follows from the table: each
namespace object becomes an inode whose open descriptor *is* the namespace (so
two descriptors on one `/proc/<pid>/ns/uts` name one object, with one `st_ino`),
`setns` checks the descriptor's type against `nstype` (a mismatch is `EINVAL`,
Linux's answer), and `NS_GET_USERNS`/`NS_GET_PARENT` are out of scope until
there is a user namespace for them to point at.

**Two decisions about the call itself, rather than about a namespace.**

* A flags word naming a namespace this kernel does not serve is refused **whole**
  - `EOPNOTSUPP`, and nothing is unshared. Linux's `ksys_unshare` is not atomic:
  by the time a later flag fails, an earlier one's unshare has already happened.
  This is a deliberate divergence, and it is the direction §4.4 prefers - a
  caller that asked for two namespaces and got an error learns that *neither*
  happened, instead of being left halfway with nothing saying so. It also means
  the served set can grow without a caller ever observing a half-built word.
* The flag vocabulary stays Linux's, which is now true in the handler and pinned
  by `t-posix` (see below). When a namespace is served, its own flag stops
  answering `EOPNOTSUPP` and starts answering Linux's `EPERM` for an
  unprivileged caller - the same `CAP_SYS_ADMIN` gate Linux puts on the seven
  privileged kinds.

**Verification, one namespace at a time, before the next one starts.** Each of
the four lands with its own `t-ns-<kind>` program, and the shape is the same for
all four - *the child's view differs and the parent's does not*, because the
parent's half is the assertion a "namespace" that is only a shared global
fails:

| program | the unshared side | the parent's side |
|---|---|---|
| `t-ns-uts` | a child that unshares reports `inside`/`inside.test` through `uname` **and** both sysctls, and a task that unshares keeps the names it had | the parent that did not unshare still reports the old names, its own three doors still agree, and a child forked *before* a refused mixed word keeps sharing the parent's namespace |
| `t-ns-mnt` | `mount` a tmpfs at a directory, then read it back | the parent's `/proc/mounts` does not carry it, and the directory is empty there |
| `t-ns-pid` | the first child is pid 1 to itself (`getpid`), and `/proc/1` names it | the real number is unchanged, and `/proc/<that number>` still names it |
| `t-ns-net` | a socket bound to a port is reachable from inside | the same port is free in the parent's namespace, and a connect to it is refused |

Each also pins its refusals: an unprivileged caller (`setresuid`-dropped) gets
`EPERM` for a served namespace, an unserved one still answers `EOPNOTSUPP`, and
a flags word mixing the two is refused whole.

**Order, and why it is that one.** UTS, then MNT, then PID, then NET: each step
is independently verifiable (its `t-ns-*` above), and none of them needs the next
one to be true. UTS is first because it is the cheapest complete example of the
shape - a per-task object, an inheritance rule, a copy on `unshare`, three doors
- and because it exercises the "refused whole" rule with nothing at stake. MNT
is second because it is the one containers actually need and because its
prerequisite (a per-process root) is a divergence worth fixing on its own. PID
is third because it is the most expensive and the least optional for anything
that runs an init inside; NET fourth because a namespace with `lo` alone is
already useful and the wire is a separate question. USER, IPC, CGROUP and TIME
stay refused, each with the reason in the table above, and each is worth
revisiting only when the thing it would isolate exists (§8.2's seccomp and §7.4's
compositor work are both more urgent).

**Step 1, landed: UTS.** The shape the paragraph above predicted was the shape it
took, which is worth recording because it is the claim this section was written
to test: a `Uts` object holding the two names (`src/uts.rs`, not target-gated,
so the copy rule is host-tested), a `Task::uts: Arc<Uts>` field, `fork`/`clone`/
`spawn_thread`/`try_spawn_user` inheriting the `Arc` by reference, and
`unshare(CLONE_NEWUTS)` replacing the caller's with `Uts::copied()` - Linux's
`copy_utsname`, so the new namespace *starts* from the old one's names and only
then diverges.

**The three doors were the whole of the cost, and one of them was already
broken.** `sethostname`/`setdomainname` wrote a pair of process-wide `String`s
in `vfs::procfs`; `uname`'s `nodename` read them; and `/proc/sys/kernel/hostname`
read them. Making the home *the caller's* was the edit. What the edit found is
that the domain half of the pair had never been wired: `uname` reported the
literal `(none)` and `/proc/sys/kernel/domainname` hardcoded the same string on
its read *and dropped every write*, so `setdomainname(2)` had no observable
effect through any door at all. Both now read the caller's namespace, which is
what makes the three agree about *which* namespace they are answering for - the
property `t-ns-uts` holds with one `Doors::agree()`.

**The capability gate is on all four doors, measured rather than recalled.**
Run as a non-root caller on this host's kernel (7.2.7): `sethostname(NULL, 100)`,
`sethostname(name, -1)` and `sethostname(name, 65)` all answer **`EPERM`** - so
the `CAP_SYS_ADMIN` check comes *before* the length check and before the pointer
check, not beside them - and the same holds for `setdomainname`, for
`unshare(CLONE_NEWUTS)` and for every `CLONE_NEW*` bit. The kernel's test for the
capability is the effective-uid one the entropy ioctls already use. The sysctl
files get their own gate for a different reason: on Linux the *file's mode*
(0644) is what refuses a non-root writer, and procfs here keeps no per-node
modes, so the capability is checked in the write policy and answers `EACCES` -
the errno Linux's own door gives. That the mode is not really there to check is a
named divergence below.

Two measurements decided the flag word's shape, and both are Linux's:

| as a non-root caller | Linux 7.2.7 | this kernel |
|---|---|---|
| `unshare(CLONE_NEWUTS)` | `EPERM` | `EPERM` |
| `unshare(CLONE_NEWUTS\|CLONE_NEWNS)` | `EPERM` | `EOPNOTSUPP`, **nothing unshared** |
| `unshare(CLONE_NEWNS)` | `EPERM` | `EOPNOTSUPP` |
| `unshare(CLONE_NEWUTS\|0x1000)` | `EINVAL` | `EINVAL` |
| `unshare(CLONE_THREAD)` | `0` | `0` |

The two rows that are not Linux's are the deliberate ones: a namespace this
kernel does not *serve* is `EOPNOTSUPP` rather than `EPERM`, because `EPERM`
would send the caller looking for a capability that does not help; and the
unserved bit is decided before any grant, so a mixed word unshares nothing.
Linux's `ksys_unshare` does not work that way - it walks the word and a later
failure leaves an earlier unshare done - and the atomicity is what `t-ns-uts`'s
witness child pins: it is forked *before* the refused call, so a rename the
parent makes afterwards is a rename it sees only if the two are still one
namespace. The whole verdict is a pure function over the word and the caller's
privilege (`abi::unshare_verdict`), which is where the four host tests argue with
it.

**Named gaps, each with what it waits on.**

* **`clone(CLONE_NEWUTS)`** - the *child* getting the copy - is still `EINVAL`:
  `validate_clone_flags` refuses namespace bits wholesale, and it says so. Serving
  it is a separate decision (where in `copy_process`'s order the copy happens,
  and what `CLONE_NEWUTS|CLONE_THREAD` means for a group that shares an address
  space) and it wants its own measurement, so the `EINVAL` stands as "this call
  cannot do that here" rather than as a half-built grant.
* **The six thread-group flags still answer `0` for a caller that has siblings.**
  Linux refuses with `EINVAL` there, because the descriptor table or the handler
  table really is shared. This kernel answers `0` for every caller, which is
  Linux's answer only for a single-threaded one - and since threads exist now
  (`t-clone`), the old comment's premise ("a process is its own only thread") is
  no longer universally true. Splitting a resource off from one's siblings is
  what is not modelled.
* **`setns` still cannot name a namespace**, and that is now the interesting
  half: `/proc/<pid>/ns/<kind>` does not exist, so there is no descriptor for the
  call to check a type against, and it answers `EINVAL`/`EBADF` - all it can
  learn. The design above (each namespace object an inode whose open descriptor
  *is* the namespace) is unchanged and still the plan for the second step.
* **`/proc/sys/kernel/{hostname,domainname}` are not mode-policed.** procfs keeps
  no per-node modes, so the capability check in the write policy is what refuses
  a non-root writer, and it answers `EACCES` - the right errno by the right
  argument for the wrong mechanism.

**And the flag table under all of this was wrong until this pass.** A caller does
not send a flag *name*; it sends the number `unshare(2)` documents, and this read
`CLONE_NEWNS` as `0x1_0000` - which is `CLONE_THREAD` - and `CLONE_NEWTIME` as
`0x8_0000`, which is `CLONE_SETTLS` and is not a namespace at all. A program
asking to unshare the mount namespace was told `EINVAL`, as though it had sent
something undefined, while a program passing `CLONE_THREAD` was told
`EOPNOTSUPP`, as though it had asked for a mount namespace. Nothing caught it
because the pin in `t-posix` had been written from the same misremembering, so
the test agreed with the bug - the case rule 1's carve-out is for, and the test
moved with the measurement.

The measurements, one call per child on this host's kernel, and the table the
implementation now holds:

| flags | Linux | why |
|---|---|---|
| `0x2_0000` (`CLONE_NEWNS`) | `EPERM` | a namespace flag, refused for want of `CAP_SYS_ADMIN` |
| `0x80` (`CLONE_NEWTIME`) | `EPERM` | the same; Linux puts this one in the *low* byte |
| `0x8_0000` (`CLONE_SETTLS`) | `EINVAL` | not a namespace and not something `unshare` can do |
| `CLONE_VM`/`FS`/`FILES`/`SIGHAND`/`THREAD`/`SYSVSEM` | `0` | thread-group splits, and a single-threaded caller has nothing to split |
| `CLONE_PIDFD`, `CLONE_IO` | `EINVAL` | real flags, meaningless to `unshare` |
| `CLONE_PIDFD\|CLONE_NEWNS` | `EINVAL` | an invalid bit outranks a namespace beside it |

So the eight namespaces are `0x80`, `0x2_0000`, `0x0200_0000` … `0x4000_0000`;
the six thread-group flags are a genuine no-op for a caller with no sibling to
split from, which is what the measurement above was taken on and what Linux
answers for a single-threaded process; and `EINVAL` is decided before the
namespace refusal, which is Linux's order. All of it is pinned by `t-posix`.

Two of those rows have moved since, and one of them is now an overstatement.
`CLONE_NEWUTS` no longer answers `EOPNOTSUPP` - it is the one namespace this
kernel serves, and the row above is the measurement the *unserved* seven still
give ("Step 1, landed" earlier in this section). And "a process is its own only
thread" stopped being true when threads landed (§6.4): the six thread-group flags
are a no-op for the *unshared* caller the table was measured on, and for a caller
that does have siblings Linux answers `EINVAL` where this kernel still answers
`0`. That is a named gap in the "Step 1, landed" list rather than a claim.

### 6.4 Additional Process Syscalls — **DONE**

All wired to real handlers in `posix.rs`/`proc.rs`: `getpgrp`, `setreuid`,
`setregid`, `getresuid`, `getresgid`, `capget`, `capset`.

| Syscall | Nr | Description |
|---------|-----|-------------|
| `getpgrp` | 111 | Implemented, verify correctness |
| `setreuid` | 113 | Set real/effective UID |
| `setregid` | 114 | Set real/effective GID |
| `getresuid` | 118 | Get real/effective/saved UID |
| `getresgid` | 119 | Get real/effective/saved GID |
| `capget` | 125 | Get capability state |
| `capset` | 126 | Set capability state |

**Threads are real, and this section said otherwise for longer than it should
have.** The paragraph here used to read "one request in this family is
deliberately not answered, and it is the largest hole left in the process ABI:
`clone(CLONE_THREAD)` — thread creation — answers `ENOSYS`". That had stopped
being true at commit `58c6947`, which landed a thread group (`Mm`: the frames
behind the group's mappings, how many threads run on them, the paging root a
thread switches to, and where a leaving thread says so), `spawn_thread` reached
from `sched::clone`, `CLONE_CHILD_CLEARTID` writing zero and waking the futex
there - which is what `pthread_join` sleeps on - `exit_group(2)` ending every
thread of a group while `exit(2)` ends only the caller, and `getpid` answering
the group id while `gettid` answers the task's own. `t-clone` pins it and sits in
the default sweep: a thread's write is visible to its creator, which is the whole
difference between a thread and a fork. The stale paragraph was contradicted by a
green test the gate ran every time, which is the lesson worth keeping from it.

What genuinely remains is narrower, and each item is named rather than implied:

* **`CLONE_SIGHAND` is accepted and the handler table is still copied.** Linux
  shares it under that flag; here a handler installed by one thread is not
  installed for its siblings. Nothing in this system's userland does that yet, so
  a *copy* is the honest version of "not shared" - but it is a divergence and not
  a completion.
* **`CLONE_VM` without `CLONE_THREAD` is answered with a copy.** That is
  `posix_spawn`'s shape met precisely (such a child only ever reads the caller's
  memory before `execve`), and the doc comment at `sched::clone` says why a copy
  beats a share there - but it is not the shared address space the flag names.
* **The rest of the thread ABI** is wired where a caller needs it: the tid
  writes, the TLS base, the signal mask inherited across `clone`/`fork`,
  `get_robust_list`, and the per-thread `/proc/<pid>/task` listing
  (`thread_snapshots`).

---

## Phase 7: Device Drivers — **7.1, 7.2 and 7.3 are done; §7.4 has two steps left**

The device work is finished: display, input, and now storage. §7.4 is a different
kind of thing - it is *analysis* of the compositors beyond weston, written from
their sources and the UAPI rather than measured here, and two of its six ordered
steps remain (§7.4's own list). One item under 7.1 is also still open and is
written down there rather than here: weston's desktop-shell does not paint its
pointer, and the measurements say so far that it is not this kernel's doing.

Two things this phase measured are worth carrying forward, because both were
*surprises that booting found rather than reading*: the cursor plane's composition
is the one piece of arithmetic on the presentation path and lives in
`crates/gpu/src/cursor.rs` with host tests, and the sync object family's semantics
disagree with the upstream sources in the one place that decides its design, which
is why they were measured on a real card before any of it was written (§7.4). The
third, from §7.3, is the same shape one layer down: a driver whose every line is a
register access has no host test to fail, so its two wrong decisions - `PxSIG` is
not the presence question, and an unpartitioned disk still has a name - were found
only by booting it.

**Gate for the phase, quoted from its own summary**: host `618 passed`,
`mpos-xtask 36 passed`, guest `=== SUMMARY pass=132 fail=0 (132 built)`, QEMU exit
33, `cargo fmt --check` clean and `cargo clippy` clean on the bare-metal target and
on `mpos-xtask`.

### 7.1 Display: virtio-gpu (framebuffer + DRM/KMS) — **ANSWERED** (2D / software)

Two faces of the same virtio-gpu scanout, both driven by the single render
task that owns the GPU (the only `flush()` caller).

- **Framebuffer (`/dev/fb0`)** — 2D scanout, BGRA32; `read`/`write` (write
  copies in and blocks on the through-flush), `mmap` of the real DMA frames
  (pinned), and the fbdev ioctls `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`.
  `FBIOPAN_DISPLAY` and `FBIO_WAITFORVSYNC` are answered, and on a compositor
  boot `FBIOPAN_DISPLAY` *presents* (drives the flush), which is what weston's
  fbdev backend uses per frame. `FBIOPUT_VSCREENINFO` is refused (a resolution
  change would tear down live mappings).
- **DRM/KMS (`/dev/dri/card0` and `/dev/dri/renderD128`, `src/drivers/drm.rs`)**
  — a virtual single-CRTC card speaking the Linux UAPI exactly
  (`drm.h`/`drm_mode.h`): `VERSION`, `GET_CAP` (`DUMB_BUFFER`, `PRIME`,
  `TIMESTAMP_MONOTONIC`, `CRTC_IN_VBLANK_EVENT`, the cursor size),
  `SET_CLIENT_CAP`, `MODE_GETRESOURCES`, `GETCRTC`/`SETCRTC`,
  `GETENCODER`/`GETCONNECTOR`/`GETPLANE`/`GETPLANERESOURCES`,
  `GETPROPERTY`/`SETPROPERTY`/`GETPROPBLOB`/`CREATE_BLOB`/`DESTROY_BLOB`,
  `ADDFB`/`ADDFB2`/`RMFB`/`GETFB`, `PAGE_FLIP`, `MODE_ATOMIC`,
  `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`, and GEM dumb buffers
  (`CREATE_DUMB`/`MAP_DUMB`/`DESTROY_DUMB`) whose `mmap` aliases real pages with
  the same per-mapping frame reference discipline the memfd uses. A page flip is
  applied to the scanout and reports back as a flip-completion `drmEvent`
  readable (and pollable) on the card fd. `SET_MASTER`/`DROP_MASTER` are a real
  claim rather than a success: the master belongs to an open file description,
  is bound to the console the caller is on, and loses the screen - `EACCES` on
  every presentation request, the queued events drained, `POLLHUP` on the card
  fd - the moment that console is switched away from (§7.1).
- **The render node, PRIME and the GL stack.** `/dev/dri/renderD128` is the
  same card state under the render minor, which is the node GBM opens to
  allocate; `PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE` (both directions, so
  `DRM_CAP_PRIME` answers `IMPORT|EXPORT`) turn a dumb buffer into a dma-buf
  and back, and `src/vfs/dmabuf.rs` is the object that crosses. That is the
  whole of what Mesa's GBM needs to hand a buffer to the card, and it is why
  the image boots **weston on DRM + GL** (`weston-backend-drm` ships only the
  GL renderer; the driver behind it is llvmpipe — the software GL that
  `mesa-dri-gallium` provides). So "the presentation path a pixman compositor
  needs" is no longer the whole of it: **GBM, EGL and GLES are served**, in
  software. What is still absent is **3D/virgl** (no `VIRTGPU_*`), **sync
  objects**, **format modifiers**, and the property and metadata families a
  compositor reads by name - all of it the subject of §7.4.

v1 divergences: one fixed connector/crtc/encoder/plane; one synthesized mode
(the GPU's size at 60 Hz, `PREFERRED|DRIVER`), no `EDID` blob and
`mm_width`/`mm_height` of zero; a flip-completion event is stamped
with the frame's landing (the render task's present), not the commit, and the
card fd announces it - but there is no vertical blank and no `WAIT_VBLANK`; dumb-buffer mmap uses a `handle << log2(PAGES_PER_BUFFER)`
offset encoding rather than a DRM VMA manager; leasing is a restricted
descriptor but `DRM_MODE_SEAL_ALL` and `LEASE_IMMUTABLE` are recorded and not
enforced, so a sealed lease can still be revoked and a sealed object still
accepts a mode change; and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` and
the colour-pipeline cap are refused `EOPNOTSUPP`, the first of which the
virtualized drivers are the ones that ask for.

**The two fences a commit carries are no longer that list's example.**
`IN_FENCE_FD` used to be "stored but nothing waits on it", and there was no
`OUT_FENCE_PTR` at all; both are now real - a commit handed an acquire fence is
ordered behind it before a pixel is copied, and a commit asked for an out-fence
writes back a descriptor the caller can wait on - and the `SYNCOBJ_*` family
that gives them something to name is dispatched. See "What the sync object
family answers" below for what the family means here, which is measured rather
than assumed.

**The scanout follows the host's display size.** The resolution used to be the
one `GET_DISPLAY_INFO` answered at boot and nothing else, which is a bug the
moment a QEMU window is resized or fullscreened: the host keeps the size it now
wants in the GPU's own device configuration (`events_read`, with
`VIRTIO_GPU_EVENT_DISPLAY` set and a config-change interrupt raised), and a guest
that never reads it goes on driving a 1024x768 scanout inside a 1920x1080 window.
What that looks like is a desktop confined to the top-left corner with a pointer
that cannot leave it - the report this fixes - because the window is showing the
guest's screen unscaled and the compositor's output *is* the guest's screen.

So the once-a-tick virtio service task now reads that one word
(`virtio::gpu::take_display_event`, an MMIO read, not a control-queue round trip:
`GET_DISPLAY_INFO` is only spent once the bit says there is a reason), and hands
the work to the render task, which owns the device. The task re-reads the size,
rebuilds the scanout resource at it (`change_resolution`), repins the new frames,
copies what was on the screen into the new one - the old block is still pinned
across the teardown, deliberately, so the pixels survive the swap - returns the
old block to the allocator (unless userland mapped the pixels, in which case it
stays pinned: a stale mapping that writes into recycled frames is worse than a
leaked screen), publishes the new geometry to `/dev/fb0` and the DRM card, and
rebuilds the console's grids to the new cell count.

And it tells userland, which is the half that makes the pointer work: a mode
change is announced as a **`change` uevent carrying `HOTPLUG=1`** on the card
device (`drm::display_changed`) - the DRM uapi has no hotplug event type, and
`libdrm`'s `drmHandleEvent` refuses a type it does not know - and a compositor's
udev monitor re-probes its connectors on exactly that word (weston's DRM backend
logs "[udev] HOTPLUG event"), finds the new mode from `GETCONNECTOR`/`GETCRTC`,
and re-modesets. `Event` gained the `extra` key list for those subsystem words,
which is where Linux's `envp` extras live too.

Two costs are real and named: a resize costs the console's scrollback (the grids
are rebuilt from the log ring, as at boot), and a drag's stream of window events
is coalesced by the render beat into one rebuild per frame rather than one per
event.

**The console's text grid, and the window it reports.** The framebuffer console
paints an 8x16 bitmap grid sized from the scanout (1024x768 is 128x48 cells,
1280x800 is 160x50, `MAX_ROWS` capping the rows at 64), and a virtual console
now *reports that grid* as its window: `TIOCGWINSZ` on `/dev/ttyN` asks
`Grid::rows/cols`, because the framebuffer console **is** that terminal's
emulator and the grid **is** the window. It used to answer a constant 80x25,
which is how a full-screen program came to draw into the top-left corner of the
screen - `vim` laid out 80 of 128 columns and 25 of 48 rows and left the rest
blank, which is the bug that reported it. `/dev/tty0` and `/dev/console` name
the console *currently shown*, so they answer the same way; both fall back to
80x25 when there is no screen at all - a machine with no display, which is not the
same thing as a boot that merely keeps its log on the serial wire (§7.1c).

**The other half of that pair is `TIOCSWINSZ`**, and it is the one that matters
where the kernel *cannot* know the size: a terminal emulator in userspace - one
inside weston, say - opens a `/dev/ptmx` pair and names its window, and the
program it runs reads that back on the slave. So a pty keeps a window of its own
(25x80 until an emulator says otherwise) and reports it, while the console
stores a set size and goes on answering its screen - the screen is what is being
painted, and a number that disagreed with it would be the same lie the
corner-drawing bug came from. A *change* raises `SIGWINCH` to the foreground
group, by the same route a `Ctrl-C` takes: a pipeline is one job and every
process in it has the same window, and the signal is the only way a running
full-screen program learns it has been resized. `stty rows`/`cols` and `resize`
are what that pair is for.

**Who may paint the screen: `VT_PROCESS` and a master that can be taken away.**
The symptom was specific and it was measured by hand rather than by a test: on a
`--mode weston` boot, switching to a text console and then *moving the mouse*
gave the compositor its screen back. Three absences produced that, and they were
all in the same place - nobody had ever asked the question "is this screen
yours?" on a compositor boot:

* `src/drivers/video/render.rs` tested `present_only ||` **before**
  `vcmux::painted()`, so on a `mpos.compositor` boot the veto existed and was
  never consulted.
* Every presentation request (`SETCRTC`, `PAGE_FLIP`, `MODE_ATOMIC`, a
  `SETPROPERTY` that moves a plane, `RMFB`) applied its pixels and *then* asked
  the render task to flush, unconditionally - so a refused flip would still have
  painted.
* `SET_MASTER`/`DROP_MASTER`/`AUTH` answered `0` and recorded nothing,
  `VT_SETMODE` accepted any struct without reading it, `VT_GETMODE` always
  reported `VT_AUTO`, and `VT_RELDISP` always agreed. A session therefore could
  not be told it had lost anything, and could not refuse.

The ABI is pinned in `src/vfs/vtmode.rs`, which holds no clock, no tasks and no
signals so that a host build can argue with the whole negotiation: `struct
vt_mode`'s eight bytes (`mode, waitv, relsig, acqsig, frsig`, padding
zero-and-read-zero on `VT_GETMODE`, which is a `_IOR` of exactly that size), the
`EINVAL` set `<linux/vt.h>` uses (unknown mode, `relsig == acqsig`, a signal
number that is not a signal), and the rule that a refused request is *settled*
rather than left pending - a refusal is modelled apart from a release precisely
so that an unrelated later switch cannot be forced through by a timer that is
counting a conversation nobody is having. Note that `VT_SETMODE` is
`_IOW('V', 2, struct vt_mode)` while every other request in the family is `_IO`:
a kernel that decoded the size from the group rather than the request would read
zero bytes of a caller that wrote eight, and `t-seat` now writes the `_IOW` form
for exactly that reason.

The mechanism is in `src/vfs/vcmux.rs`, and both halves of it were then measured
against `drivers/tty/vt/vt.c` and `drivers/tty/vt/vt_ioctl.c`, which moved them.

`^A <digit>` and `VT_ACTIVATE` share one decision
(`vtmode::Table::request_switch`) and **neither one waits**. Linux's `VT_ACTIVATE`
is `set_console()`: record `want_console`, schedule the console callback - which
is `change_console()`, and that is where the releasing console's `relsig` goes out -
and return 0. There is no `vt_release` sleep anywhere in the current console code,
and a waiting `VT_ACTIVATE` is not merely un-Linux-like, it deadlocks a seat
manager: `vt_switch` writes `VT_SETMODE(VT_PROCESS)` on the console it is standing
on and then asks for a different one, so the promise that owes the answer is the
asker's own, and the asker is parked inside the call its answer would unblock.
`VT_WAITACTIVE` is the one console ioctl that sleeps - `vt_event_waitqueue`,
interruptible, no timeout - and it still does.

Leaving a `VT_PROCESS` console sends its `relsig` - `SIGUSR1`/`SIGUSR2`, named in
`src/sched/signal.rs`, both fatal by default - **to the process that wrote
`VT_SETMODE`, and to nobody else**. That is Linux's `vc->vt_pid`: `vt_setmode`
stores `get_pid(task_pid(current))`, and `vt_acquire`/`change_console` deliver with
`kill_pid(vc->vt_pid, …)`. Aiming those two signals at the console's *foreground
group* was this kernel's earlier shape, and it is why
`cargo xtask run --mode weston` could not be run twice: seatd's `vt_open` is
`open /dev/ttyN`, `VT_SETMODE`, `KDSKBMODE`, `KDSETMODE`, `close(ttyfd)` - so the
group in the foreground when a handover signal went out was weston's (which is
what `ash` reporting `User defined signal 2`, i.e. `strsignal(12)`, was saying when
the desktop was being quit with `^C`) or the login shell's, and neither has a
`SIGUSR2` handler for a promise it never made. Two rules come out of the same
measurement and both are pinned in `vtmode`'s host tests: a promise is **not**
ended by the last close of the descriptor that wrote it - that close is the normal
case, and clearing there would unmake the claim in the breath it was made - and a
promise whose keeper has died ends where Linux ends it, at the delivery that cannot
land, running Linux's `reset_vc` (`VT_AUTO`, no signals named, and the console back
to `KD_TEXT` so the text that replaces it is actually visible) instead of waiting
on a ghost.

The deadline that bounds a *wedged* keeper - 15000 ticks, `RELEASE_TIMEOUT_TICKS` -
is this kernel's own, not Linux's: with no timeout and no answer, Linux's pending
switch simply sits until the session speaks or the box is rebooted. Keeping a bound,
plus the operator's second `^A` forcing the switch through, is what stops one stuck
compositor owning the machine. `frsig` is dropped on the way in, as Linux drops it
("the frsig is ignored, so we set it to 0"), so a refusal is heard only as the
requester's own `VT_ACTIVATE` return or a `VT_GETSTATE`; and a console number the
machine does not have is `ENXIO`, not `EINVAL`.

What that still leaves as named gaps: Linux holds a refcounted `struct pid`, so a
promise can never be inherited by an unrelated process later given the same
number, while this kernel stores the reusable task slot - closing that wants a
per-task generation count; `VT_SETMODE` here refuses `relsig == acqsig` and a
signal number outside 1..=31, both of which Linux accepts without looking; and the
only vector with an IST stack is `#DF` (see §11.1's last-resort entry), with no
NMI or machine-check handler installed at all.

The master is now a claim rather than a formality. It belongs to the *open file
description* (`File::ofd`), because that is the only identity that survives the
way a seat manager actually hands over a card - `SCM_RIGHTS` passes the
descriptor, and seatd's and weston's fds are one description - and it is bound to
the console its taker was on, read from the controlling tty at `SET_MASTER` time.
`AUTH` answers `EACCES` to a non-master and `EINVAL` to the master, which is
inverted from what one would write by instinct and is exactly what libdrm's
`drmIsMaster` reads. When the bound console stops being the one on the screen -
which is *only* ever measured as `holds_screen()`: master's console, on the
screen, in `KD_GRAPHICS` - the card drains its queued events and reports
`POLLHUP`, because a compositor parked in `poll` inside `drmHandleEvent` waiting
for a flip that will now never complete is not merely frozen, it is frozen with
the keyboard pointed at it. Releasing the master on the last close follows the
`EVIOCGRAB` precedent beside it, for the same reason: a killed compositor must
not hold a screen from beyond its own grave.

Two judgement calls are load-bearing and both came out of watching what broke
first. A master *not* bound to any console - a test program with no controlling
tty - is allowed to keep driving and is never allowed to veto the text console,
so `master_vc()` and `master_holds_screen()` are different questions answered
separately. And the render task now builds the six console grids on a compositor
boot too, because taking the veto away from `present_only` without giving the
text VT something to show replaces a stolen screen with a black one; `takeover()`
is still not called, so the kernel log and the `^A` keystroke keep riding the
serial wire and `--mode weston` keeps its promise about which window is which.
The grids are fed but not drawn until somebody holds the card, which is what
keeps the pre-seat window empty rather than filling it with a boot log the mode
says belongs to the compositor. Input follows the same fact from the other side:
`virtio/input.rs` delivers a grabbed node's events only while the grabbing
session holds the screen, and hands the keystrokes to the console when it does
not - the complement of a rule that used to be merely "grabbed means not the
console", which left a released VT with a keyboard that reached nobody.

Rejected: reordering `present_only` and `painted()` on its own, which fixes the
symptom by handing the loser the screen and the winner nothing; and refusing a
flip *after* committing it, which is what the ordering at the four presentation
sites would have made natural, and which paints the frame it just refused.

**Closing the remaining gaps to Linux.** The shape above was deliberately looser
than Linux in six places, and each is now the ABI it imitates rather than an
approximation of it:

* `/dev/tty0` is its own inode, `(4, 0)`, and every open of it resolves through
  `Inode::open_replacement` to the console actually on the screen - so `ttyname`
  on a descriptor opened that way says `tty3`, and a foreground-process-group
  rule has something to be a rule *about*. It used to be the console tty
  registered a second time, which made it `(4, 64)` and left the `(4, 0)` arm of
  `Tty::vc_number` unreachable.
* `VT_SETMODE`, `VT_RELDISP`, `VT_ACTIVATE`, `VT_WAITACTIVE`, `VT_WAITUNTIL`
  and `VT_OPENQRY` ask
  who is calling: the terminal's own foreground job, or superuser. Without that,
  any process holding a `/dev/tty` fd could answer a compositor's `relsig` for
  it, or write its mode. `KDSETMODE` keeps Linux's second door as well - the
  holder of the card may set a console's video mode, because the driver that owns
  the screen is what says what is drawn on it.
* `KDSKBMODE(K_OFF)` now swallows keystrokes, *for the virtual console only*. The
  first attempt at this dropped them in the console mux, which also carries the
  serial line, and produced a machine that could be locked out of its own shell;
  the mode belongs to the console's keyboard, and the serial wire is the
  machine's, so `virtio/input.rs` consults it and `vcmux::forward` does not, and
  a session's last tty close restores `K_XLATE` alongside the mode it already
  restored.
* `GET_MAGIC` mints a per-descriptor, one-shot token and `AUTH_MAGIC` is the
  master's to spend, which is how a compositor lets a client's descriptor
  page-flip without making it a master. A spent token is gone, an unknown one is
  `EACCES`, and closing a descriptor retires both its tokens and its
  authorisation - a token's whole purpose is to name a file description, and a
  freed one would name whatever lived at that address next.
* `SET_MASTER` and `DROP_MASTER` are `void`, which is not the shape one writes by
  instinct: taking the card *steals* it and ends the previous holder's claim,
  because `SET_MASTER` is also the recovery path for a compositor that lost its
  console to a `^A` and just won it back. A refusal there would leave the screen
  permanently promised to a session that no longer has it.
* `MODE_CREATE_LEASE` / `LIST_LESSES` / `GET_LEASE` / `REVOKE_LEASE` exist: a
  lease is the same card state behind a descriptor restricted to a list of object
  ids, only the master may mint one, a lease may not lease out what it was not
  given, and revoking empties it while leaving the descriptor open, which is what
  Linux's dead file private looks like from the holder's side. `IMMUTABLE` and
  `SEAL_ALL` are recorded and reported and *not* enforced - the honest
  half-measure, since sealing there refuses property changes this synthesized
  card does not have.

**Those four requests were numbered wrongly, and it is the one bug in this file
that no test in the repository could have caught.** They were written as
`0xd3`..`0xd6`; `<drm.h>` puts the family at `0xc6`..`0xc9`, and both copies of
the header on the development host - `/usr/include/drm/drm.h` and libdrm's -
agree. Thirteen too high means the request misses every arm of the dispatch
`match` and lands in the `_ => -25` catch-all, so a lease cost a real caller an
`ENOTTY`: libdrm's `drmModeCreateLease` sends `0xc6`, and it never reached
`create_lease` at all. The doc comment beside the constants said the numbers came
from `<drm_mode.h>`, which is where they do not live - ioctl numbers are
`<drm.h>`'s - and that is most of how it happened.

`t-drm-lease` was green throughout for a reason worth stating: it built its four
requests from *the same constants the driver matched on*, so the test and the
driver agreed about a number no userspace ever sends. A test whose expectation is
drawn from the value it is checking agrees with any value at all, which makes it
no test. The fix is therefore two-sided. The constants are `drm.h`'s, and the
test now compares each request word against a **literal from the header** -
`ioc_d(0xc6, 24)` and its three siblings - written out in the check rather than
read from the constant beside it, so the four disagree the moment either side
drifts. The other sixty-odd `NR_*` constants in the file were checked the same
way against the same header and all agree; this was the only family that did not.

And a flip-completion event is now queued *after* `request_flush_blocking()`
returns, so its timestamp is the frame's landing rather than the commit, and the
card fd gains a wake list (`poll_subscribe`) instead of relying on the tick
re-check `poll_select`'s comment lists it under. `FBIO_WAITFORVSYNC` waits for
that boundary with a 40 ms budget. It is called a landing everywhere in the code,
because there is no display controller here to be in blank; `WAIT_VBLANK` and its
sequence requests are still `ENOTTY` rather than an answer that would be a
clock this kernel does not have.

### 7.1b What the console answers, and what it deliberately does not

A terminal question (`ESC [ 6 n`, `ESC [ 5 n`, `ESC [ c`, an `OSC 10/11 ; ?`, a
`DCS 1 + q`) is answered by exactly one thing: the emulator that owns the screen
those bytes are drawn on. Which one that is, is a per-terminal question, and this
kernel used to answer it wrong in the one direction that is invisible until a
program is damaged by it.

The measurement that settled it, on Linux against a pty pair - slave in raw mode,
`ECHO` off, `VMIN=0/VTIME=1`, each of six sequences written to the slave and both
ends read: `ESC [ 6 n`, `ESC [ ? 6 n`, `ESC [ 5 n`, `ESC [ c`, `OSC 11 ; ? BEL`
and `DCS 1 + q … ST` each arrived at the master **byte for byte**, and **none** of
them was answered (6 of 6 runs, no queue at the slave within `VTIME`). A pty is a
wire; the emulator holding the master has the cursor, the window and the
capabilities. `drivers/tty/vt/vt.c` answers the same questions for a VT because
there the kernel *is* the emulator.

What this kernel did instead was answer everywhere, and `fish` showed the cost.
Measured on the guest (`--mode terminal`, `fish 4.8.1`, `TERM=linux`, root's shell
on the serial console): the shell's own reader log says
`Writing terminal command QueryCursorPosition` then `Received cursor position
report y: 24 x: 0` - the question was answered here, and never reached the wire,
because answering also swallows it. And `stty size` reported `25 80` while
`fish`'s prompt-spacing block padded the dangling line out to exactly the
*reported* column count: `stty cols 40` changed the block from 80 filler cells to
40, while the cursor report kept counting against 80. On a window wider than the
number the kernel guessed, the filler never reaches the margin, no wrap happens,
and the `CR` + `ESC [ K` that close the block erase the line the command printed -
which is why `echo -n hello` disappeared under the next prompt here and did not on
a real terminal, whose emulator knows its own width and answers truthfully.

So the rule is now `src/vfs/tty.rs`'s `Attached`, decided per write by
`Tty::kernel_draws`, and it is not "is this a virtual console" - all six consoles
are pty pairs too, and what distinguishes them is who reads the master:

| the bytes land on | who answers a question |
|---|---|
| a grid the kernel has made (`term::has_screen`, i.e. a framebuffer boot) | the kernel, and the question is kept off the wire |
| the serial wire alone, no grid (`--mode terminal`) | the person's terminal, reached by the forwarded question |
| a `/dev/pts` master a process holds | that process's emulator |
| `Discard` - a terminal with nothing behind it | the kernel, because silence there is a hang |

The second half of the same fix: the cursor a report answers with is now the
cursor the screen is drawn with. Both the discipline and `term::Grid` drive one
`crates/term/src/cursor.rs` - the window size, the scroll region, deferred autowrap,
line feed at the region's bottom, `DECSTBM`, save/restore (`ESC 7/8`, `CSI s/u`),
`ESC D/E/M`, the alternate screen - and
`vfs::tty::tests::the_report_and_the_screen_never_disagree_about_the_cursor`
feeds 37 streams through both and asserts one pair of numbers out of each. The
discipline's own `Screen` is gone: it was a 25x80 guess, pinned at row 24 forever
because it clamped instead of scrolling, blind to `TIOCSWINSZ`, and it did not see
the echo of what was typed at it (echo now goes through the same tracking).

What is answered is now exactly what `drivers/tty/vt/vt.c` answers, and the rest
is a gap named here rather than approximated:

* `csi_ECMA`'s `case 'c'`: `if (!vc->vc_par[0]) respond_ID(tty);`, and
  `respond_ID`'s entire answer is the VT102 id `"\033[?6c"`. So `ESC [ c` and
  `ESC [ 0 c` are answered, and nothing else in that family is.
* `csi_DEC`'s `case 'c'` is **not a question at all**: `CSI ? Ps c` is the console's
  *cursor-type set* (`vc_cursor_type = CUR_SIZE(par…)`). This is what the bug that
  found this section was made of: the `linux` terminfo the editors here read
  builds its cursor capabilities out of sets - `civis=\E[?25l\E[?1c`,
  `cnorm=\E[?25h\E[?0c`, `cvvis=\E[?25h\E[?8c` - and vim writes `civis` on every
  command line it paints and again as it leaves. Answering a set queues a reply no
  one asked and no one reads; it waits in the input queue as typed text for
  whoever reads next, so quitting vim left `9c9c` on the prompt and busybox tried
  to run it (`-sh: 9c9c: not found`). Measured with `strace` on the guest: two
  `write(1, "…\033[?1c…")` with no `read(0, …)` after them, and two answers of
  `\033[?62;1;2;6;7;8;9c` queued.
* `csi_ECMA`/`csi_DEC` both handle `case 'n'` with the same 5/6 test, so
  `ESC [ ? 6 n` is the same question as `ESC [ 6 n`. It was not treated as one:
  the swallow was measured off the raw parameter *bytes* (`params.first()` is
  `?`) while the answer was measured off the parsed ones, so the private form left
  unanswered - a caller waiting on a reply the console does send. `first_query_param`
  is now the one parse both halves read.
* `EPgt`/`EPeq`/`EPlt` sequences - which is `CSI > c`, the secondary Device
  Attributes request - reach neither handler, so the console does not answer them
  and neither does this one. It travels to the wire instead of being answered with
  an identity invented here.

Two gaps with names, not stubs: the cursor type a `CSI ? Ps c` asks for is not
drawn (there is no `vc_cursor_type` behind this console's cursor, which has one
state - `Grid::cursor_visible`), and a `--mode terminal` boot with a real terminal
on the far end therefore does not get the blinking or shape that sequence asks
for. Pinned on the host by `a_cursor_type_set_is_forwarded_and_never_answered`,
`a_device_attributes_query_is_answered_and_not_forwarded`,
`a_secondary_device_attributes_query_is_forwarded_not_answered`,
`a_query_to_a_terminal_the_kernel_does_not_draw_travels_unanswered` and
`a_query_to_a_screen_the_kernel_draws_is_answered_and_kept_off_the_wire`, and in
the guest by `t-tty-query`, which drives one end of a pty pair as the program and
reads the other as the emulator - so "answered and not forwarded" and "not
answered, and therefore forwarded" are two different observable queues rather than
a reading of the same one. Quitting vim on the built image, which is how this was
reported, now leaves the prompt empty; it used to leave `9c9c` and
`-sh: 9c9c: not found`.

Three more, each with the prerequisite it is waiting for:

* **The answering half cannot be pinned by a booted test.** `cargo xtask test`
  boots with no display, so no grid exists, so every question a booted program
  asks of a `/dev/pts` or a console pair is forwarded - which is the right answer
  for that boot and pins nothing about the case where the kernel draws the screen.
  `t-tty-query` covers the forwarding half end to end; the answering half rests on
  the host tests above until the gate grows a framebuffer boot.
* **A serial console's width is not knowable from the guest**, and Linux has no
  better answer for a `ttyS0` either: `stty cols 120` is the fix a person types,
  and a forwarding boot no longer needs it, because the terminal at the other end
  reports its own geometry now. What *was* wrong - guessing `80` and then also
  answering the question so that nothing could correct it - is fixed. A terminal
  that wants to say its size (`IAC SB WILLSYNCD?`-style negotiation, or `ESC [ 2048 ) 0`)
  is not implemented and is not pretended to be.
* **Cell counting for a non-ASCII glyph is unmeasured, so it is unfixed.** Both
  models count one byte as one cell, which is what `Grid` has always drawn and what
  the cursor inherits from it - consistent, and wrong for a UTF-8 terminal in
  exactly the way a `⏎` (3 bytes, 1 cell) shows up. Aligning it needs a
  measurement of wide/narrow cell advance on a real Linux console first; that, not
  a per-byte guess, is the number to copy. Same for `CSI S`/`T`, `L`/`M`, `@`/`*`,
  `P`/`X`, `DECSTR`, `DECCOLM` and origin mode (`DECOM`), which neither model
  implements: a sequence nobody acts on moves nobody's cursor, so the report and
  the screen still agree, and each stays a gap rather than a half-implementation.

* **The kernel's line editor is gone; `n_tty`'s is what remains.** The canonical
  mode used to keep a 64-line history for the up arrow, walk it with the arrow
  and `^P`/`^N`, offer `^A`/`^B`/`^E`/`^F`/`^K`/`^L`/`^T`, and recognise an
  `ESC [ A` as one key rather than as characters — all of it a *line editor's*
  job on Linux, where `n_tty` claims exactly ERASE/KILL/WERASE/REPRINT/LNEXT and
  types every other byte into the line, arrows' `ESC [ A` included. The shells
  and editors on this image (fish, vi) run the terminal raw and bring their own
  editing, so the kernel's editor served nobody and disagreed with `read(2)`
  about what a canonical line contains. Canonical mode now: unclaimed control
  bytes are characters (echoed as their `ECHOCTL` `^X` form, and `ESC [ A`
  reaches a canonical reader inside its line), kill/werase erase from the end
  with `\b \b`, and `^R` reprints the line without replaying a prompt the
  discipline never owned. Pinned in the host tests
  (`a_typed_escape_sequence_reaches_the_reader_with_the_line`,
  `unclaimed_control_bytes_are_typed_into_the_line`,
  `vreprint_redraws_the_pending_line`) and in the guest by
  `t-tty-query`'s cooked half.

---

### 7.1c Which consoles the kernel has — **DONE**, two gaps named

**The kernel used to be told how it was launched, which is not a fact about a
machine.** `cargo xtask run --mode terminal|gui|mirror|weston` picked one of four
words — `mpos.headless`, `mpos.nomirror`, `mpos.mirror`, `mpos.compositor` — and
the kernel re-derived its whole console policy from it: whether the framebuffer
console was *the* console, whether the serial wire also got the bytes, and whether
the GPU was engaged at all. The first three are one question in three costumes.
The fourth — "is there a window?" — is a fact about the *host*, and the machine
cannot discover it either: a QEMU booted with `-display none` still has a
virtio-gpu and still answers `GET_DISPLAY_INFO` with a mode (that is the note the
headless flag used to carry to justify itself). So the old shape had a word that
was neither a machine fact, like a device, nor a Linux one.

What replaced it is the parameter Linux has always used for exactly this:
`console=`. `src/boot/console_list.rs` reads it — allocation-free, because it runs
at the top of the boot task before the heap exists — and the rules are Linux's:

* every `console=` word names a console, and the log goes to all of them;
* the **last** one is preferred: its writes park until the hardware has taken them,
  where a peer sink is written best-effort, so a slow host terminal can never
  throttle the console somebody is reading;
* a value naming a console this machine does not have is reported and ignored, as
  Linux reports and ignores one.

Two names are accepted: `ttyS0`, the wire, and `tty0`, the virtual console on the
screen (Linux's spelling for "the console currently displayed"). The four run modes
became four lists — `console=ttyS0` for terminal and weston, `console=tty0` for gui,
`console=ttyS0 console=tty0` for mirror — so mirror stopped being a flag pair and
became an *order*, and terminal and weston became the same list, because what
differs between them (a compositor driving the scanout) is runtime, not boot-time.

**Four lists were one list too many, and three modes are the honest count.** That
last sentence is the reason the fourth word went away: a mode that tells the guest
exactly what another mode tells it was never a fourth machine. `weston`/`compositor`
is deleted, and `gui` — which had been the fourth shape, `console=tty0` with the
serial line sent to `/dev/null` — is the wire-alone list with a window on it. What
is left is the two questions a mode can actually answer: is there a window, and is
the framebuffer one of the guest's consoles. So `terminal` and `gui` share
`console=ttyS0` and differ only in `-display`; `gui` and `mirror` share the QEMU
wiring and differ only in one `console=tty0`. Every mode now puts the wire on stdio,
which deletes a branch rather than adding one: the mode that discarded the serial
line discarded the kernel log with it, and a log nobody can read is not a machine
anybody wants — that was the one mode whose wiring and console list could disagree
about the wire, and `run_modes_round_trip_and_name_their_console_list` used to spend
half its assertions policing that agreement.

`weston` is *not* kept as a synonym for `gui`, and that cost a shape change in the
config layer. `[qemu] mode` used to fall back to `mirror` when it could not parse
the word (so a `cargo xtask test` boot, which attaches no window, would not fail on
a typo it never uses) — a fallback that is now the wrong answer, because the retired
word names a machine whose consoles differ from the default's. `QemuConfig` therefore
stores the key as text (`mode_text`) and `run_mode()` resolves it at the two places
that boot a window: absent or blank means `mirror`, an unknown word is an error. The
image marker keeps its `build/console.mode` name and its old contents read as "no
marker": an image baked as `weston` costs one boot-file refresh to rename it, and the
`mpos.cmdline` it rewrites is already `gui`'s.

Records elsewhere in this file that say `--mode weston` are measurements of the boot
that name described, and are not rewritten. The reading of an old `--mode gui` record
now splits by what it describes: one that says the console is *on* the framebuffer —
line 308's "with the serial line discarded", the `KD_TEXT` console of line 228 — and
today's `gui` agree again (see the reversal record below); one that describes the
window left for a compositor and the log on the wire was the a0015ba interim shape,
and its machine is today's `--mode terminal` with a window, or `--mode mirror` for
the console-in-both-places.

**The rule the design rests on, and what was not measured.** "The log goes to
every console named, and the last is preferred" is `kernel-parameters.txt`. The
reading this kernel depends on is stronger: a console *not* named is not a console
at all. That is what makes `console=ttyS0` alone mean "the wire and nothing else",
and therefore what lets `--mode terminal` be a one-word list instead of needing a
headless flag. It is the rule in `register_console()`, which drops a console the
command line did not ask for once a preferred console has been chosen — **read out
of the console registration code, not measured on a machine here.** The half that
*was* measured has its own record above: `SET_MASTER`, `VT_PROCESS` and
`KDSETMODE` were measured against `vt.c`/`vt_ioctl.c` when the seat landed.

**The display device is engaged on every boot now.** A wire-only boot used to
`drop` the GPU and never hand it to the render task; that was the same flag doing a
second job. With the flag gone the task is spawned whenever the probe finds a
device, because `/dev/fb0` is a device a program may open and a session may claim
the card, whatever the kernel's own console list says. What the list decides is
only whether the kernel's *log* moves onto the screen.

**The grids are made whenever there is a display.** Six grids sized to the
scanout (160x50 cells on a 1280x800 window) are made when the render task starts,
in every mode, exactly as the compositor path always did; the console list decides
only whether the kernel's *log* moves onto them. The alternative was tried first
and rejected on measurement: make them lazily, on the first
`DRM_IOCTL_SET_MASTER`, since that is the moment the scanout can be handed to a
text console and therefore the moment a released VT has to be drawable — six grids
cheaper on a boot that never draws. `t-ioctl` caught what that costs: it failed
with `the console reports 25 rows (got 50, want 25)` and `80 columns (got 160,
want 80)`, because the tests that take DRM master (`t-seat`, `t-drm*`) run earlier
in the same suite, so the console's window *changed under it* mid-run. A console
whose geometry depends on who holds the card is neither Linux's answer — fbcon
binds to the display whatever the DRM master is doing — nor a stable one, and the
test was right to say so. What it pins now is the rule: the console's window is the
screen's grid in cells whenever there is a screen (checked against the
framebuffer's own `FBIOGET_VSCREENINFO` and the 8x16 font), and 25x80 only when
there is none. The cost is real and accepted: six grids on every boot that has a
display, including a `cargo xtask test` boot that never shows them, which is what a
machine whose display device is up pays.

**`cargo xtask test` on this change.** All milestone markers (`M0_OK` … `M8_OK`,
`All ready`), 131 user programs built, 114 `=== RUN`, 112 `=== PASS` - and the run
does **not** reach `=== SUMMARY`, because `t-tty-vtime` hangs and times it out (the
pre-existing red `AGENTS.md` records, unchanged by this work). One FAIL:
`t-stdresolve`, 19 of 20 live DNS rounds, one `baidu.com` lookup answering
`EAI_AGAIN` while this host was also running the build; every other network program
passed and it passed in the previous run, which is the shape `AGENTS.md` says to
read as load. `t-ioctl` passes on the new rule (the log shows `the console's rows
are the screen's`). The 14 programs after `t-tty-vtime` in name order never ran -
`t-dsp`, `t-udev-worker`, `t-uevent`, `t-umask`, `t-uname`, `t-unix`, `t-unix-dgram`,
`t-unix-scm`, `t-unlinkat`, `t-wait4`, `t-waitid`, `t-write`, `t-writev`, `t-xattr` -
which is a limit of this tree and not of this change: the hang is where the suite
stopped before it too.

**Two gaps with names, not stubs.**

* `console=tty1`..`console=tty6` — Linux's "pin the log to that VT" — is *refused*
  rather than approximated: this console writes to the VT that is on screen, which
  is `tty0`'s meaning, so answering the word while doing something else would be
  the private dialect the conventions refuse. The value is reported as unknown and
  the boot proceeds on the consoles it did name.
* `/dev/console` has no preferred-console plumbing: it follows the console
  currently on screen, and the list's preference decides only the write policy.
  Linux makes the last `console=` both the preferred console's device and the
  routing rule; only the second half exists here.

**`gui` moved back onto the framebuffer, with the wire kept — a deliberate
reversal, not the old `/dev/null` mistake.** The paragraph above folded `gui` onto
`console=ttyS0` (wire-alone list, window attached) and deleted the shape that put
the text console on the screen. A user asked for that shape back: the whole machine
in the window — boot log, virtual consoles, login prompt and shells all rendered
there, the host terminal quiet. So `gui` now bakes `console=tty0` (the parser always
supported the framebuffer-alone list; the note above that "no run mode bakes this
list any more" is reversed). What makes this *not* the discarded-and-deaf mode the
collapse removed is where the wire goes: not stdio (the terminal stays quiet) and not
`/dev/null` (the log survives). It is a launcher-side copy to `build/serial.log`,
because three writers reach the port whatever the guest's console list says — the UEFI
loader prints to COM1 before the kernel runs, the kernel banner prints before the
`console=` parse, and a panic forces the serial wire back. The mode may silence the
terminal; it may not make it deaf. `gui` and `mirror` therefore now differ in the
guest's list *and* in the host wiring, so `a_mode_word...retired`'s claim that three
words own three distinct lists holds again, and the QEMU test renamed to
`a_mode_switches_the_window_and_where_the_wire_is_read` pins that `gui` puts the wire
on a file and not on stdio. The retired `weston` boot-file refresh now rewrites the
line for real (`console=ttyS0` → `console=tty0`) rather than only renaming the marker.
**Not yet re-booted:** the `gui` and `mirror` windows were not re-driven after this
change; the in-window-console behaviour is read off the `render.rs`/`wire_bytes`
`vt_is_sink` gate and the getty-on-`tty1` wiring, the same caveat the a0015ba record
carries — a manual boot of `cargo xtask run` (now defaulting to `gui`) is what closes
it.

### 7.2 Input: virtio-input (evdev) — **DONE**

`src/drivers/virtio/input.rs` is a real evdev provider, and this is what makes
`libinput` accept a device. At `init` each device's capability picture is read
from the virtio config (`ev_bits` per event type, `abs_info` per axis,
`prop_bits`, `ids`) and frozen; the node answers `EVIOCGVERSION`, `EVIOCGID`,
`EVIOCGNAME`, `EVIOCGBIT(evtype)`, `EVIOCGABS(axis)` and `EVIOCGRAB` (the ioctl
dispatcher in `fs.rs`). Nodes live at `/dev/input/eventN` (major 13, minor
64+N), are announced with `SUBSYSTEM=input` uevents, and have
`/sys/class/input/eventN/{dev,uevent}` - so udev/`libinput` enumerate them the
way Linux's do. Keyboard events also feed the console line discipline as before.
The QEMU machine attaches `virtio-keyboard-pci` **and** `virtio-tablet-pci` (a
pointer). A reader - blocking, `select`, or an `epoll` interest - is woken by
the service task the moment a record lands, and a grab is arbitrated per open
file description: `EBUSY` to a second claimant, released when the claimant's
descriptor closes.

**Who a record belongs to** is Linux's other half, and it is done. While the
grab is held, a reader that is not the grabber is told nothing - neither the
records, nor a readable answer from `poll` - and two readers of an *ungrabbed*
node each see every event, which the single shared queue could not do either
(whoever read first consumed it for everybody).

The device keeps one *window* of records and gives every client a *position* in
it (`crates/evdev`) instead of handing each client a buffer of its own the way
Linux does: the same picture with the copying shared. A position is a sequence
number in the device's record stream rather than an index into the window, so
the window dropping its oldest record - the bound that keeps an unread device
from pinning memory forever - moves the floor under a client instead of shifting
what it has not read, and a client that fell further behind than the window is
long resumes at the oldest record still held: the accounting Linux does with
`SYN_DROPPED`. A client is an open file description (`File::ofd`), the same key
the grab and `flock` already arbitrate by; its position is taken when the
description opens the node and forgotten when the description's last descriptor
closes, so an address that is reused cannot inherit a reader's place. While the
grab is held the non-owners' positions are lifted past the records that were the
grabber's, so a release hands the window to nobody - the records the grabber did
not read are still its own.

The policy sits in `crates/evdev` rather than in the driver, because the driver
is bare metal and the policy is arithmetic: nine host tests pin it (two readers
each see every record; a client that appears late does not read the backlog; a
grab keeps the records from every other reader; a release does not hand the
window over; a position below the window resumes at the floor; only the clients
a record is *for* are woken - so a non-owner is not woken to find nothing; a
short buffer is `EINVAL`; a forgotten client starts at the end of the window;
the clientless read is not the grabber). What `t-evdev` adds on the machine is
the *shape* of the rule, because the host sends no keystrokes during a run:
`poll` on a non-owner is never ready while the grab is held, the owner's own
answer is unchanged by its grab, and - when a record was queued, which the test
reports rather than assumes - draining one reader leaves the other's copy
intact. That last one is the assertion a shared queue fails, so a run with a
record to work with does discriminate; a run without one says so in its own
output. A *blocking* read by a non-owner of a grabbed device is the one case
that cannot be asserted from inside the guest without a keystroke, and the
`readable`/`take` pair in the unit tests covers it.

One divergence, recorded rather than papered over: `read_at` with no client - a
kernel reader that never named a description, which is the VFS's own `read_file`
and a `splice` source - reads as a client of its own, keyed 0, an address no
description can have. `read`/`readv`/`pread` all arrive through `File::read` and
so carry the real description; the clientless reader sees only what arrives after
it first reads, and someone else's grab excludes it, which is what Linux's
`pread` on a grabbed device from a non-owner means.

**The self-test's hands: `/dev/uinput`.** Everything above could be pinned on
the machine except one thing - a rule about *whose* event a record is needs a
record, and the self-test host sends no keystrokes. `crates/uinput/src/lib.rs` (the
interface's arithmetic) and `src/drivers/uinput.rs` (the device) close that. A
program opens `/dev/uinput`, declares what its device can report
(`UI_SET_EVBIT`, `UI_SET_KEYBIT` and the rest of the family), names it
(`UI_DEV_SETUP`), and asks for it to exist (`UI_DEV_CREATE`) - at which point it
appears as the next free `/dev/input/eventN` with the ordinary evdev face, and
every whole `struct input_event` written to the descriptor is what that device
reports. `UI_DEV_DESTROY`, closing the descriptor, or the process exiting takes
it away again, node and all: the descriptor *is* the device, so a `dup` is one
device and a second `open` is another. The arithmetic - which request means which
bit, what a write has to be shaped like, how `struct uinput_setup` is laid out -
is ungated and has six host tests of its own; the device is bare metal, like the
driver it feeds.

`t-evdev` now makes a device, finds it by the name it gave it, and pins the rule
with records it reported itself: both readers see the frame; draining one leaves
the other its copy - the assertion a shared queue fails; while the grab is held
the owner is readable and the non-owner is not, and the owner reads its own
event; a release hands nothing over; the next record is the other reader's
again; and destroying the device takes the node out of `/dev/input` and ends the
stream for a descriptor still holding it. None of it depends on the host, so it
runs on every sweep.

What a uinput device does *not* get, recorded because it is the shape of the
thing and not a defect: a `/sys/class/input/eventN` tree - `sysfs` builds its own
tree once at boot and has no runtime registration - so `udev`/`libinput` will
not classify a device made this way. That is enough for the conformance suite,
which opens the node directly, and not enough for a compositor; it is a `sysfs`
job rather than an input one. `UI_ABS_SETUP` and force feedback are not
implemented either: a synthetic device has no axes and answers `EVIOCGABS`
`EINVAL`, which is the honest answer for one whose geometry nobody described.

Writing the test found an ABI defect that had been there since the ioctls were
added, and this work fixed it: `EVIOCGNAME` answered with the length *including*
the terminating NUL, and a virtio device's name kept the config's own terminator
*inside* the string - so a caller that used the answer as a length counted a byte
that is not part of the name (a 20-character keyboard name came back as 22).
Linux's answer is `strlcpy`'s: the name is what stands before its first NUL, a
size of zero is the length query (answered without copying), and the answer is
that length *excluding* the terminator even when the copy had to be cut short,
which is how a caller with a small buffer learns the name did not fit. The
standalone arm that served sizes whose low byte is zero - the 256-byte buffer
`libevdev` passes - answered differently from the family handler, so it is gone
and one branch answers for every size. `t-evdev` pins the rule without
hardcoding any host's name: the answer must equal the position of the first NUL
in the buffer it wrote.

**Two more findings came out of the same boot, and both are about what a
*classifier* asks rather than about input.** `EVIOCGPHYS`/`EVIOCGUNIQ` answered
`ENOTTY` for the strings a virtio input device does not have, and `libevdev`'s
`libevdev_set_fd` tolerates a failure there only when the errno is `ENOENT` -
which is what Linux's own `str_to_user` returns for a NULL attribute - so the
wrong one made the whole read fatal, far from the ioctl that caused it: libinput
gave up with "failed to create input device" for *every* node on the machine,
and a compositor came up with a screen and no keyboard. And the
`/sys/dev/char/M:m` / `/sys/dev/block/M:m` links pointed at the class entry
rather than at the device that entry names; a reader resolves them to identify a
node, and `evdev_device_have_same_syspath` compares the *strings* a device's own
`fstat` devnum and the path udev gave it resolve to, so two answers that disagree
is a device libinput drops silently. They carry the *device tree's* path now
(`../../devices/virtual/...`), block devices excepted because this tree has no
device directory for them. `t-evdev` pins the two errnos; the sysfs side is
`src/vfs/sysfs.rs`'s own tests.

### 7.2b `/dev/dsp`: the OSS parameters belong to the writer — **DONE**

The virtio-sound driver used to decide everything at boot — S16LE, two channels,
44.1 kHz "or the nearest", and a header comment that openly said no userland
existed to ask. On Linux those numbers are *negotiated per open*, through the
`'P'`-typed `SNDCTL_DSP_*` family in `linux/soundcard.h`, and a program that
cannot ask cannot fall back either. The family now answers on `/dev/dsp`:

* `SPEED`, `STEREO`, `CHANNELS` and `SETFMT` (and `GETFMTS`, `GETBLKSIZE`,
  `GETOSPACE`/`GETISPACE`, `RESET`/`SYNC`/`POST`) — with the rounding rule read
  off the header's semantics: a rate that is not on the device's list moves the
  stream to the *nearest one it has* — nearest by relative distance, which is
  what `44100 → 48000` and `10000 → 11025` agree on — and the ioctl answers what
  the stream settled on rather than what was asked (`abi::oss_round_rate`,
  host-tested). A real reconfiguration is a stop → `SET_PARAMS` → prepare →
  start cycle on the device itself, and a refused one restores the old
  parameters and answers `EINVAL`.
* The refusals are answers too: `SETFRAGMENT`/`SUBDIVIDE` (the buffer geometry
  is this driver's `SET_PARAMS` constant today) and `NONBLOCK` (the hand-off
  blocks in the device; a flag that promised otherwise would be the lie this
  kernel's rules put above the feature) each answer `EINVAL` by name.
* Pinned by the request words' `_IOC` recomputation (host, `abi::oss_tests`) and
  end-to-end in the guest (`t-dsp`, including `fstat` showing a `chmod` that
  stuck).
  **This bullet was untrue when first written and is true only now** — see §12.4a. The
  self-test machine never registered `/dev/dsp`, because the driver searched for the sound
  card at the wrong PCI ID, so `t-dsp` failed wholesale from its first `open`: this entry
  claimed an end-to-end guest pin for a node that did not exist. The host `_IOC` pin was
  always real; the guest half landed with the ID fix and the user-address DMA fix that
  followed it.

### 7.3 AHCI/SATA (stretch) — **DONE: the driver, the second disk, and what measuring it corrected**

**Why**: real hardware support beyond virtio. Every block device this kernel had
driven hung off a virtio transport, so the storage path was only ever exercised on
a machine that already has the paravirtualisation the kernel needs for everything
else. A SATA controller is the other half of that sentence: a device with its own
register file, its own DMA rules and its own failure modes, on hardware QEMU
presents with one `-device`.

**What it is not**: it is not on the compositor path. §7.4's list is about pixels
and none of it touches storage. It is here because the kernel's answer to "what is
a disk" was *one driver wide*, and a second driver is what turns that answer into
a claim rather than an accident. Deliberately out of scope: NCQ, ATAPI, port
multipliers, hotplug and 28-bit LBA. One controller, one port, `IDENTIFY`,
`READ DMA EXT`, `WRITE DMA EXT`, `FLUSH CACHE EXT`.

#### A disk has a *name* now, and that is the part to settle first

`crate::uevent::Numbering::of_boot_disk` is the one function the whole system
agrees on for what a block device is called: `/dev` builds its nodes from it,
`/sys/class/block/*/dev` prints it, `/proc/partitions` lists it, and a uevent
listener matches an event against it. It is hard-coded - `vda`, major 254
(`VIRTIO_BLK_MAJOR`), the boot disk's partitions - and every one of those names
hangs under a hard-coded devpath, `/devices/virtio/block/<name>`.

That is right while there is one disk and wrong the moment there are two. Linux's
answer is the one to copy rather than invent, because it is the answer userland
already reads: a SCSI/SATA disk is `sda`, major **8**, minor 0, partitions at
1.., and `/dev/vda` and `/dev/sda` are two devices no program treats as synonyms.
A kernel that called a SATA disk `vda` would be publishing a private dialect in
the one place §3.3 cares about most - a name.

| | virtio-blk | AHCI |
|---|---|---|
| name | `vda`, `vda1`… | `sda`, `sda1`… |
| major | 254 (`VIRTIO_BLK_MAJOR`) | 8 (`SCSI_DISK_MAJOR`) |
| minor | 0 for the disk, 1.. for partitions | the same rule |
| devpath | `/devices/virtio/block/vda3` | `/devices/ahci/block/sda` |

So the work has two halves and the smaller one comes first, which is §4 rule 7's
own advice ("the refactor lands first, green on its own, then the change"): the
numbering becomes the numbering of *the disks this machine has*, built from the
block devices that actually came up, with the devpath a fact carried per disk
rather than derived from the name. That refactor changes nothing while there is
one disk - and it is testable anyway, because `sysfs`'s and `procfs`'s host tests
already build `Numbering` literals by hand, so a *second* disk's rendering in
`/proc/partitions` and under `/sys/class/block` can be pinned with no hardware at
all.

#### The driver, mechanically

- **Probe by class code, not by vendor.** `crate::pci::find` matches vendor and
  device ids, which is what a virtio device has and what a SATA controller does
  not: the class is `01`/`06`/`01` - mass storage, SATA, AHCI 1.0 - and
  `Device::class_code()` already computes exactly that. Enumerating
  `pci::devices()` and matching the class is the whole of the discovery.
- **The register file is BAR5** (`ABAR`), a memory BAR, so it is *not* in the
  direct map: `mm::map_mmio` is the existing window for exactly this, the same
  call `virtio`'s `Hal` makes for its own BAR. `COMMAND_MEMORY_SPACE` and
  `COMMAND_BUS_MASTER` have to be turned on first, as `VirtioBlk::init` does.
- **Bring-up is the spec's reset sequence**: set `GHC.AE`, clear `GHC.HR`, wait
  for it to clear, read `CAP`/`PI`, and for the first implemented port whose
  `PxSSTS.DET` says "device present, PHY up" (3) run `PxCMD.ST` off → `FRE` off →
  set `PxCLB`/`PxFB` → `FRE` on → `ST` on. Every wait is bounded, because this
  runs on the boot path of a machine that may have no such disk: a controller that
  never answers has to end in `None` rather than in a `hlt`.
- **Four aligned allocations**, each its own frames: the command list (1 KiB,
  1 KiB-aligned), the received-FIS area (256 B, 256-byte-aligned), one command
  table whose command header is 128-byte-aligned, and a page-sized bounce buffer
  for the data - the same single physically-contiguous page the [`BlockDev`]
  contract already promises every caller, so this driver is not the one that has
  to copy twice. `mm::alloc_frames` + `mm::phys_to_virt` is the pair `HalImpl`
  already uses.
- **One command at a time.** `PxCI` takes one bit; the driver waits for it to
  clear and *then* reads `PxTFD`/`PxIS` for the error bits rather than treating
  the bit clearing as success. An ATA error reports itself in `PxTFD.ERR` with the
  command already retired, so a driver that checked only the bit would call a
  failed read a successful one - the shape §4.2 forbids.
- **`IDENTIFY` is the discovery** (`0xEC`, the device's 256 words read into the
  bounce page): the LBA48 capacity in words 100..103 becomes
  `capacity_sectors`, and a device that will not identify itself is a plain
  `None` - there is no honest way to present one.
- **`flush` is `FLUSH CACHE EXT`** (`0xEA`), answering `true` only when the
  command completed without an error, which is what `BlockDev::flush` asks of a
  device that has a cache.

#### The machine, and the pin

The self-test machine boots its ESP through virtio-blk and must keep doing so: the
point of the SATA disk is that there are *two* disks, not that the boot moved. So
the test boot gains an `ich9-ahci` controller with one raw disk image attached
(`-device ich9-ahci,id=ahci`, a `-drive …,if=none`, and an `ide-hd` on `ahci.0`),
which is a change in `xtask/src/qemu.rs`'s `Boot::Esp` arm plus a file for
`xtask/src/selftest.rs` to make - not `machine.toml`'s `extra_args`, which would
leave the wiring invisible to the tree the gate is run from.

`main.rs` tries virtio-blk first and AHCI second, so a machine with either boots
and a machine with both has both. The pin is a guest program that opens
`/dev/sda`, reads sector 0, writes a sector and reads it back, checks
`/proc/partitions` names it with major 8, and checks that the capacity `IDENTIFY`
reported is the capacity the reads can actually reach - a capacity that is
*reported* but not *readable* being the failure a wrong `PxCLB`/`PRDT` setup
really has.

#### One thing that is a choice rather than a gap

The controller's interrupt is not wired to anything, and `BlockDev` has no
interrupt-driven path: the driver polls `PxCI`. That is honest rather than
temporary - a uniprocessor kernel driving one command at a time has nothing to do
while it waits - and it is the same choice `virtio`'s synchronous paths already
make.

#### What was built, and the two things the machine corrected

`src/drivers/ahci.rs` is the driver: probe by class code, BAR5 through
`mm::map_mmio`, the spec's reset sequence, stop-the-port-then-point-it-at-our-memory
bring-up, four aligned DMA pages, one command at a time through slot 0, and
`IDENTIFY`/`READ DMA EXT`/`WRITE DMA EXT`/`FLUSH CACHE EXT`. The numbering
refactor it needed landed first (`c1e5932`), `main.rs` brings up virtio-blk and
then AHCI so a machine can have both, and the test machine gets an `ich9-ahci`
with a 4 MiB disk beside its ESP. `t-ahci` is the pin - 16 checks over
`/dev/sda` - because every line of the driver is a volatile access to hardware
and no host test can reach any of it.

**Two things the first working version got wrong, both found by booting it, and
both worth a line because they are the kind of mistake the design could not have
predicted.** The first is `PxSIG`. The design said "refuse anything that is not
the ATA signature `0x00000101`", which is what the spec's table suggests. The
measured controller reports **`0xffffffff` on the port that has the disk on it**
until that port has been brought up, so the gate rejected every port and the boot
printed no SATA disk at all. The signature is not the presence question -
`PxSSTS.DET` is, and it said `0x113` ("device present, PHY up") correctly all
along - so the driver now refuses exactly one thing, the ATAPI signature
`0xeb140101`, and lets `IDENTIFY` decide what a disk is. That is also the shape
Linux's `ata_dev_classify` has: ATAPI, SEMB, PM and ZAC are named, and everything
else is a disk.

The second is the **unpartitioned disk**, which the numbering refactor deliberately
left as it was and flagged (§7.3 said the driver would answer it). The old rule
gated a disk's whole name on its having a partition table, so an unpartitioned
disk had no `/dev/sda`, no line in `/proc/partitions` and nothing a program could
open - including the partitioner that would have put a table on it. The test
image is 4 MiB of zeros with no table, so this arrived immediately: `/dev/sda`
did not exist and `t-ahci` failed at its first check. The rule is now the one
Linux has - the whole disk is numbered whatever is on it - with a host test in
`uevent` for the empty-table case, and that is why the numbering carries the
disk's own size rather than deriving a partition's.

#### The third thing the machine corrected: what the driver costs a boot

`cargo xtask run` stopped printing anything after `virtio-blk: vda, ...` for
**28.4 s** before the next line, which reads as a hang and is not one - the boot
completes. Measured on a copy of the image with the same wiring and a
timestamped serial log: the gap sits between `virtio-blk: vda, 4194304 sectors`
(`main.rs:374`) and `blk: no data partition on this disk` (`main.rs:418`), which
is `drivers::ahci::init()`'s window. Removing the device with
`-machine q35,sata=off` makes the same two lines **2 ms** apart, so the cost is
the SATA probe, and the QEMU monitor's `info pci` names the device: a plain
`-machine q35` already carries an ICH9 SATA controller at `00:1f.2`
(`8086:2922`) with nothing attached to it.

What the driver does with that controller, read out of an instrumented boot:

```
ahci-probe: 00:1f.2 bar=0x81084000/0x1000 GHC=0x80000000 CAP_PI=0x3f
ahci-probe:   port 0 SSTS=0x0    port 1 SSTS=0x0    port 2 SSTS=0x113  <-- DET == 3
ahci-wait: PxCI retire FAILED after 7067 ticks          = 28.3 s
ahci-dead: TFD=0x441 CI=0x1 IS=0x40000001 SIG=0xeb140101 SSTS=0x113 CMD=0xc017
```

Port 2 answers `PxSSTS.DET == 3` - "device present, PHY up" - with no device
behind it, the driver brings it up, issues `IDENTIFY`, and **`PxCI` never
retires**. It did not need to be waited on: the same read shows `PxIS.TFES` set
(bit 30 of `0x40000001`) and `PxTFD.ERR` set (bit 0 of `0x441`), i.e. the port
answered "that command failed" on the first read and the wait was looking only at
the retirement bit, which is what `identify`'s own error check - at
`drivers/ahci.rs:274`, *after* the wait - would have seen immediately. The
`PxSIG` had even become `0xeb140101`, the one signature this driver says it
cannot drive; it is read before bring-up, when the same port still reads
`0xffffffff`.

So two faults, one of them about the number:

1. **The wait asked one register a question two registers answer.** Fixed in
   `crates/ahci` (the `mpos-ahci` crate; `drivers/ahci.rs` is behind
   `#[cfg(target_os = "none")]`, so a decision inside it has no reachable test -
   the `pci`/`drivers::pci` and `evdev`/`drivers::evdev` split again):
   `PortStatus::waiting` stops on *either* answer, retired or refused. Five host
   tests carry the measured words above, and one of them - the phantom port -
   fails with the old `PxCI`-only rule.
2. **`WAIT_TRIES = 5_000_000` was a count nobody had costed.** Its comment claimed
   "tens of milliseconds ... on a dead one"; measured, one volatile MMIO read of
   an emulated register under KVM is ~5.7 µs, so five million of them is 28.3 s.
   The count shape stays, and the reason is now written beside it rather than
   assumed: the identical wait runs while a mounted filesystem reads through
   `BlockDev`, which holds the port inside `critical_section::with`, so
   interrupts are masked and the 250 Hz tick counter does not advance - a
   deadline in ticks there would never expire, while reads always do. What a
   count buys therefore depends on what a read costs, on this machine ~1000×
   more than on silicon, which is the honest weakness of the shape and is the
   reason the budget is no longer the only bound.

Measured after, same wiring on an independent copy of the image: **28.412 s → 0.001 s**, and
`t-ahci === PASS` on a machine that does have a disk, where a healthy command
retires at try 0-2 (also measured), so the patience that is left is not being
spent. **Why no test ever caught this:** `xtask/src/qemu.rs:168` gives the
selftest machine an `ich9-ahci` *with* a disk at a lower BDF than the chipset's
own, and the controller loop returns the first disk it finds - so the built-in
empty port is never probed there, while `cargo xtask run` boots a machine whose
only SATA controller *is* the empty one. A machine shape no test machine has.

**The two gaps the first version of this fix left, both closed in the same
investigation, both measured:**

- **A request longer than one page panicked the driver.** `udevd` asking a new
  SATA disk what is on it reaches `AhciDisk::read_at` with **262144 bytes**, and
  a 6144-byte unaligned superblock probe reaches it with **6656**; both ended the
  machine on `check_span`'s assert, because the block node's read hands the
  driver the *whole* covering window of the caller's request
  (`vfs/devfs.rs::DevBlockNode::read_at`) and virtio-blk has always accepted any
  sector-aligned length. The one-page promise was this driver's own, never
  `BlockDev`'s, and §3.3 says whose limit it therefore is: the driver's.
  `AhciPort::issue` now builds **one PRDT entry per bounce page** - a command
  carries `BOUNCE_PAGES` = 32 pages, 128 KiB, and a longer request is more
  commands, planned by `crate::ahci::chunks` - so `PRDTL` says how many runs the
  device fills and the total goes in `PRDBC`. `check_span` keeps only what is
  still a contract, the sector boundary on both offset and length, which is the
  pair virtio-blk asserts too. The pin is `t-ahci`: a 16 KiB write and read
  (four runs) and then **one `pread` of 1 MiB** - eight commands, 32 runs each -
  checked against a pattern that is a function of the byte's *index*, because the
  failure this exists to catch is a table whose completion bit is on every
  rather than only the last descriptor: that one reads back perfect with three
  quarters of the buffer unwritten, and a uniform pattern would not notice. The
  `DBC` encoding and the plan are host-tested in `crate::ahci`, including the run
  a 22-bit count cannot say (refused, not truncated).
- **A bring-up that failed kept its memory.** `release()` gives back the command
  list, the FIS area, the command table and the bounce, and it calls `stop()`
  first: a controller still running its engine reads a list that has gone back to
  the allocator. This is not a tidy-up on a hypothetical machine - the `q35`
  controller at `00:1f.2` walks exactly this path on every boot of `cargo xtask
  run`, so the phantom port used to cost 35 pages per boot, and would have cost
  35 more for each dead port a bigger controller has.

**And one claim this section made about the same scratch boots, retracted.** A
qcow2 overlay with `build/mpos-uefi-x86_64.img` as its backing file stopped
printing after `mpos: handing the machine to /init`, and it did so with the
pre-fix driver and with `-machine q35,sata=off` as well, which looked like a
userspace stall worth naming as a separate item. It is not one: **the image's own
guest was running while the overlay read it.** The run wiring opens its disk with
`file.locking=off`, so nothing stopped a second QEMU from taking a live
filesystem as its backing file, and what that boot froze on was another machine
writing underneath it. The same image copied to an independent qcow2 with
`qemu-img convert` and booted on the same wiring with the same kernel runs the
whole way: `Seeding 4096 bits and crediting`, `Saving 4096 bits of creditable
seed for next boot`, `[ ok ]`, no `[ !! ]`, and no `EOPNOTSUPP` anywhere on the
wire. Kept rather than deleted for two reasons: a scratch overlay of an image that
is *also* booted is not a look at that machine but a fight between two of them,
and the bisect that ruled the block layer out (`sata=off`, old driver, same
freeze) is exactly what made the wrong conclusion look earned. A later clean
check - `-machine q35,sata=off` on a freshly converted copy - reaches
`mpos login:`, which is the sentence that ends it.

**What the failed port is allowed to take with it.** `release()` gives a dead
port's pages back only after `stop()` has seen the engine and the FIS receiver
come down. That conditional is not caution for its own sake: a port whose `CR`
never clears still holds `PxCLB` and `PxFB`, so freeing those frames hands the
buddy system a page the *device is writing to*, and the corruption surfaces as a
bad page-cache block or a process's altered data far from here, with no line
pointing back. The measured case stops cleanly (`PxCMD` quiet, `PxCI` empty), so
the leak is the branch and not the rule; the rule is that the branch must not
free. An abandoned attempt to force the point - `SCTL.DI`, re-initialising device
detection to make a stuck port let go - was written, measured as never needed on
this machine, and removed again rather than kept as untested code.

**Gated**: host `629 passed`, `mpos-xtask 36 passed`, guest
`=== SUMMARY pass=132 fail=0 (132 built)` with QEMU exit 33, and `t-ahci === PASS`
over its wide transfers. End to end, on two fresh `qemu-img convert` copies of
the shipped image booted on the run machine's own wiring:

- the shape `cargo xtask run` boots, whose only SATA controller is the empty
  built-in one - `Seeding 4096 bits and crediting`, `Saving 4096 bits of
  creditable seed for next boot`, `mpos login:`, no `[ !! ]` and no
  `Unable to seed` on the wire;
- the same with a real 64 MiB `ide-hd` on an added `ich9-ahci` - `ahci: sda`,
  then the same seedrng lines and the same login prompt, and **no panic** where
  `udevd`'s 262144-byte probe used to end the machine.

One loose end, recorded rather than tidied: a single boot of the empty-controller
shape stopped after `Seeding 4096 bits and crediting` and printed nothing more for
45 s. It has not reproduced since - four later boots of that shape, and two of the
disk-attached shape, all reached the login prompt - so it is an unreproduced
observation and not a diagnosis, and the `release()` rule above is a safety
measure argued from what a stuck engine owns, not a fix claimed for it. If it is
seen again, the thing to read first is whether that port's `PxCMD.CR` came down.

---

### 7.4 The next compositor: niri, wlroots, and the renderer families — **ANALYSIS**

§7.1–§7.2 answer *weston*: DRM/KMS to present on, evdev for input, `memfd` +
`SCM_RIGHTS` for buffers, and Mesa's software GL through GBM and EGL. That is one
family, and a narrow one - the wlroots architecture with its GL renderer. This
section is what the *other* families want, read off their sources and the kernel
UAPI rather than measured here, and it says so: nothing below has been executed
on the machine, which is why it is a plan rather than a claim. The two
architectures fail for different reasons, and the surprise is which one is cheap.

#### What wlroots' pixman route already has

`WLR_RENDERER=pixman` on the DRM backend never creates an EGL context at all;
`wlr_allocator_autocreate` walks gbm → shm → **drm dumb** → udmabuf and, with no
GBM to be had, settles on the dumb allocator. Its requirements are short and the
card meets every one: `/dev/dri/card0` opened and `SET_MASTER` held (its
`drmIsMaster` check), the `DUMB_BUFFER` capability, `CREATE_DUMB`/`MAP_DUMB`/
`DESTROY_DUMB` with an `mmap(MAP_SHARED)` over the result,
`PRIME_HANDLE_TO_FD` to hand that buffer on, and `ADDFB`/`PAGE_FLIP` or
`MODE_ATOMIC` to put it on the screen. Formats are `LINEAR` or `INVALID` only,
which is what this card produces; `EDID`, modifiers and leases are each optional
to it, and the empty DRM lease it prefers for reopening a node falls back to a
plain `open` on `EINVAL`/`EOPNOTSUPP`, by its own source. So
`WLR_RENDERER=pixman sway` is the cheapest second-compositor experiment there is,
and it should be the first thing this plan does.

What it also needs is not a kernel object: `$XDG_RUNTIME_DIR`, libinput's udev
tagging (present, §7.2), and a seat. `seatd` is in the image and enabled, the
card arrives from the seat manager as a descriptor passed over its own unix
socket - `SCM_RIGHTS`, which this kernel already has (`t-unix-scm`) - and weston
takes its card that way today, which is the model to follow. All of it is rootfs
and init work.

#### What niri needs, and the two things that still stop it

**Where it stopped first, measured.** niri 26.04, booted in the guest, does not
reach the renderer at all - it dies in the TTY backend's setup with

    thread 'main' panicked at src/main.rs:186:6:
    called `Result::unwrap()` on an `Err` value: error initializing the TTY backend
    Caused by:
        0: error opening the primary GPU DRM node
        1: the provided file descriptor does not refer to a DRM node

which is `DrmNode::from_path("/dev/dri/card0")` failing its *identity* check.
niri's own helper had already found the card through eudev, and the node itself
was fine - the right path, an `st_rdev` of 226:0 - because the `drm` crate on
Linux asks sysfs a second question:

    let path = format!("/sys/dev/char/{}:{}/device/drm", major(dev), minor(dev));
    stat(path.as_str()).is_ok()

That is: the node's parent GPU, and the parent's `drm/` directory, whose entries
are the node *names* of one GPU. This tree had everything about the card except
that chain - the node, its class entry, the number's index - so a perfectly good
card was refused. It now has it (`src/vfs/sysfs.rs`: each node's `device` link,
and the parent's `drm/{card0,renderD128}`), which is also the directory
`node_with_type(NodeType::Render)` reads to hand niri `/dev/dri/renderD128` for
`NIRI_PRIMARY_GPU`-free startup. Worth knowing when reading a compositor log:
libdrm's test for the same question is weaker - `fstat` plus a look for
`/dev/dri/card*` - which is why weston never noticed the difference.

**Where it stopped second, measured the same way.** A second boot of niri 26.04,
on a kernel carrying that chain, stopped on the same line of the same panic. The
tree was right - `/sys/dev/char/226:0`, its `device` link, the parent's
`drm/{card0,renderD128}` listing - and the card was still refused as "not a DRM
node", because resolving that path is not one lookup: `226:0` is a link to
`../../devices/virtual/drm/card0`, whose `device` is a link to `../../virtio-gpu`,
and the second link's two `..` have to reach the directory that names `card0` -
a node four levels down. The walk answered `..` by *searching* the tree for the
parent (`Vfs::parent_of`), a walk bounded at 512 directories that returns nothing
when the bound is reached. On the guest's root filesystem - 483 directories at
depth four or less, before the pseudo-filesystems are counted - that budget is
spent before the search gets that deep, so the `..` stood still, `virtio-gpu` was
looked up inside `card0`, and the answer was `ENOENT`. Measured by the host test
that now pins it: the same chain resolves with 480 directories under the root and
fails at 500. The hole was that a splice threw the walk's chain away (`base =
here`) and re-derived it by searching; it now keeps it, and the search is left
where it belongs - a `..` whose base is a cwd, whose ancestors nothing has
walked. niri has not been booted since *that* fix either, so what it meets next
is again read off its source rather than measured, and it is the renderer below.

> **Update, measured (2026-10-01): it is not the renderer next, and the two
> paragraphs after this one are not where it stops.** Rebooted with the sysfs
> chain in place, niri gets past the DRM-node identity check and reaches
> `niri::backend::tty: using as the render node: "/dev/dri/renderD128"` - and
> stops there, *before* any EGL device is created, so no
> `software EGL renderers are skipped` line is ever printed and the process does
> not exit with an error. It stays alive in state `R` with **zero voluntary
> context switches**: a tight loop, never a wait. What it loops on is the Wayland
> display socket's lock file - the same unit forever on the same name:
>
> ```text
> stat("/run/user/0/wayland-1.lock", ...) = 0
> open("/run/user/0/wayland-1.lock", O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC, 0660) = 22
> fcntl(22, F_SETFD, FD_CLOEXEC) = 0
> close(21) = 0
> flock(22, LOCK_EX|LOCK_NB) = 0
> fstat(22, {st_mode=S_IFREG|0640, st_size=0, ...}) = 0
> ```
>
> `/run/user/0` holds exactly one file, `wayland-1.lock`, zero bytes, with no
> `wayland-2.lock` and no socket: the name never advances, so this is not a scan
> for a free `wayland-N` but a **lock-and-verify retry that never converges**,
> and the verify is the `fstat`/`stat` pair. Two measurements say the loop is the
> caller's and not libwayland's: the open carries `O_TRUNC` and is followed by an
> explicit `fcntl(F_SETFD)`, which libwayland's `wl_socket_lock` does not do
> (checked against `wayland-server.c` 1.26.0 *and* by disassembling the guest's
> own `libwayland-server.so.0.26.0`); and **niri links no libwayland at all** -
> `readelf -d` lists `libinput`, `libseat`, `libgbm`, `libudev`, `libpixman`,
> `libxkbcommon` and `libc`, and no `libwayland-*` - so it uses
> `wayland-backend`'s pure-Rust server, which is where the loop lives.
>
> **The bug it was spinning on is this kernel's, and it is fixed.** `stat(path)`
> and `fstat(fd)` disagreed about `st_dev` for one and the same file: measured by
> `t-stat-consistency` on `/tmp/t-stat-consistency.lock`, `fstat` answered
> `dev=0 ino=669` where `stat` answered `dev=1 ino=669`. The inode agreed; the
> device did not. `stat` looks the filesystem up by path, and a descriptor cannot
> - and an inode does not name its own filesystem, so the fallback
> (`fs_id_of_inode`) can only match a mount by its *root* inode, which a plain
> file inside the mount is not. `File` therefore records the filesystem id at
> `open`, where the path is known, and `fstat` prefers it; both doors now answer
> the same pair. Pinned by `user/abitest/src/bin/syscall/t-stat-consistency.rs`
> (the two doors, the `O_CREAT|O_TRUNC|O_RDWR` order a lock protocol uses, and
> both `/tmp` and `/run/user/0`).
>
> **The three further defects the diagnosis raised are now settled - one not a
> defect, one fixed, one re-attributed.** First, `/proc/<pid>/fd` readdir does
> *not* skip descriptors: built to the question, `t-fd-list` opens a contiguous
> run of twelve descriptors and finds every one of them in the listing, along
> with the three standard ones. The compositor's `0, 1, 17, 18` was a process
> that had opened and closed `2..=16`, which is what a compositor's startup
> looks like - so the listing is a pin now and the observation is withdrawn.
> Second, the kernel panic **is** a defect and is fixed: `pf_diag` prints the
> faulting instruction's own bytes, and it read them at `rip` *without asking
> whether that page was mapped* - and on an instruction-fetch fault `rip` is
> the faulting address itself, so the read faulted again from ring 0, at the
> same `CR2` with the U/S bit now clear, and the machine died of
> `KERNEL PANIC: #PF` while describing a *user* fault. The read is guarded by
> the same `leaf_pte` question the rest of the diagnostic asks, and the message
> now says when the bytes are not there to print. Measured before and after by
> `t-segv`, which jumps into a `PROT_NONE` page: the run that used to panic now
> prints `PFDIAG ... bytes=[] (empty: the page holding rip is the absent one)`,
> the child dies of `SIGSEGV`, and the parent - and so the machine - runs on.
> That program also faults the common way (a store through a null pointer), so
> a fix that refused to read *any* instruction bytes would fail it. Third, the
> non-root `ENOSYS` is not a kernel gap: the guest's `libseat.so.1` contains no
> `syscall` instruction at all (every call goes through libc), and its backends
> are `noop`, `seatd` and `logind` - there is no `builtin` - so "Function not
> implemented" is libseat's **noop** backend, the one it falls back to when no
> seat daemon answers. The image's own configuration is right (`seatd` is in
> `/etc/runlevels/default`, runs `-g seat`, and `mpos` is in `seat` in
> `/etc/group`), so the next step is to watch the daemon rather than the kernel:
> `rc-service seatd status`, `ls -l /run/seatd.sock` and `id mpos` on a boot
> where the non-root run fails.
>
> What has *not* changed is the conclusion two paragraphs below: a non-software
> EGL device still means `virtio-gpu` 3D, and `DRM_VIRTGPU_*` is still the
> largest single item in this section. What the fix above buys is that niri gets
> past its socket setup to the place that check lives, instead of spinning
> forever before it. **And that item is reachable on this host**, which was
> worth checking before calling it out of scope: QEMU here (11.1.1) carries the
> 3D device under a *different name* - `virtio-gpu-gl-pci`, with `virtio-vga-gl`
> and a rutabaga/gfxstream variant beside it - and
> `qemu-system-x86_64 -display egl-headless,gl=on -device virtio-gpu-gl-pci`
> starts, so the host can present a virgl-capable GPU. The `virtio-gpu-pci` the
> machine boots today has no `virgl` property at all, so 3D is a *machine* change
> plus a kernel one: `machine.toml` would name the GL device and a GL display
> backend, and the kernel would grow the `DRM_VIRTGPU_*` family and the 3D
> command queue behind it. That is a feature rather than a defect - it is why it
> is not in the fix above - but it is not blocked by the host.

> **Update, measured (2026-10-01, the same boot one call further on): the stop
> after the lock file was `listen(fd, -1)`, and this kernel answered `EINVAL`
> where Linux answers 0.** With the `stat`/`fstat` pair agreeing, the
> lock-and-verify loop converged, the name was taken, and niri died one line
> later - still `src/niri.rs:2435`, now on the socket's own error:
>
> ```text
> thread 'main' panicked at src/niri.rs:2435:67:
> called `Result::unwrap()` on an `Err` value: Io(Os { code: 22, kind: InvalidInput, ... })
> ```
>
> That line is `ListeningSocketSource::new_auto().unwrap()`, the `Err` is a
> `BindError::Io`, and the failing call is inside wayland-server 0.31's
> `ListeningSocket::bind_absolute`, which after the lock loop does exactly three
> things: `stat` the socket path, `UnixListener::bind`, `set_nonblocking`. Rust's
> std spells the bind as `socket(AF_UNIX, SOCK_STREAM)` + `bind(fd, addr, 2 +
> strlen + 1)` - 24 bytes for `/run/user/0/wayland-1`, not the
> `sizeof(sockaddr_un)` of 110 a C caller passes - and then `listen(fd, -1)`,
> because on Linux it is the *kernel* that caps a backlog. Measured in the guest
> with a twenty-line C probe making the same calls: `bind(..., addrlen 24)` ok,
> `listen(..., -1)` **`EINVAL`**, `listen(..., 0)`, `(..., 128)` and `(..., 4096)`
> all ok. So the short `addrlen` was not the problem and the negative backlog is
> the whole of it.
>
> `clamp_backlog` refused a negative backlog on purpose ("negative is `EINVAL`"),
> and that reading of the rule is wrong in the one place that matters: Linux's
> `__sys_listen` compares the argument to `SOMAXCONN` as an *unsigned* int, so
> every negative lands on the ceiling instead of being refused. Measured on this
> host before writing it: `listen(fd, -1)` returns 0 for `AF_UNIX` and `AF_INET`,
> and `ss -lx`/`ss -ltn` then report the backlog as 4096 - `net.core.somaxconn`,
> not the zero a "negative means a closed queue" reading would leave. Nor is it a
> corner case anyone had to go looking for: `-1` is what Rust's std passes from
> `UnixListener::bind` *and* `TcpListener::bind`. The rule lives once now
> (`src/net/unix.rs::clamp_backlog`) and the IPv4 pool reads it too - that arm had
> read `-1` as **zero**, a queue one deep, the same defect in the other family's
> clothes. §4.1 carries the rule; `t-unix` and `t-tcp-listen` pin it, each shown
> red on the kernel built before it.
>
> **Where niri stops now, measured the same way.** Rebuilt and booted with the
> fix, `ListeningSocketSource::new_auto()` returns and the two lines that were
> never reached appear:
>
> ```text
> INFO niri: listening on Wayland socket: wayland-1
> INFO niri: IPC listening on: /run/user/0/niri.wayland-1.11.sock
> ```
>
> `/run/user/0` holds `wayland-1` as a real socket (`srwxr-xr-x`, plus its lock
> file and that IPC socket) where the boot before left a zero-byte lock and
> nothing else. The next stop is a *different* call, one line above those in the
> log:
>
> ```text
> WARN niri::backend::tty: error adding primary node device, display-only devices
> may not work: DRM access error: Error loading resource handles on device
> `Some("/dev/dri/card0")` (Bad address (os error 14))
> ```
>
> `EFAULT` from the primary node's resource-handle load, and niri is gone within
> the twenty seconds the probe watched (the renderer refusal below is the rest of
> that story).
>
> **That call was this kernel's, and it is fixed.** `Error loading resource
> handles` is smithay's wrapper around drm-rs's `resource_handles()`, which is
> two calls to `DRM_IOCTL_MODE_GETRESOURCES`: a *sizing* call whose pointers are
> all null and counts all zero, then a *fill* call whose pointers come from the
> `Vec`s reserved at the counts the first call reported (`drm-ffi`'s
> `get_resources`). An empty `Vec`'s `as_mut_ptr()` is Rust's dangling non-null
> pointer - address 4 for `u32` - so the fill call carries `count_fbs = 0` beside
> a non-null `fb_id_ptr`, and this kernel read that pointer as "somewhere the
> caller asked me to write" and answered `EFAULT`. The card has no framebuffers
> before its first commit, so `count_fbs` is 0 on *every* first enumeration:
> nothing about the card was wrong, the call was.
>
> Linux's rule, measured on this host first with a C probe that makes both calls:
> the sizing call answers 0 with the counts (`fbs=0 crtcs=4 conns=8 encs=13` on
> the host's card), and the fill call answers 0 as well - with `fb_id_ptr = 4`
> and `count_fbs = 0` - because its copy sits inside the count test, so a
> zero-length array is never dereferenced. `get_resources`' two arms skip an
> empty array the same way now (`!fbs.is_empty()`, `!ids.is_empty()`), and the
> *poisoned pointer* rule is untouched: a count that asks for a real id at an
> address that is not the caller's is still `EFAULT`, which is what the
> `t-drm-args` check beside it pins from the other side.
>
> Pinned by `t-drm`, whose own enumeration does the two-call idiom against a card
> that has no framebuffer yet - the only state in which this can be observed at
> all, which is why `t-drm-args` cannot carry it (by the time that program runs,
> `t-drm` has created one, so its `count_fbs` is 1 and the arm never fires). The
> kernel built before the fix prints `FAIL: the fill call succeeds with an empty
> array beside a dangling pointer (got -14, want 0)`; the one after it passes.
>
> **The call after that one is the property list, and this time the failure was
> in the caller.** With the primary node added, niri died one line later - no log
> message, a panic in drm-rs itself:
>
> ```text
> thread 'main' (28) panicked at .../drm-0.14.1/src/control/mod.rs:497:29:
> index out of bounds: the len is 0 but the index is 0
> ```
>
> That line is `match values[0]`, which drm-rs evaluates as soon as a property's
> flags carry `DRM_MODE_PROP_OBJECT` - it reads the property's single value to
> learn *what kind* of object the property names. Linux gives every `OBJECT`
> property exactly one such value (`drm_property_create_object` stores the type
> tag), and this kernel had the planes' `FB_ID` and `CRTC_ID` advertising an
> empty list, with the connector's `CRTC_ID` carrying a lone `0` - a value drm-rs
> maps to `ValueType::Unknown` rather than to `CRTC`. So the properties were
> never *wrong* enough to fail a caller's check; they were wrong enough to make
> one index off the end of its own array.
>
> Measured through libdrm on this host's card before writing it: the connector's
> `CRTC_ID` and both planes' `CRTC_ID` answer `count_values = 1`,
> `values[0] = 0xcccccccc` (`DRM_MODE_OBJECT_CRTC`), and a plane's `FB_ID`
> `0xfbfbfbfb` (`DRM_MODE_OBJECT_FB`) - `0x40` (`OBJECT`) in the flags of all
> three, with `0x80000000` (`ATOMIC`) beside it. The three properties now carry
> their tag, and `OBJECT_FB` joins the three tags the file already had
> (`DRM_MODE_OBJECT_*` out of `<drm_mode.h>`, all four now matching it).
>
> Pinned by `t-drm-props`, which asks each of the four by name and checks flags,
> count and value. Counterfactual, measured in a worktree at the same commit:
> the kernel built before the fix fails it, the one after passes.
>
> **Named, not fixed: the `ATOMIC` flag.** Those properties are
> `ATOMIC | OBJECT` on Linux, and Linux *hides* them from a client that has not
> set `DRM_CLIENT_CAP_ATOMIC`; this kernel accepts the cap and stores nothing, so
> the flag is absent and the properties are shown to every client. Closing it
> honestly means the cap becoming per-file state and the property lists filtered
> by it - a change nothing yet asks for (niri sets the cap), so it is written
> down here rather than half-done.
>
> One instrument note for whoever takes it: `strace -f niri` ended *two* earlier
> boots in `KERNEL PANIC: #GP` from ring 0 (err `0x0`, `rip 0xffffffff8004b01f`,
> with `0xffffffff8013aede` at `rsp+0x0c0`), while `strace /bin/true` in this
> boot exited 0. So it is not "any ptrace on this kernel" - it is something a
> traced compositor reaches, and it is a defect of this kernel and not of niri: a
> user process must not be able to panic the machine. Not diagnosed here;
> recorded with the two boots that saw it. The same boot also logs
> `error spawning "waybar": Os { code: 12, kind: OutOfMemory }`, which is not
> what a missing binary answers (`ENOENT`) and is worth a look of its own.

niri's TTY backend is `GpuManager<GbmGlesBackend<GlesRenderer>>` and nothing
else - there is no pixman renderer in it - and, worse for this kernel, it
*rejects* the renderer that is available:

    ensure!(!egl_device.is_software(), "software EGL renderers are skipped");

So on llvmpipe the render node is left `None`, no primary renderer is ever
created, and `connector_connected` cannot obtain one: there is no output at all.
That is a deliberate upstream choice (software EGL devices "segfault on importing
dmabufs from other renderers"), not a bug to route around, so **niri here
requires a hardware-style GL device**: `virtio-gpu` 3D. The `DRM_VIRTGPU_*`
family - `GET_PARAM`, `RESOURCE_CREATE`, `RESOURCE_INFO`, `SUBMIT_3D`,
`EXECBUFFER`, `MAP`, `TRANSFER_{TO,FROM}_HOST`, `CONTEXT_INIT`, resource blobs -
is what virgl (and venus, for Vulkan) is built on, and it is the largest single
item in this section.

> **Update, measured (2026-10-02): that item landed, and what stops eglinfo is
> not it.** The 3D half is answered — `VIRGL` and `CONTEXT_INIT` negotiated, the
> `CTX_*`, `RESOURCE_CREATE_3D`, `SUBMIT_3D`, `TRANSFER_*_3D` and `GET_CAPSET*`
> commands dispatched, the cap sets cached at bring-up, `EXECBUFFER` and
> `TRANSFER_FROM_HOST` made real — and on the GL machine the driver states the
> result in its own words:
>
> ```text
> virtio-gpu: virgl=1 context_init=1 capsets=2 cached=2 mask=0x6
> ```
>
> Every number there is the one the Linux measurement below produced, and
> `cached=2` is the one that carries weight: it is both the `GET_CAPSET_INFO` and
> the `GET_CAPSET` round trip completing. It is not enough to produce a renderer,
> and the reason is a *different* hole, measured the same way on
> `virtio-gpu-gl-pci` with `-display egl-headless,gl=on`:
>
> - **every EGL platform eglinfo can initialise reports software.** `eglinfo -p
>   surfaceless` and the device and GBM platforms answer `OpenGL renderer string:
>   llvmpipe (LLVM 23.1.2, 128 bits)` — the software rasteriser — on a card with
>   3D behind it.
> - **Mesa's loader says what it wanted, and did not get it.** It prints
>   `MESA-LOADER: failed to retrieve device information` once per lookup, and the
>   message list inside the shipped `libEGL.so.1` names the two other ways out:
>   `MESA-LOADER: device is not located on the PCI bus`, and `failed to get
>   driver name for fd %d` / `DRI2: failed to get driver name`.
> - **The card's parent device answers nothing.** `/sys/dev/char/226:0/device`
>   resolves to `/sys/devices/virtual/virtio-gpu`, and on it `vendor`, `device`,
>   `subsystem`, `driver`, `modalias` and `uevent` are *each* `ENOENT`; the
>   directory holds one entry, `drm/`. The chain below the node is right —
>   `/sys/dev/char/226:0/device/drm/` lists `card0 -> ../../drm/card0` and
>   `renderD128 -> ../../drm/renderD128`, each `dev` reading back `226:0` /
>   `226:128`, and `drmGetNodeTypeFromFd` answers `PRIMARY` for `card0` and
>   `RENDER` for `renderD128`. The *minor* is modelled; the *device* is not.
> - **libdrm agrees, and this is the sharpest number.** A twenty-line `gd` probe
>   against `libdrm.so.2` (2.134.0) answers `drmGetDevices2(0, devs, 16) = 0` —
>   it enumerates *no* device — where Linux returns the card; `kmscube` says the
>   same thing in words, `no drm device found!` then `could not open drm device`;
>   and `drmGetVersion(fd)` names the driver **`mpos`**, where Linux names it
>   `virtio_gpu`.
>
> **Two interpositions say what the fix is not.** Interposing `drmGetVersion` to
> return `virtio_gpu` (confirmed applied: the probe then prints it), and
> interposing `open`/`openat` to hand `device/vendor` and `device/device` the
> values Linux's card reports (`0x1af4` / `0x1050`), each changed *nothing* — the
> renderer stayed llvmpipe, and the loader kept refusing. So Mesa is not choosing
> on the version name, and not on the PCI ids alone: it is refusing a device
> whose parent chain has no bus identity at all — no `subsystem` link above all,
> which is what `devices/virtual/...` literally means. The direction of the
> remaining work is therefore the device model rather than the command queue:
> the card has to hang off a device that says what it is, as Linux's sits three
> levels down (`.../0000:00:01.0/virtio0/drm/card0`, with the PCI function's
> `vendor`/`device` and the virtio device's `subsystem`). Until that exists
> `is_software()` is true, no renderer eglinfo can report is anything but
> llvmpipe, and this section's first bar is unreachable however complete the 3D
> family is.

> **Update, measured (2026-10-02, the same day, one instrument later): the gate
> is libdrm's device information, not the sysfs files the loader reads
> directly.** The design the block above argued — the card needs a bus identity
> in sysfs — was tested rather than built, by a recorder that logs every `/sys`
> path a caller opens or resolves (`LD_PRELOAD` over `open`/`openat`/
> `readlink`; the alternative, `strace -f`, is the instrument this file already
> records panicking the machine). It says something narrower, and it says it
> twice:
>
> - **on the guest** the loader asks exactly three questions about the render
>   node, in this order — `open /sys/dev/char/226:128/device/vendor`,
>   `open …/device/device`, `readlink …/device/subsystem` — and all three
>   `ENOENT`;
> - **on this host**, whose Mesa is the same 26.2.3 and whose libdrm is the same
>   2.134.0, a working card (amdgpu) is asked **only for the `subsystem` link**:
>   `readlink /sys/dev/char/226:128/device/subsystem -> ../../../../bus/pci`.
>   The loader never reads `vendor` or `device` at all.
>
> So the guest is not missing a file the loader wants for its own sake; it is on
> a *fallback* path, taken because libdrm's `drmGetDevice2` — which is where the
> PCI id comes from on a working system — could not describe the card. That
> reading fits the library's own imports (`libgallium` pulls `drmGetVersion`,
> `drmGetDevice2`, `drmGetDevices2` and `drmGetNodeTypeFromFd` from libdrm; with
> udev built in, `drmGetDevice2` answers out of the *udev* database rather than
> by reading sysfs at run time) and fits the number this section already has:
> **`drmGetDevices2` returns 0**.
>
> Serving the three questions through the same preload settles it as a test.
> With `device/vendor` reading `0x1af4`, `device/device` `0x1050` and
> `device/subsystem` resolving to `/sys/bus/pci`, the renderer is **still
> llvmpipe**, and the loader asks nothing further under `/sys`. Two candidate
> gates are therefore ruled out by measurement instead of argument: the kernel
> driver *name* (`drmGetVersion` forced to `virtio_gpu`, confirmed applied — no
> change) and the per-node sysfs identity (served — no change). What is left is
> the layer underneath both: what `udev` was told about the card.
>
> **The design is re-opened on that, and the next measurement is named.** Dump
> the guest's udev database entry for `c226:0` and set it beside Linux's for the
> same card, then find the property libdrm's udev path needs and does not get.
> `DEVTYPE=drm_minor` is absent from the node's `uevent` today (measured: it
> carries `ACTION`, `DEVPATH`, `SUBSYSTEM`, `DEVNAME`, `MAJOR`, `MINOR` and
> nothing else), and `ID_PATH` and the `PCI_*` keys a udev rule would add have
> never been looked at. Nothing in the device model changes until that dump is
> in hand: the sysfs chain the block above proposed would have been built to
> satisfy a caller that is not the one failing.
>
> Two instrument notes, both machine defects, recorded because they cost this
> measurement three reboots. The guest's console — tty1, carried over the wire
> by the kernel's console mux, since `/etc/inittab` puts no getty on ttyS0 —
> **stops accepting input and stops emitting output after a session of heavy
> commands**, with the machine still running and `info status` still answering.
> And the 9p share is exported to a bus the guest enumerates — `0000:00:09.0` is
> there — while the boot prints `virtio-9p: no share on the bus`, so there is no
> file channel into the guest to work around the first defect with.

> **Update, measured and then built (2026-10-02, the same night): the chain the
> caller reads, and what this tree now answers.** The first recorder was
> incomplete, and that is why its answer was too: libdrm opens sysfs through
> `fopen64`, `open64`, `opendir` and `stat64`, and the first version hooked only
> `open`/`openat`/`readlink` - so *libdrm's own reads were invisible to it*, and
> what it showed was Mesa's fallback rather than the parse that fails. With
> those four doors covered, the same recorder run against a working card on this
> host gives the target, in this order:
>
> ```text
> opendir   /dev/dri
> stat64    /dev/dri/{card1,renderD128}
> open      /dev/dri/renderD128
> open64    /sys/dev/char/226:128/device/vendor
> open64    /sys/dev/char/226:128/device/device
> stat64    /sys/dev/char/226:128/device/drm
> readlink  /sys/dev/char/226:128/device/subsystem -> ../../../../bus/pci
> fopen64   /sys/dev/char/226:128/uevent
> fopen64   /sys/devices/pci0000:00/.../0000:65:00.0/{vendor,device,
>           subsystem_vendor,subsystem_device,uevent}
> ```
>
> The guest answers the first four and stops: `device/vendor`, `device/device`
> and `device/subsystem` are `ENOENT` there, because the node's `device` link
> lands on `/sys/devices/virtual/virtio-gpu` - a directory holding one child
> (`drm/`) and no attributes at all.
>
> **Built, and pinned by a host test that fails on the old tree**: the card now
> hangs off the display-class PCI function, so `device` leads to a directory
> that carries `vendor`, `device`, `subsystem_vendor`, `subsystem_device` and
> `uevent`; `add_pci_attributes` gained the `subsystem` link it never had (which
> is what libdrm resolves *before* it parses anything else, and gives up when it
> is missing); and the parent's `drm/` listing was moved with it. That last part
> had a bug worth recording: the listing's entries climb from the *parent*, one
> level deeper than the node's own, and leaving them at the old depth made them
> dangle - a listing whose entries resolve to nothing, which a reader walking
> `/sys` (udev does, at coldplug) follows into nothing. The test now resolves
> them, not merely lists them.
>
> **The section's first bar is still open, and this is the part not to paper
> over**: `eglinfo` has not been re-run on the GL card with this kernel. Every
> attempt died on the console defect above - a multi-kilobyte line is truncated
> mid-send, at 8 bytes every 60 ms as much as at 16 every 20 - so the run that
> would answer "non-software renderer" has not happened. What *is* measured: the
> tree answers every question the working card's log asks, a host test pins it,
> and the gate is green on a machine with a display-class GPU
> (`pass=140 fail=0`, exit 33).
>
> **And one more machine defect, found by bisection rather than assumed**:
> `udev failed to start` on this machine, and it is **not** this change - the
> same image built with the change stashed fails identically, and the gate never
> sees it because the gate boots an initramfs and not Alpine. It is its own
> named gap, and it is now the reason a *stock* boot reports no udev at all.

> **Update, measured (2026-10-02): the card is found now, and Mesa says where it
> still stops.** The identity chain was built and the acceptance was run - not
> over the console, which cannot be driven (below), but from a script in
> `/etc/local.d` that runs the probes at boot and leaves their output on the
> disk, where the host reads it with `qemu-nbd` and a mount. That technique is
> the instrument; the files were temporary and are not in the tree.
>
> With the chain in place, on `virtio-gpu-gl-pci`:
>
> ```text
> devpath=/sys/devices/pci0000:00/0000:00:04.0
> PCI_ID=1AF4:1050  PCI_SUBSYS_ID=1AF4:1100  PCI_SLOT_NAME=0000:00:04.0
> subsys=../../../bus/pci
> drmlist=card0 renderD128
> ```
>
> - **`kmscube` no longer says `no drm device found!`.** It initialises EGL 1.5,
>   prints its EGL and GL information, and is still running when a 25 s timeout
>   kills it (`rc=143`). The device is found, opened and driven, so that part of
>   Phase E's second bar moved.
> - **The renderer is still `llvmpipe`, and Mesa names the next gap itself**:
>   `MESA-EGL: warning: egl: failed to create dri2 screen`, then
>   `MESA-EGL: warning: DRI2: failed to create screen`, on the GBM platform and
>   on surfaceless. It says the same with `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`
>   forced - so the driver *is* reached and its **screen creation** is what
>   fails. That is the item now: not the identity, not the command family, but
>   whatever the virtio_gpu driver's screen creation asks for and does not get.
> - **A bug the first version of the chain had, worth the paragraph**: the card
>   hung off the *first* display-class function, and a q35 machine has the
>   firmware's own VGA (`1234:1111`) before the `virtio-gpu` it was given - same
>   class, so class alone chose the VGA. Mesa read it and said so:
>   `pci id for fd 3: 1234:1111, driver (null)`, the id of a display adapter it
>   has no driver for. The choice is now the function a driver bound to (the VGA
>   has none), pinned by a fixture that puts the VGA first - and **the honest
>   shape is the driver telling sysfs where its own device is rather than sysfs
>   guessing from class and driver**, which is owed.
> - **`DRM_IOCTL_VERSION` now answers the driver's name, `virtio_gpu`**, where it
>   answered `"mpos"`. Measured *not* to be the gate - with the name corrected
>   the renderer does not move - but landed anyway: Linux's `drm_version.name`
>   is `drm_dev->driver->name`, Mesa keys its kmsro table on it (`libgallium`
>   carries `virtio_gpu` beside `kms_swrast`), and a private spelling in a field
>   a caller identifies the driver by is the shape §3.3 forbids. `t-drm` carries
>   the new expectation.
>
> **And the console defect, sharpened by measurement.** It is not length: after
> the *first* command the receiver is dead. `^A 2` - the mux's own switch, which
> narrates `[tty2]` on the wire when it works - produces nothing at all, so the
> bytes never reach the mux; the forwarder is not stuck
> (`poll_schedule_timeout`, its normal select); and the machine is idle, not
> looping (`HLT=1`, `RIP` in kernel space). A machine that cannot be typed at
> after one command cannot be measured through, which is why the boot-time
> instrument above exists.

> **Update, landed: the two dumb-buffer divergences are closed lazily.** The table
> below measured three answers this card gave that Linux's does not, and two of them
> had one fix: `RESOURCE_INFO`'s `res_handle` was 0 for a plain dumb buffer where
> Linux names a resource, and `TRANSFER_TO_HOST` on one was `EINVAL` where Linux
> completes the copy. The reason the fix was deferred is in `crates/gpu/src/
> virtgpu.rs`'s header - making `CREATE_DUMB` create the device resource, as Linux's
> `virtio_gpu_mode_dumb_create` does, "would make every dumb buffer eagerly backed and
> add a device round trip to a path the console, weston and the fbdev node all
> allocate through". So the resource is made **lazily** instead, on the first request
> that addresses the handle as one (`drivers/drm.rs`'s `back_dumb_with_a_resource`):
> `RESOURCE_INFO` and `TRANSFER_TO_HOST` answer as Linux's do, and a caller that never
> asks pays neither the eager backing nor the round trip. That is a difference in this
> implementation and not in the ABI - the id is opaque and no request reports whether
> a resource exists without asking about it - and the pages materialised are the ones
> the scanout has been reading, so the transfer copies the pixels the caller drew.
> Pinned by `t-drm`, which now asserts a dumb buffer's `RESOURCE_INFO` is non-zero,
> that asking twice names the same resource, that the transfer completes, and that a
> handle this card never made is still `ENOENT`. The third divergence - a transfer
> whose rectangle does not fit the backing the device was given is refused where Linux
> would read past it - stays, and is a refusal rather than a different answer.

#### Measured: what Linux's card answers, request by request

The family is written from an observation rather than from the UAPI header,
because the part worth knowing is not in the header at all: **which requests a
card with no 3D refuses, and with which errno.** The instrument and the full
table lived in `user/staging/virtgpu-probe/` - that scratch tree is deleted, so
what remains of the instrument is this paragraph and the table below: Alpine's
netboot `virt` kernel in
a twenty-line initramfs, booted on `virtio-gpu-pci` and then on
`virtio-gpu-gl-pci` + `-display egl-headless,gl=on`. (`virt` and not `lts` for a
measured reason: the `virt` netboot initramfs already carries `virtio-gpu.ko`,
`drm.ko`, `drm_kms_helper.ko` and a `modules.dep` whose closure covers them, so
the 218 MB `modloop-lts` is not needed.) Linux 6.12.110-0-virt, QEMU 11.1.1.

Four things decide the shape, and each is a way a plausible design is wrong:

- **Nothing in the family is `ENOTTY`.** `virtio_gpu_ioctls[]` is registered
  unconditionally, so every request is *answered* even on a card with no 3D -
  and they are answered differently. On `virtio-gpu-pci`: `GETPARAM(1)`
  (3D_FEATURES) is `0`; `GET_CAPS`, `EXECBUFFER` and `TRANSFER_FROM_HOST` are
  `ENOSYS`; `CONTEXT_INIT`, `RESOURCE_CREATE_BLOB` and a 3D `RESOURCE_CREATE`
  are `EINVAL`; while `TRANSFER_TO_HOST`, `WAIT` and `MAP` **succeed**. One
  errno for the whole family would send a caller down a path it cannot recover
  from, which is §4's wrong-shape failure in its plainest form.
- **`CONTEXT_INIT` answers `EEXIST` on a virgl card**, because the driver
  creates a context *implicitly* on first use, so an explicit request arrives
  second. `GETPARAM(7)` reports the capset mask `6` (VIRGL and VIRGL2), and the
  device announces two cap sets: id 1 max-version 1 size 308, id 2 max-version
  2 size 1408.
- **`RESOURCE_CREATE`'s `target` is the switch.** Without 3D only `2` is
  accepted, with `depth`, `nr_samples`, `last_level` and `array_size` each
  capped at 1 (`EINVAL` otherwise); `4` is `EINVAL` there and succeeds with
  virgl.
- **`RESOURCE_INFO` answers `size = 4096`** for a 64x64 B8G8R8A8 resource
  created with `size = 0` - the driver's floor is a page, not `w * h * 4`.

What the instrument does **not** do is capture *Mesa's* own sequence: that wants
a full Alpine userland with `virtio_gpu_dri.so` and a tracer, not the twenty-line
initramfs, so "which request the winsys makes in which order" is still a source
reading rather than an observation here. The directory's README says so, so it
cannot be mistaken for measured.

#### The two decisions the kernel work starts from

With the measurement in hand the kernel work has two forks already settled, and
both are written down because each rejects an option that looks cheaper.

**The driver becomes this kernel's, not the crate's.** `DRM_VIRTGPU_*` is
carried by the virtio-gpu *control queue*, and that queue lives inside
`virtio_drivers::device::gpu::VirtIOGpu` - whose command set stops at `GET_EDID`,
which negotiates no `VIRGL` feature, and which exposes no accessor for the queue.
So the family cannot be added from outside the crate. The two ways in are (a)
vendor `virtio-drivers` 0.13.0 into `vendor/` with a `[patch.crates-io]`, keeping
the 2D path bit-identical, or (b) own the GPU driver in
`src/drivers/virtio/gpu_ctrl.rs` on the crate's *public*
`VirtQueue`/`Hal`/`Transport`, keeping the crate for every other device. **(b) is
the choice**, and the reason is measured rather than aesthetic: the kernel calls
exactly **four** things of that crate's GPU device - `new`, `resolution()`,
`change_resolution()` and `flush()` - and uses neither its EDID parsing (413
lines the DRM layer replaces with its own synthesized blob) nor its virtio cursor
queue (the DRM cursor is a real second plane, `drm_cursor.rs`). Vendoring ~13,000
lines to add ~400 would make every future crate bump a merge, while (b) puts the
new ABI facts in a host-testable module, which is where §4.6 wants them. What
(b) puts at risk is a working, pinned 2D path - and the pins (`t-fbdev`, `t-drm`,
the boot's own display probe) are what guard it.

**(b) has landed**, as the refactor §7's rule wants landed first: the same
commands in the same order, so the machine boots exactly as it did and the pins
above are the evidence rather than an assertion. `src/drivers/virtio/gpu_ctrl.rs`
owns the transport and the control queue; `crates/gpu/src/abi.rs` holds the wire
vocabulary and is host-compiled, with eight tests pinning the command numbers
against `<linux/virtio_gpu.h>`'s literals, every struct size, the header's field
offsets and the feature word. Three things narrowed deliberately rather than
carried over, each because the crate accepted something it never used: `EDID` is
no longer negotiated (the 413-line parser behind it was never called), the
cursor queue is no longer configured (nothing sends cursor commands - the DRM
cursor is a plane), and the feature word is now `WANTED_FEATURES` rather than
"everything the crate listed". The gate after it: `=== SUMMARY pass=137 fail=0
(137 built)`, QEMU exit 33 - the same line the tree printed before the change,
which is what a behaviour-preserving refactor owes.

What that leaves for the 3D work is additive: the commands, and the bits that
gate them.

**A 3D submission reaches the device through the render task.** The render task
takes the device out of `DEVICE` at its start and is its sole owner; that is what
makes `flush` single-caller, as `video/render.rs`'s own header says. A
compositor's `EXECBUFFER` runs on another task and so cannot touch it. The
pattern to reuse already exists and is the same shape: `request_flush_blocking`
is a request counter the task polls plus a parked-waiter list, and the 3D path is
that handshake generalized into a mailbox. The rejected alternative - handing the
device to the ioctl path under a shared lock - would make `flush` multi-caller
and put a control-queue round trip on the synchronous syscall path while the task
may be mid-flush.

**And the family cannot be half-served**, which is why this section is a plan and
not a landed change. The measurement makes it explicit: Linux's no-3D card
*succeeds* at `RESOURCE_CREATE` (target 2), `MAP`, `WAIT` and `TRANSFER_TO_HOST`,
and answers `ENOENT` only for handles it genuinely does not have. This card has
no device resources at all, so serving the refusals alone would hand `ENOENT`
back for handles a Linux card accepts and refuse a `RESOURCE_CREATE` Linux
completes - a *nearly right* family, which §4 says is worse than an absent one.
The family therefore lands whole, on top of the control queue, or it does not
land.

Two smaller blockers used to sit behind it, and **the first is no longer one:
threads are done** (`clone(CLONE_THREAD)`, §6.4 - a task on the caller's memory
and descriptors, with `CLONE_CHILD_CLEARTID` and a real futex wake for
`pthread_join`). That matters here precisely because a real compositor spawns
them: `tracy_client`'s profiler thread, `async-io` under the D-Bus feature, and
Mesa's own worker pools (`LP_NUM_THREADS`; one CPU makes llvmpipe's default
single-threaded, but the path exists, and Vulkan ICDs use it unconditionally).
**Explicit sync is the one that remains**: Smithay will
not expose `wp_linux_drm_syncobj_v1` at all unless `DRM_IOCTL_SYNCOBJ_EVENTFD`
answers, and its DRM plane config writes `IN_FENCE_FD` when a fence is present -
a property this card accepts and then ignores, with no `OUT_FENCE_PTR` beside it.
A compositor that never gets the protocol will not pass a fence, so nothing is
*wrong* today; it does mean a client's buffer is read with no synchronisation
primitive at all, which is the implicit-fence boundary `src/vfs/dmabuf.rs`
already names as v1's.

What niri does **not** require is worth as much as what it does, because each one
is work this plan does not have to do: no `EDID` blob (`make`/`model`/`serial`
become "Unknown", and the physical size is `unwrap_or((0, 0))`), no `non-desktop`,
`max bpc`, `panel orientation`, `vrr_capable` or `VRR_ENABLED` (each looked up by
name and defaulted, with a `debug!` at most), no gamma (`GAMMA_LUT` is tried,
then the legacy `MODE_SETGAMMA`, then given up on), and no DRM lease (the global
is created and no-ops without it). What it *does* need is for the properties it
finds by name to exist under those names - and the half of `GETPROPERTY` that
reports the name is what such an interface depends on and cannot fake. This card
answers the names it needs: `CRTC_ID`, `ACTIVE`, `MODE_ID`, `FB_ID`, `type`,
`SRC_*`/`CRTC_*`, `pixel blend mode`, `IN_FENCE_FD`, `DPMS`.

#### The renderer families, and what each asks of the kernel

- **llvmpipe** (software GL - the driver behind today's weston). The JIT needs
  anonymous memory made executable: `MAP_PRIVATE|MAP_ANON` and then `mprotect`
  `RW`→`RX` or straight `RWX`, which `mmap`/`mprotect` already allow. It wants
  threads for `LP_NUM_THREADS`, a writable `$XDG_CACHE_HOME/mesa_shader_cache`,
  and `madvise(MADV_DONTNEED)` to give its scratch back - which is real now
  (§5.2), so the scratch it releases is memory the machine gets back rather than
  a mapping that only grows.
- **lavapipe** (software Vulkan) is the same stack one layer up, plus the Vulkan
  loader and an ICD manifest: userspace work that puts `vulkaninfo`-class probing
  and a Vulkan-based renderer within reach on a machine with no GPU 3D.
- **virgl and venus** (virtio-gpu 3D, and Vulkan over it) are `DRM_VIRTGPU_*`
  again. This is the only path that makes `is_software()` false, so it is on
  niri's critical path - and on lavapipe's performance path rather than its
  correctness one. It is also the only item here that needs the GPU's own
  command stream rather than the card's bookkeeping.
- **pixman** (software 2D) needs a buffer and a mapping and nothing else, which
  is the whole reason the wlroots route above is the cheap one.

#### The gap, as a list

Devices and ioctls, roughly in the order the compositors reach for them:
`MODE_GETFB2` ✅; `CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` ✅ (refused, with the
reason); `MODE_CURSOR`/`MODE_CURSOR2` and `MODE_GETGAMMA`/`MODE_SETGAMMA` (the
legacy pair niri falls back to) - both ✅, the gamma pair refused with the reason
and the cursor pair driving a real second plane; the
`SYNCOBJ_*` family (`CREATE`, `DESTROY`, `HANDLE_TO_FD`, `FD_TO_HANDLE`, `WAIT`,
`SIGNAL`, `RESET`, `QUERY`, `TRANSFER`, `TIMELINE_WAIT`, `TIMELINE_SIGNAL`,
`EVENTFD`) with `DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE`; and
`DRM_VIRTGPU_*`.

**Four of those requests are answered now, and three of the four answers are
refusals - deliberately.** `MODE_GETFB2` (0xce) is a real implementation:
`struct drm_mode_fb_cmd2` goes in as an `fb_id` and comes back carrying the
geometry, the fourcc, the modifier flag and the four plane arrays, which is the
one thing libdrm's `drmModeGetFB2` exists for that `GETFB` cannot say. Two
details there are the same lesson this section already records twice. First, a
legacy `ADDFB` names its layout as bits per pixel plus depth because its struct
predates fourcc codes, so `GETFB2` on such a framebuffer reports a format the
card *derived*; `legacy_fb_format` derives it exactly the way
`drm_mode_legacy_fb_format` does for the two depths this card can scan out (24 ->
`XRGB8888`, 32 -> `ARGB8888`), which are the two `ADDFB2` already maps, so the
two doors report the same fourcc for the same pixels rather than two dialects of
one fact. Second, the three planes this card does not have are written as
*zeros*, not skipped: libdrm reads `handles`/`pitches`/`offsets`/`modifier` as
four-element arrays whose meaningful length is "how many are non-zero", so a
kernel that left them alone would describe a four-plane buffer over addresses the
caller never provided. `t-drm` pins the round trip, both spellings, and the zeroed
planes.

`MODE_GETGAMMA`/`MODE_SETGAMMA` (0xa4/0xa5) are refused `EINVAL`, and that is the
same decision as the `GAMMA_LUT`/`GAMMA_LUT_SIZE` absence above rather than a
separate one: there is no colour ramp to read or write. A caller is told the shape
before it asks, because `GETCRTC` now writes `gamma_size` 0 - it wrote nothing
there at all before, which left a `malloc`ed struct's garbage in a field libdrm
reads, the identical failure the mode-name comment in `write_mode` describes.
`EINVAL` is Linux's answer for a CRTC whose `gamma_size` is zero. **It is the one
number in this file taken from Linux's source rather than measured on a card, and
it is recorded as such:** no machine here has a CRTC without a gamma table to
measure - the amdgpu CRTC next door reports `GAMMA_LUT_SIZE` 4096 - and in the
3.7 tree `drm_mode_setgamma` answers `-EINVAL` to a `gamma_size` the CRTC cannot
hold and `-ENOSYS` only to a *missing callback*, a distinction a CRTC with no
table never reaches. A refusal rather than an accepted no-op, because `OK` from
`MODE_SETGAMMA` is a compositor believing it applied a colour transformation no
pixel will ever show.

`CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` (0x3b/0x3c) are refused `ENOTTY`, which
is §7.1's recorded answer for the other vblank door - `WAIT_VBLANK` and its
sequence facility - and the same argument decides both: both requests are about a
*clock*, and this card's only boundary is a frame it presented.
`drivers::video::render::landings` states the constraint in its own comment,
"Calling it a vblank counter would be a lie the ABI cannot tell", and `st.seq`
counts *commits* rather than periodic boundaries, so there is no truthful number
to put in the reply. Naming the two in the dispatch `match` is what makes the
answer this card's rather than the catch-all's; two spellings of "I cannot tell
you about vblanks" that disagreed would be the private dialect `AGENTS.md` §3.3
forbids.

All four also gained the argument check that `arg_shape` exists to perform, and
that is the only *behaviour* that changes for the two refused pairs: Linux's ioctl
layer copies every DRM struct in before a handler can refuse it, so a caller whose
pointer is not its own memory now hears `EFAULT` where before these answered
`ENOTTY` without ever looking at the struct. `t-drm-args` pins that per request,
beside the errnos.

**One sentence earlier in this section is now known to be wrong.** It says
`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is "the one thing that *is* missing without
a hardware reason". Measured with `modetest -p` on the same amdgpu card: each
CRTC has four planes, the cursor planes report `formats: AR24` and `gamma size 0`,
and their **only** properties are `type`, `IN_FORMATS` and `IN_FORMATS_ASYNC` -
there is no `HOTSPOT_X` and no `HOTSPOT_Y` anywhere in the machine. So a real card
with four real cursor planes refuses that cap's premise too, which makes it a
driver opt-in rather than the compositor-facing family the sentence implied - the
same finding, and the same correction, as `FB_DAMAGE_CLIPS` and `OUT_FENCE_PTR`
further down that table.

**The cursor is a plane now, and the cap is still refused for the reason the
measurement gives.** The card has a second plane whose `type` is Cursor, listed by
`GETPLANERESOURCES` and described by `GETPLANE`; the `plane_id` a `GETPLANE`
carries finally selects *which* plane answers, where it used to be ignored
entirely and every question was answered with the primary plane - a bug on its own,
and a blocker for a second plane, since a compositor finds its cursor plane by
asking about each id and reading `type` back. `MODE_CURSOR` and `MODE_CURSOR2` are
the legacy door to the same plane; they are in the `PRESENTATION` list with the
other requests that put pixels on the screen, because a pointer that keeps moving
after the master loses its console is pixels.

`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` stays refused, and the measurement is why:
`modetest -p` reports `type`, `IN_FORMATS` and `IN_FORMATS_ASYNC` as the *only*
properties on any of the real card's four cursor planes, with no `HOTSPOT_X` or
`HOTSPOT_Y` anywhere in the machine. Having a cursor plane is a different question
from publishing hotspot properties, and the hotspot itself is not lost - it is in
`MODE_CURSOR2`'s struct, which this card honours, and a compositor that needs it
subtracts it from the position it asks for. So the cap is refused because there is
no property for it to be about, which is exactly what a real card's cursor planes
measure as.

**The composition is the one thing here that is arithmetic, so it is the one thing
in its own host-tested module.** There is no display controller to composite an
overlay, so the cursor is blended into the scanout block by the same pass that fills
it: `crates/gpu/src/cursor.rs` holds that blend - `ARGB8888` over `BGRA`, premultiplied,
hotspot-pinned, clipped at all four edges in one intersection computed up front -
with ten `cargo test` cases beside it, following `drm_blob.rs` and `evdev.rs`. The
driver calls it at the end of every `copy_to_scanout`, which is what makes the two
orderings come out right without either side knowing about the other: a
compositor's flip lands *under* the pointer rather than erasing it, and a
`MODE_CURSOR` move re-copies and re-blends, so the pointer moves without the
compositor redrawing anything. The `ARGB8888` in that sentence is the format the
fourcc correction above had to land first.

**Two gaps in this area are named rather than filled.** The cursor plane publishes
`type`, `FB_ID`, `CRTC_ID`, `CRTC_X`, `CRTC_Y` and its own `IN_FORMATS` - no
`SRC_*`/`CRTC_W`/`CRTC_H`, because this card's cursor has no scaler and no crop and
a published-then-ignored property is the silent success `AGENTS.md` §3 forbids. A
compositor that wants to crop or scale a cursor will not find the properties and
will do it itself, which is the honest outcome but is narrower than a real card. And
an unknown CRTC in `MODE_CURSOR` is `EINVAL`, which is what this file has always
answered for an object it cannot use, where Linux's `drm_crtc_find` says `ENOENT`;
the whole file would have to move together for that to change, so the cursor follows
the file rather than splitting it. The cursor plane work is where the cap's answer belongs,
and it had this measurement to build on rather than a guess.

Properties and caps, cheap one at a time and load-bearing in aggregate: `EDID`
(a blob - with non-zero `mm_width`/`mm_height` and a `subconnector`, if anything
is ever to show a monitor's name), `non-desktop`, `max bpc`,
`panel orientation`, `vrr_capable`/`VRR_ENABLED` with
`DRM_CAP_ASYNC_PAGE_FLIP` beside them, `GAMMA_LUT`/`GAMMA_LUT_SIZE`,
`FB_DAMAGE_CLIPS` (the blob that lets a compositor say *what* changed instead of
sending whole frames), `OUT_FENCE_PTR`, `IN_FORMATS` with
`DRM_CAP_ADDFB2_MODIFIERS`, and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` - refused
today, and asked for precisely by the virtualized drivers.

**The list above was written from the sources, and measuring it corrected three
of its entries.** `modetest -c -p` against the real card on the development host
(AMD `amdgpu`, DRM interface 3.64.0, Linux 7.2.7) is the instrument, and what it
reports is the shape each property really has:

| property | object | flags | shape as measured |
|---|---|---|---|
| `EDID` | connector | immutable blob | 128 bytes + extension blocks |
| `DPMS` | connector | enum | `On=0 Standby=1 Suspend=2 Off=3` |
| `link-status` | connector | enum | `Good=0 Bad=1` |
| `non-desktop` | connector | **immutable range** | `values 0 1` - a range, *not* the bool the sources suggest |
| `max bpc` | connector | range | `values 8 16`, value 16 |
| `vrr_capable` | **connector** | immutable range | `values 0 1`; every DP/eDP output carries it, with value 0 on the ones whose sink is not VRR |
| `subconnector` | connector | immutable enum | `Unknown=0 VGA=1 DVI-D=3 DP=10 HDMI=11 Native=15 Wireless=18`, and only on connectors that have one (the eDP output has none) |
| `VRR_ENABLED` | **CRTC** | range | `values 0 1` |
| `GAMMA_LUT` / `GAMMA_LUT_SIZE` | **CRTC** | blob / immutable range | size reports `values 0 4294967295`, value 4096 |
| `IN_FORMATS` | plane | immutable blob | |
| `FB_DAMAGE_CLIPS` | plane | - | **absent on a real card**, so it is a driver opt-in rather than a family a compositor can rely on |
| `OUT_FENCE_PTR` | plane | - | **absent on a real card** for the same reason |

Two consequences for this plan, and both change what "add the property" means.
`non-desktop` and `vrr_capable` are **ranges with two values**, so a card that
answered them as a `PROP_BOOL` would give a reader a shape it does not expect -
and `vrr_capable` belongs on the connector while `VRR_ENABLED` belongs on the
CRTC, which the sources' prose does not make obvious. And the last two rows are
the reminder the whole section is written around: a property family a
*compositor* looks up by name is one the driver may legitimately not have, so
the honest answer for this card is absence plus the `drm_info`-visible reason -
not a blob property invented to make the list look complete.

**What was then built, and what was left absent on purpose.** The card now
publishes four of them, in `src/drivers/drm.rs`, each one true:

| property | object | shape here | why |
|---|---|---|---|
| `EDID` | connector | immutable blob | a real 128-byte EDID 1.4 base block from `crate::gpu::blob`, rebuilt when the host resizes the window - the detailed timing *is* the advertised mode |
| `non-desktop` | connector | immutable range 0..1, value 0 | this is a panel, not a headset |
| `max bpc` | connector | range 8..8, value 8 | the scanout is eight bits per channel; a range of one value means a write of 16 is `EINVAL` rather than a success that changes no pixels |
| `vrr_capable` | connector | immutable range 0..1, value 0 | the refresh rate cannot vary, and saying so is what stops a compositor asking |
| `IN_FORMATS` | plane | immutable blob | one linear modifier over the two formats `GETPLANE` reports and `ADDFB2` accepts - one array feeds all three, so they cannot drift |

The connector's `mm_width`/`mm_height` now come from the EDID's own centimetre
fields times ten (the relation the measurement found: 30 cm and 19 cm reported
as 300x190), and `subpixel` is written as the 0 a real connected connector
reports.

`DRM_CAP_ADDFB2_MODIFIERS` is answered 1 *and* made true: `ADDFB2` takes
`DRM_MODE_FB_MODIFIERS` with `DRM_FORMAT_MOD_LINEAR` and refuses any other
modifier with `EINVAL` - which is the `IN_FORMATS` list saying the same thing in
the other direction, and the reason the two are pinned together. A cap answered
1 over an `ADDFB2` that refused every modifier is a caller building a buffer it
cannot bind. `DRM_CAP_ASYNC_PAGE_FLIP` is answered 0 and *named* in the match
rather than left to the catch-all: tearing flips are not missing here, they are
a presentation mode this card does not have.

**And the format list itself was wrong in two ways at once, both found by
arithmetic against `<drm_fourcc.h>`.** `fourcc_code` packs the four named
characters low byte first, so the card's `DRM_FORMAT_ARGB8888` - `0x3432_4152` -
decodes to "RA24", which is `DRM_FORMAT_RGBA8888`; the real `ARGB8888` is
`0x3432_5241`. `DRM_FORMAT_ABGR8888` had the same slip, `0x3432_4142` being
"BA24" (`BGRA8888`) rather than "AB24". So `ADDFB2` was accepting two formats it
never named and **refusing the one it did** - and since a cursor buffer is
`ARGB8888` by definition, that is why the cursor plane below could not have bound
one.

Correcting the names exposed the second problem. `XRGB8888` ("XR24") and
`XBGR8888` ("XB24") are the *same* 32-bit word read the other way round in
memory: B,G,R,X against R,G,B,X. Presentation here is a verbatim row copy into a
BGRA block and nothing in the path swaps channels the way a real plane's scaler
does, so a card accepting both would show one of them with red and blue
exchanged - silently, at full frame rate. The accepted set is therefore narrowed
to the two spellings whose bytes are the order the scanout actually holds,
`XRGB8888` and `ARGB8888`, and the other two are refused rather than accepted
into wrong pixels. `GETPLANE`, `IN_FORMATS` and `ADDFB2` all read the one
`PLANE_FORMATS` array, so all three moved together.

The byte order is measured, not assumed, and the instrument is this repository's
own: `crate::term::blit`'s `put` writes a pixel as blue, green, red, 0, and that
console has been showing correct colours since it was written. It agrees with
`drivers::virtio::gpu`'s module comment ("BGRA, row-major") and with
`B8G8R8A8_UNORM` being virtio-gpu's default scanout format. A real card lists
`RA24` as well, because real hardware converts; this one does not, and now says
so instead of pretending.

`GAMMA_LUT`/`GAMMA_LUT_SIZE`, `VRR_ENABLED`, `panel orientation`,
`subconnector`, `FB_DAMAGE_CLIPS` and `OUT_FENCE_PTR` are **absent**, and each
absence is the measurement's rather than an omission: a real card carries
gamma and VRR properties because its driver can honour them, carries
`subconnector` and `panel orientation` only on connectors that have one (the
measured eDP output has neither), and carries neither of the last two at all.
A property whose write changes nothing is the silent success `AGENTS.md` §3
forbids. The one thing that *is* missing without a hardware reason is
`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`, and it belongs with the cursor plane in
the `MODE_CURSOR` work rather than here.

Syscalls and memory, the parts this section cannot fix from inside the display
driver: `/proc/<pid>/maps` **landed** (§Remaining has the file and the two
divergences it carries) - a crash reporter now has somewhere to look for a
program's base address; and the frame pool (§10.4), since the renderer families
allocate scratch in tens of megabytes where the current per-frame accounting was
never sized for it. Threads used to head
this list and are §6.4's now - landed. Anonymous demand paging
and `MADV_DONTNEED` used to be on that list - they are §5.1/§5.2's rewrite now,
and the scratch a renderer gives back is memory the machine can use again.

Services, which are rootfs work: `sway`/`niri` with their libraries
(`libdisplay-info`, `libseat`, `xkbcommon`, `pango`/`cairo` and the fonts *and
writable font cache* those need), `vulkan-loader` with an ICD, and the
compositor-starting half weston already models.

#### What the sync object family answers, measured and then built

`SYNCOBJ_*` is the last item on the list above besides `DRM_VIRTGPU_*`, and its
design is now written from measurement rather than from the sources, because the
two disagree about the one question the whole family turns on: **what a syncobj
that holds no fence means.** Four instrument programs were run against the real
card (`/dev/dri/renderD128`, AMD `amdgpu`, DRM interface 3.64.0, Linux 7.2.7) -
one for creation, destruction and the descriptor handover, one for the wait
family, one for the transfer and eventfd edges, and one for the three rules the
first three could not separate between them. Everything below is a line one of
them printed.

**This host's header is ahead of the upstream one in two ways a caller notices.**
`struct drm_syncobj_wait` and `struct drm_syncobj_timeline_wait` have each grown
a trailing `__u64 deadline_nsec`, so they are 40 and 48 bytes rather than the 32
and 40 the older headers describe, and
`DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE` / `..._FD_TO_HANDLE_FLAGS_TIMELINE`
exist at `1 << 1` on both directions. The first is not a detail: a program built
against the old layout sends a request word whose encoded size is eight bytes
short, and the entire wait family answers `EINVAL` - which is what the first
instrument run measured, in full, before anyone noticed. It is why every number
below is quoted with the struct it came from.

**A syncobj either holds a fence or it does not, and "does not" is a refusal
rather than a success.** This is the divergence from the sources, which treat a
fence-less binary syncobj as already signaled because there is nothing to wait
for. Measured here, every request that needs something to wait on refuses a
fence-less object instead:

| on a fence-less object | measured |
|---|---|
| `WAIT`, one handle, timeout `0` or `-1` | `EINVAL` (no blocking without `WAIT_FOR_SUBMIT`) |
| `WAIT` with `WAIT_FOR_SUBMIT` | blocks; `ETIME` when the timeout expires |
| `TIMELINE_WAIT` at a point no fence covers | `EINVAL` |
| `TIMELINE_WAIT` with `WAIT_FOR_SUBMIT` | success as soon as *a* fence exists, whatever the point; `ETIME` otherwise |
| `HANDLE_TO_FD` with `EXPORT_SYNC_FILE` | `EINVAL` |
| `EVENTFD` | accepted, and never fires |

So the state a syncobj is in is not "signaled or not" but "has something behind
it or not", and the operations that *create* one are the whole of how it changes:
`CREATE` with `SIGNALED`, `SIGNAL`, `TIMELINE_SIGNAL`, `TRANSFER` into it, and
`FD_TO_HANDLE` with `IMPORT_SYNC_FILE` each leave a fence behind; `CREATE`
without the flag and `RESET` take it away. A **timeline** is not a kind chosen at
creation - `CREATE` has no timeline flag - but a syncobj that has had a point
submitted to it: `TIMELINE_SIGNAL` on a binary syncobj is `ok` and makes it one,
and `TIMELINE_WAIT` on a syncobj that never had a point is `EINVAL`. `QUERY`
reports the highest point submitted (0 for one that never was), which is why
`RESET` on a timeline reads back 0, and why `SIGNAL` - which a timeline also
accepts - leaves the point where it found it.

The rest of the family, as measured: `TRANSFER` requires only that *one* of its
two objects hold a fence, otherwise `EINVAL`; it both copies the fence and sets
the destination's point to `dst_point` when the destination is a timeline, and it
does not care whether `dst_point` is above or below the point already there.
`HANDLE_TO_FD` with `flags = 0` answers a **syncobj descriptor** - the anonymous
file that *is* the syncobj, which is what `wp_linux_drm_syncobj_v1` passes between
processes - while `EXPORT_SYNC_FILE` answers a **sync-file** descriptor. The two
are told apart by the import: `FD_TO_HANDLE` with `flags = 0` requires a syncobj
descriptor and installs a second handle for the same object, while
`IMPORT_SYNC_FILE` requires a sync-file descriptor **and** a `handle` that names
an existing syncobj to replace - `handle = 0`, which is how the other direction
asks for a *new* handle, is `ENOENT` here. A program that follows the descriptor
matrix wrong gets `EINVAL` from the other direction's entry point, not a second
object. `EVENTFD` registers a one-shot per call: the eventfd's counter is
incremented by one when the registered point first becomes covered, immediately
if it already is, once per registration (registering twice and then signaling
once reads back `2`), and never if the syncobj is destroyed first.

Two consequences for the work, and they are what the implementation is shaped
around. **First, nothing on this machine can produce an unsignaled fence** - the
only fence a client can obtain is a signaled stub, which the instruments confirm
by never once seeing a wait block on an object that had one - so the whole
"asynchronous" half of the family collapses into "has a fence yet or not", and
that is a truth about this card rather than a simplification of it: there is no
engine here to be in flight, which is the same fact `WAIT_VBLANK` and the CRTC
sequence pair are refused on. **Second, `WAIT`/`TIMELINE_WAIT` are the only
blocking requests in the family**, which is what makes them the two that need the
scheduler: a `WAIT_FOR_SUBMIT` parks until some other request leaves a fence
behind, and `EVENTFD` is the same wait expressed as a counter for a caller with
an event loop - which is why it is the probe Smithay makes before it will expose
`wp_linux_drm_syncobj_v1` at all.

`DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE` are both answered `1` by the
real card, so this card answering `1` is a statement it has to make true rather
than a capability it can decline.

**What was then built, and the one place it is deliberately narrower.** The
twelve requests are dispatched, `crates/gpu/src/sync.rs` holds the state machine with
nine host tests and `t-drm-sync` pins 103 checks in the booted machine - the
family's whole shape, the descriptor handover in both directions, a waiter woken
by *another task* rather than merely told it would be, and the two commit fences
end to end. Two names are worth putting beside the work. The **handle table is
per open file description**, which is what makes two clients each have a handle 1
and is the reason `SYNCOBJ_HANDLE_TO_FD` is the request the wayland protocol is
built on; a record dies with its description, and the object itself lives on
while a descriptor names it. And the **out-fence a commit writes back is already
through** when the caller has it, because this card presents synchronously: the
flush has returned before the commit does, so the honest fence is a retired one.
That is a narrower answer than a real card's - a caller cannot use it to learn
about a frame that is still in flight, because there is no in-flight frame - and
it is the same absence as `DRM_CAP_ASYNC_PAGE_FLIP` being 0.

**Gate, quoted from its own summary**: host `615 passed`; `mpos-xtask 36 passed`;
guest `=== SUMMARY pass=131 fail=0 (131 built)`, QEMU exit 33, `9 of 140 user
tests left out by tests.toml` (the same nine as before, `t-drm-sync` being a
tenth program that runs). `t-drm-sync` printed `=== PASS` over 103 checks.

#### Order

Each step stands on its own, which is the only reason the order is this one:

1. **A wlroots compositor on pixman.** Rootfs and init only; no kernel change is
   expected. `WLR_RENDERER=pixman` on `/dev/dri/card0`, and whatever the run says
   is missing becomes `t-drm`'s next case. Cheapest possible answer to "does a
   second compositor family run here", and a better rendering instrument than
   weston into the bargain.
2. **The property and cap families above.** No new subsystem: `GETPROPERTY`,
   `GETPROPBLOB` and one client cap, each observable from userspace
   (`drm_info`, `modetest`) before any compositor is involved.
3. ~~**DRM sync objects**~~ **Landed**, with `IN_FENCE_FD`/`OUT_FENCE_PTR` given
   the behaviour their names promise - which is what makes explicit sync real
   rather than merely accepted. The step was measured before it was written, and
   the measurement is the part worth keeping: see "What the sync object family
   answers" above for what this card means by a sync object and how it differs
   from the sources.
4. ~~**Threads** (§6.4)~~ **Landed** - not a display item, but on the critical path
   of every compositor that is not single-threaded and every Vulkan driver, so it
   is worth knowing the step is behind us before the property families.
5. ~~**`DRM_VIRTGPU_*`**~~ **Landed, 2D and 3D both** — the largest item, and the
   only one that makes niri possible. **"Then virgl in the image" is the part
   still open, and it is upstream of the 3D queue**: Mesa does not reach the
   driver at all, because libdrm cannot describe the card — `drmGetDevices2`
   returns 0. That chain is **built** now, from a working card's own sysfs reads
   (§7.4's third update): the node hangs off the display-class PCI function, and
   PCI functions carry the `subsystem` link they never had. What is left here is
   a *measurement*, not a build: the GL-card run that says whether the renderer
   is off `llvmpipe`, which the console defect below has so far killed every
   attempt at.
6. ~~**`/proc/<pid>/maps`**~~ and the frame pool: the supporting cast, wanted by
   everything above and by nothing in particular. (`MADV_DONTNEED` was on this
   line, and §5.2 has it now; `maps` is §Remaining's, and it is written - see
   the board's entry for the two things it needed, the loader's recorded ranges
   and the `MAP_SHARED` bit the perms column reports.)

---

### 7.5 The device set is assembled by the drivers that own it — **LANDED**

`drivers::devices::standard_devfs` used to decide which hardware nodes to publish by naming the
drivers: `if let Some(rng) = crate::drivers::virtio::rng::global()`, and the same shape for the
console port, the sound card, the framebuffer, the input registry. That is registration reaching
into the drivers, and one file to edit for every new device; it also let the two drift, because the
builder had to know which of a driver's globals were populated (the aarch64 entropy node was a
second `#[cfg]` arm here until §15.2 unified the transport).

Inverted: `devices::publish(owner, nodes)` fills a registry, and `standard_devfs` drains it without
naming a single device. Each probed driver publishes its own nodes at the one install door it
already had — `rng::install` (the path both buses reach, the PCI `init_global` and the MMIO
`rng_device`), `console::install`, `sound::install`, `VirtioGpuDev::install`, `input::register` — so
the probe's answer decides whether a node exists, at the place the probe happens. Publishing is
idempotent per owner, so a driver that comes up on two buses, or retries, cannot produce two nodes
under one name.

What deliberately did not move is the distinction the nodes themselves draw: the core set (the
console, the ttys, the entropy *pair*, the null/zero/full sinks, `kmsg`, `ptmx`, the `/proc` links)
is the device interface every machine has rather than the result of a probe, and `rtc0`/`rtc`, the
`/dev/dri` pair and the disk projection stay in `devices.rs` for the same reason — the subsystem
exists whether or not a device answered. The entropy pipe's `CharDevice` adapter moved to
`virtio/rng.rs` with its node; the others already lived in their drivers.

Measured: host tests `mpos` 517; x86_64 `cargo xtask test` `=== SUMMARY pass=160 fail=0 (160 built)`
with QEMU exit 33; aarch64 `cargo xtask test --arch aarch64` `=== SUMMARY pass=143 fail=0`; both
bare-metal builds with no new warning; `cargo xtask depgraph` within the ratchet, mutual pairs 8/8 —
the inversion adds no module edge, because a driver calling `crate::drivers::devices` stays inside
the `drivers` module. The aarch64 boot check that resolves `/dev/hwrng` and `/dev/input` through the
mounted tree (`check_dev`, `M7_DEV_OK`) is the end-to-end evidence that the driver-published nodes
are the ones userland sees.

### 7.6 Configuration space over MMIO: what the firmware names, and the reserved word that hid it — **LANDED**

x86_64 reads configuration space through the 0xcf8/0xcfc port pair (mechanism 1), which
reaches 256 bytes per function - the address register has eight bits of offset - so a
device's PCIe extended capabilities are outside it. A PCIe machine may instead decode
configuration space as *memory* (ECAM, or MMCONFIG: one 4 KiB page per function, one
1 MiB window per bus) and say where in ACPI's MCFG table. Enumerating over MMIO with the
port pair as the fallback starts with reading that table.

Neither prerequisite existed. The kernel had **no ACPI support at all** - the machine that
describes itself with tables was the one whose description the kernel ignored - and the
loader's configuration-table walk knew only the device-tree GUID. Both are in now:

- `crates/acpi`: the root pointer (both checksums, and the extended one is a condition on
  using a revision-2 pointer), an SDT header validated against the bytes that arrived, the
  RSDT/XSDT walk, and MCFG's windows down to the per-function offset. Fourteen host tests,
  and `fuzz/acpi_tables` (3.4M executions clean).
- the loader finds the root pointer in the firmware's configuration table (the ACPI 2.0
  GUID first, the 1.0 one second) and validates it *there*, while the firmware's memory is
  certainly still mapped, before writing `rsdp_phys` into the record. The field is appended
  after the device-tree pair for the reason that field's own note gives. The tables are not
  copied, unlike the device tree: the memory map reports them ACPI-reclaimable, which the
  frame allocator never hands out, so the kernel reads them through the direct map.
- `drivers::pci` walks them and says what it found. On this host, in the sweep's own boot:
  `pci: MCFG names ECAM at 0xe0000000, segment 0, buses 0..=255, 256 MiB`.

**The first version of the parser was wrong, and the boot is what caught it.** MCFG keeps
**eight reserved bytes** between its header and its first entry. A reader that starts the
entry loop at the header's end reads the reserved word as an entry's base, the entry's base
as its *segment*, and the entry's end bus as its own - and the answer that produces is not
a machine, it is a plausible-looking one: `base 0x0, buses 0..=224` where the window is
really `0xe0000000, 0..=255`. The host tests agreed with the bug because their fixtures
were built the same way, which is the lesson worth keeping: a fixture that encodes the
same mistake as the code proves nothing about the format. Both are fixed, and the wrong
shape is a test of its own now (`the_reserved_word_is_not_read_as_an_entry`).

A second, smaller one was the same class: the first walk validated the root table from a
36-byte read, so the table's own declared length looked like an overrun and every table
was refused. Reading a table is necessarily staged - the length is in the header and the
checksum is over the whole table - so `crates/acpi` grew a `declared_length` door that
says what the header claims *without* validating it, and `pci::read_table` reads the
length, then the table, then validates the whole of it.

**What was deliberately left for the next slice, and why it was a step with a size.** With
the tables read, the window was *named* and not yet *used*. Switching reads to it means
mapping it, and this machine's MCFG covers 256 buses - **256 MiB of address space**. Mapping
that eagerly, for a walk that touches two or three buses, is the wrong trade; the next step
was a per-bus mapping (1 MiB on demand), an equivalence check that ECAM and mechanism 1
answer the same for the same function before the switch is trusted, and mechanism 1 kept as
the fallback for a machine whose firmware names no window at all. It is §7.6a, immediately
below.

### 7.6a The switch itself: mapped a bus at a time, and trusted only after both mechanisms agreed — **LANDED**

The three pieces the paragraph above named are in, and the shape they took is worth
recording because two of them are not the obvious ones.

**One address, two mechanisms.** `drivers::pci`'s six accessors (`read8`/`read16`/`read32`
and their writers) are now a dispatch rather than a port pair: they ask a window for the
register's kernel-half address and fall back to the ports when there is none
(`window_byte` → `slice`). The port bodies are the same code they were, renamed
(`port_read32`/`port_write32`/…), and every caller above - `config_read`/`config_write`,
`Func`, `read_function`, `read_bars`, `scan_bus`, and through them `virtio`'s
`PciTransport` and the `config` files under `/sys` and `/proc` - is unchanged, because the
mechanism is not its business. The masks are load-bearing and identical either way:
`off & 0xfc` for a dword, `& 0xfe` for a word, nothing for a byte, so "which mechanism read
this" cannot change which bytes were touched.

**A bus at a time.** `slice(bus)` maps that bus's mebibyte on the first access to it, into
this machine's MMIO window (`hal::map_mmio_window`), and remembers where in a table indexed
by bus number - `[usize; 256]`, with zero meaning "not yet mapped", which is unambiguous
rather than a convention because no mapping this machine hands out is ever at address zero.
A slice is never handed back: the bus is walked for the machine's life and a driver probing
an hour later asks for the same one. The map itself happens *outside* the critical section
that reads the table, because mapping allocates frames and writes page tables and that is
not work to do with interrupts off. The two steps cannot disagree: `slice` is the only
writer of a window's `slices`, `use_window` and `use_ports` run at most once each and only
at boot, and a task preempted between them could only find the entry still zero and map the
same bytes a second time. A window that does not cover a bus, or a machine with no room left
for a mapping, answers `None` and that access goes through the ports - the honest refusal
rather than an address nothing answers for.

**The check, and why it is a check rather than a preference.** `init` walks the bus over the
ports *first* - the mechanism that asks nothing of the firmware, and therefore the one known
to work - and, if the firmware named a window, walks it again through the window and
compares the two `Vec<Device>`s, which compare element for element because the model already
derives `PartialEq` (`crates/pci`). Equal: the window is kept, and from then on discovery
and the `config` files read memory. Different: `first_difference` names the first function
the two walks disagreed about (or says that they did not even find the same *number* of
them), the ports are put back, and the boot log says so. A window that starts above bus 0 is
refused for a sharper reason than "it cannot serve the walk": the check would be *vacuous*.
`enumerate` walks from bus 0, so every access on it would fall back to the ports, the two
snapshots would agree by construction, and the boot would claim a window it never read a byte
through - the one failure mode a check like this must not have. That is also why
the `Ecam` struct this module used to keep is **gone**: `discover_ecam` now returns
`crate::acpi::EcamRange` itself, so "does the window cover this bus" and "where is that bus's
slice" are answered by the host-tested crate instead of by a kernel-side copy of the same
four fields.

Making the layout one definition rather than two is `crates/acpi`'s new `function_offset`
(device and function bits inside a bus's mebibyte), which `EcamRange::offset` is now the bus
term plus - so a kernel mapping one bus at a time and a reader walking the whole window
cannot drift, and a window that does not start at bus zero is the case that would have
shown it. Host test: `a_bus_slice_and_a_whole_window_are_the_same_layout`; the fuzz target
reaches it through `EcamRange::offset`.

**Measured.** On the sweep's own machine, in its boot log:

    pci: MCFG names ECAM at 0xe0000000, segment 0, buses 0..=255, 256 MiB
    pci: ECAM and the port pair agree on all 14 functions; configuration space is over MMIO
    pci: 14 functions enumerated

and then the whole sweep through the switched mechanism: x86_64 `cargo xtask test`
`=== SUMMARY pass=160 fail=0 (160 built)` with QEMU exit 33 - which is the evidence that
matters, because every driver in it (virtio over `PciTransport`, AHCI, the GPU and its
DRM card) probed its device through the window. aarch64 is untouched: `drivers::pci` is
x86_64's module, and that arm's `pass=143 fail=0` stands. Host tests `mpos-acpi` 14 → 15.
`cargo xtask depgraph` within the ratchet (the switch adds no module edge: `drivers::pci`
already called `hal` and `acpi`).

**A rustfmt catch-up rode along, and it is named rather than hidden.** `cargo fmt --check`
was red at `HEAD`: the two previous commits had landed unformatted code
(`EcamRange::offset`'s over-long signature line, and two spots in `drivers::devices`'s new
`Publisher` drain and `virtio/rng.rs`'s import order). The first two are in files this slice
touches anyway; the last two are their own commit, because a formatter pass and a mechanism
switch have no common *why* (`AGENTS.md` §3's one-intent rule).

**What is still open, and it is a size rather than a smell.** A function's configuration
space is 4 KiB and this kernel still publishes `CFG_SIZE` = 256 - the *port* mechanism's
reach - so PCIe extended capabilities are outside every reader even now that the window
could reach them. Deliberately not joined to this one: a `/sys/.../config` file's size would
start depending on which mechanism the firmware named, and "the window makes it possible" is
not "the window makes it right". It is a `docs/ROADMAP.md` item of its own.

## Phase 8: Security Hardening (Low Priority)

### 8.0 A userspace process could panic the kernel — **fixed: a file is not one buffer**

`dd if=/dev/zero of=/tmp/x bs=1M count=700` used to end in a kernel panic and
QEMU exit `35`. The cause is now read off the machine's own report rather than
inferred, and it was neither of the two things first written here:

    KERNEL PANIC: memory allocation of 134217728 bytes failed
      last 8 syscalls: nr 1 (write) ...

Not ramfs being unbounded, and not the ext4 block cache (fixed at 512 preallocated
slots). It was a **ramfs file's own storage**: one `Vec<u8>`, so growing a file
means a `realloc` for one *contiguous* run - doubling to 128 MiB - which is not a
shortage of memory but an order the buddy allocator never serves. The machine died
with bytes still in hand.

**Fixed** by the shape Linux uses: `Chunked`, a file as a list of 64 KiB chunks,
so growth allocates 64 KiB at a time however large the file gets
(`vfs/ramfs.rs`, `a_file_is_chunks_and_not_one_contiguous_run`). The invariant the
first version got wrong is worth stating because nothing else on the page says it:
**every chunk but the last is whole**, because a chunk that stops being last has
bytes after it now - a 5-byte interior chunk makes `len()` lie about everything
that follows, which three existing file tests caught at once.

Also landed, as the tmpfs behaviour this kernel genuinely lacked: a per-mount byte
budget, charged *before* allocating and answered with `ENOSPC`
(`Usage`, `a_full_mount_answers_enospc_instead_of_growing`), with room returned on
truncate-down and on the last reference dropping rather than on `unlink` - a
deleted-but-open file still occupies its bytes. The budget guard checks what the
machine could actually hand over: free frames **plus the heap's own cached
bytes**, since the heap keeps what it has taken and `free` never rises again.

`t-fill-ramfs` covers the end-to-end property (refused, not dead) and one
non-obvious trap it found: `ftruncate(0)` leaves the descriptor's offset where it
was, so the next write asks for a hundreds-of-megabytes hole and is refused
*correctly* - a test that forgets to `lseek` back measures the kernel's manners,
not its memory.

**Still open, and it is the rest of the original item**: the global allocator's
failure path is still `panic!`, so any *other* unbounded user-driven growth - a
`Vec` in another filesystem path, the page tables a process that touches enough of
what it mapped will need - still ends the machine. Anonymous demand paging
(§5.1/§5.2) took the cheapest half of that away: mapping too much is now a
statement about addresses rather than a claim on memory, so the
map-a-tebibyte-and-never-touch-it shape this item was written around costs one
VMA and nothing else. What remains is the work the old plan named: make
user-facing allocation fallible end to end (`ENOMEM` out of the syscall, not a
panic) and give a process that runs the memory out something to lose besides the
kernel.

### 8.1 ASLR — **DONE** (four landmarks; `ET_EXEC`'s own address deliberately not one of them)

Measured on Linux before writing any of it, over 64 runs of the same program
each time: a position-independent binary's code base, its heap anchor and its
`mmap` page all move by amounts of the order of a tebibyte, and a process's
stack top moves too. The same program linked `-no-pie` kept its code address at
`0x401156` in every single run while its stack and its `mmap` page went on
moving. That last row is the shape of this feature: **a fixed-address image must
not be moved**, because the absolute addresses its own instructions carry were
chosen at link time, and "randomising" it produces a program that cannot start
rather than one that is hard to attack.

What is drawn per `exec`, in `Layout` (`src/sched/runtime.rs`):

| Landmark | Range | Notes |
|---|---|---|
| `ET_DYN` image load bias | a tebibyte's worth, page-aligned | `map_image` places every segment at `bias + vaddr`, and `AT_PHDR`/`AT_ENTRY`/`brk` are the *placed* addresses |
| The interpreter's base | the same, stacked below `mmap` | This is `libc` on this userland - a fixed base here is a supply of return addresses identical in every process |
| Stack top | 8 MiB | Linux bounds this by memory and rlimit, so the number is not portable; 8 MiB is what keeps the whole region clear of the image below |
| The `mmap` region's start | a tebibyte's worth | `mmap_cursor` starts here and grows down as before |

The regions are **stacked rather than drawn independently** - `mmap`'s start,
then the interpreter below it, then the image below that, with a fresh draw
between each pair - so no draw can land one region on top of another, and the
top of a placed image is always below `MMAP_BASE`, which is what keeps a bias
from walking a program's pages up into the kernel half. `fork` draws nothing: a
child that inherited its parent's pages at addresses of its own had would not
have inherited its parent's pages.

Relocation had to be settled to get here, and the answer is Linux's: **the
kernel relocates only an image that has no interpreter.** A dynamic one is
relocated by that interpreter, which finds the program's load address from
`AT_PHDR` - so a kernel that also applied the table would add the load bias to
every pointer twice. That pass used to run for both kinds, which was invisible
only while the bias was always zero, because writing `addend` twice and writing
`addend + 0` twice land in the same place.

`t-aslr` pins the parts a running program can see: it forks, `execve`s *itself*
in a reporting mode, and compares what two fresh runs say about themselves - the
stack and the `mmap` page differ, the code address and the heap anchor do not.
Self-exec is why it can do that without a shell, a fixture, or a runner that
passes arguments it does not pass to anything else. What it cannot cover is the
load bias, because every program in that initramfs is `ET_EXEC` and none of them
has an address the kernel is allowed to choose; that half is covered by the
desktop image, where every binary is a dynamic `ET_DYN` and a bias applied to
the mappings but not to `AT_PHDR` means the machine never reaches a login
prompt.

**Not done, and each one is a decision rather than an oversight**: no `vdso`
(there is none to move); the stack's range is our bound, not Linux's
memory-sized one; and the draws are 28 bits of page granularity where Linux
gives an `ET_DYN` base rather more. Adding `listen`/`accept`-shaped inbound TCP
(§3.3) and a `MAP_FIXED`-free `mmap` hole review would both want this layout
re-examined, which is worth knowing before either is attempted.

### 8.2 seccomp

| Syscall | Nr | Description | Status |
|---------|-----|-------------|--------|
| `seccomp` | 317 | Secure computing mode | Done (STRICT) |
| `prctl(PR_SET_SECCOMP/PR_GET_SECCOMP)` | 157 | Legacy latch spelling | Done (STRICT) |

- `SECCOMP_MODE_STRICT` — **done**. Latched via `seccomp(2)` (`op=0, flags=0,
  args=NULL`) or `prctl(PR_SET_SECCOMP, 1)`, and read back with
  `prctl(PR_GET_SECCOMP)` (0 unconfined, 1 strict). It is the real Linux
  allow-list, not a block-list: the thread may make only `read`, `write`,
  `exit` and `sigreturn`; every other syscall is fatal (`SIGKILL`), matching
  Linux's `do_exit(SIGKILL)` on a strict violation. The latch is per-task,
  cannot be cleared, survives `exec` and is inherited across `fork`/`clone`.
- `SECCOMP_MODE_FILTER` — **done, wired, and measured action by action**. The
  program, its validation and its evaluation are `src/seccomp.rs` (classic BPF
  with the packet idioms left out, one description of the instruction set shared
  by the validator and the interpreter, forward-only jumps that must land inside
  the program, an out-of-range load ending the program with `KILL_THREAD`, and
  the stacked action order compared as a *signed* field so `KILL_PROCESS`
  outranks `ALLOW`); the plumbing is `Task::seccomp_filters` — a cumulative
  per-thread list inherited across `fork` and kept across `exec` — reached
  through one `install_seccomp_filter` from both `seccomp(2)`'s op 1 and
  `prctl(PR_SET_SECCOMP, 2)`, which copies the program out of the caller and
  validates it *in full* before anything is stored (`EINVAL` malformed, `EFAULT`
  unreadable, `EACCES` for a caller that is not root, `EINVAL` for any flag), a
  machine-wide armed flag that keeps an unfiltered syscall to one relaxed load,
  and a gate that acts on the answer *before* the table runs.

**The eight answers were then measured**, one child per action and a control
beside every table, because four of them were not what this kernel did:

| Action | What Linux does (measured, 7.2.7) | Before | Now |
|---|---|---|---|
| `ALLOW`, `LOG` | the call proceeds | same | same |
| `ERRNO\|d` | returns `-d` — and `d == 0` is a *successful* 0 | same | same |
| `TRAP` | no handler: killed by `SIGSYS`. With one: the handler runs, the call never happens, and the caller reads its own `rax` back (`getppid` answered 110, `getpid` 39 — the number, not a value) | `SIGSYS`, then `-ENOSYS` | `SIGSYS`, then `nr` |
| `KILL_THREAD` | killed by **`SIGSYS`**, not `SIGKILL` | `SIGKILL` | `SIGSYS` |
| `KILL_PROCESS` | killed by **`SIGSYS`** | `SIGKILL` | `SIGSYS` |
| `TRACE`, nobody tracing | the call fails with **`ENOSYS`** | allowed | `ENOSYS` |
| `USER_NOTIF`, no listener | the call fails with `ENOSYS` | `ENOSYS` | same |
| anything unrecognised | killed by `SIGSYS` — the man page's "treated as `KILL_PROCESS`", confirmed here | `-ENOSYS` returned | killed |

Three drafts of that measurement were wrong in ways that produced plausible
numbers, which is why the control exists and why it is written down: judging
*every* syscall with the action measures libc's collapse rather than the kernel
(every line read `SIGSEGV`); `BPF_LD|BPF_W|BPF_ABS` is **0x20** and not 0x00, and
writing 0x00 makes it `ld #0` — after which every conditional filter quietly
judged nothing; and a parent that reads its child's pipe to EOF before waiting
hangs for ever when that child is *stopped* holding the write end. The control —
filtering `getpid` to `EPERM` must fail `getpid` with errno 1 while `gettid`
answers normally — caught each in turn.

- `SECCOMP_GET_ACTION_AVAIL` — **done, and it answers the truth**. Supported
  ⇒ 0, with the caller's own word left alone (Linux's return value carries the
  answer; there is nothing to write back); not implemented — today only
  `USER_NOTIF`, whose purpose is to hand the call to a userspace supervisor this
  machine cannot summon — or carrying *any* data bits, however supported the
  action in them is (`ERRNO|1` is refused, measured), ⇒ `EOPNOTSUPP`; a `flags`
  it would have to ignore ⇒ `EINVAL`; an unreadable pointer ⇒ `EFAULT`. Before
  this it answered success and wrote 0 for every question, so a caller that
  asked learned nothing at all — and since the probe exists precisely because
  Linux kills the process for an action it does not know, answering "no" to
  everything was at least survivable, while answering "yes" to something not
  implemented would not have been.
- **Pins.** `t-seccomp` is new and holds one line per fact: the probe's table
  including the data-bits refusal, one child per fatal action (each is killed by
  signal 31 and each must run before this process installs anything, because a
  filter is inherited and the more restrictive of several decides), the two
  refusals that leave the caller alive answering `ENOSYS`, `LOG` letting the call
  through, then install + the `ERRNO` answer + `PR_GET_SECCOMP` reading 2 + a
  second filter judging another number (the list is cumulative) + inheritance in
  a child that installs nothing + stacking, proved by a `TRAP` installed on top
  of an inherited `ERRNO` winning the decision. `t-posix`'s probe line — it used
  to assert the empty-set answer — now expects `EOPNOTSUPP` for a word naming no
  action at all.
- **Remaining, named: `PTRACE_EVENT_SECCOMP`.** The third case of `TRACE` is a
  tracer that has set `PTRACE_O_TRACESECCOMP` getting its own stop, carrying the
  action's data in `PTRACE_GETEVENTMSG`. This kernel cannot produce it, so
  `PTRACE_SETOPTIONS` refuses the bit with the same `EINVAL` Linux gives for one
  it does not know — every traced caller is therefore the no-option kind, whose
  behaviour *is* Linux's. Building that stop is what would make `TRACE` mean
  notify rather than allow.
- **Not measured, and recorded rather than guessed**: strict mode's own violation
  signal. `seccomp(SECCOMP_SET_MODE_STRICT)` answers `EINVAL` on this machine for
  every argument shape tried, including after `PR_SET_NO_NEW_PRIVS` and with
  `args == NULL`, which is what an outer sandbox filtering `seccomp(op = 0)`
  looks like. Strict therefore keeps `SIGKILL`, the documented answer, and
  `t-posix`'s pin of it stands unchanged.

### 8.3 User Stack Canary — **DONE** (kernel side)

`AT_RANDOM` is now filled from the kernel ChaCha20 pool (RDRAND + boot-epoch +
tick seeded) at every exec — **16 random, per-process, per-boot bytes** — which
is exactly what musl/glibc derive `__stack_chk_guard` from, so a stack-protector
canary is now unpredictable (it was a fixed constant before, defeating the point).
The fail path (`__stack_chk_fail` → `abort()` → `SIGABRT`) is musl's own and
needs no kernel work; `-fstack-protector-strong` is a user-build flag, not the
kernel's. Kernel-side stack protection is therefore complete.

---

## Phase 9: Architecture Expansion (Far Future)

### 9.1 aarch64 Complete Boot

Current status: **stages 1a-1f, 2, 5a, 5b, 6a, 6b, 6c, 6d, 6e landed and `hal`
compiles for aarch64: the machine boots the
way the x86_64 one does, the kernel owns its own address space, two kernel tasks
can suspend and resume each other, the machine reads its own description of
itself, it reaches its devices through a window in its *own* half of the address
space - the firmware's thirty-two virtio-MMIO transports are found through the
tree and read back - code now runs at EL0 with an address space of its own,
traps into the kernel and is answered, and that address space is a thing a
process can live in rather than only enter: its mappings can be protected,
shared, copied on write and taken back, and its tables are returned when it
goes. On top of that the kernel can now answer the questions a *running* process
asks of a space (6c), it reads a real wall epoch off the PL031 (6d), it turns on
user SIMD, masks a line and waits on an interrupt (6e) - and with the `hal` seam
split into six baseline concerns plus three optional capabilities, aarch64
implements the six it has and `crate::hal` compiles for it.**
`cargo xtask boot-aarch64`
builds the kernel (`aarch64-unknown-none`), the loader
(`aarch64-unknown-uefi`), lays out an ESP with `EFI/BOOT/BOOTAA64.EFI` and boots
it under AAVMF; the loader reads `mpos.elf` off the volume, applies its
`R_AARCH64_RELATIVE` relocations against `KERNEL_VIRT_BASE`, builds four-level
tables from the firmware's own memory map and branches into the higher half at
**EL1**; then the kernel brings the frame allocator up from the record and
rebuilds the address space from `mm::vm::kernel_mappings`, which is the same
plan x86_64's `install` consumes. Measured, from one of three consecutive runs
that print every marker - the two numbers that move between runs, `47944` and
`47946` frames, differ by one page of the firmware's own noise, and every number
below is identical across those runs, the two frame addresses stage 6b prints
included:

```
mpos-boot: kernel image: 98 KiB
mpos-boot: initramfs: 11360 KiB
mpos-boot: RAM: 248 MiB
mpos-boot: device tree: 1052672 bytes at phys 0x44699000
mpos-boot: kernel: 104 KiB at phys 0x447da000, virt 0xffffffff80000000, entry 0xffffffff80011618
mpos-boot: mapped 496 MiB of physical memory, identity and through the direct map at 0xffff800000000000
mpos-boot: device window 0x8000000..0xc000000 mapped at its own address and at 0xffffffff40000000, Device and not executable
mpos-boot: page tables: 325 pages of them
mpos 0.1.0 (aarch64) booting
console: PL011 at 0xffffffff41000000
image: text 74 KiB, rodata 20 KiB, data 8 KiB (102 KiB total)
handoff: mpos-boot 0.1.0, record at 0x456ba020, image phys 0x447da000 -> virt 0xffffffff80000000, direct map at 0xffff800000000000
memory: 14 regions, 187 MiB the allocator may use
cmdline: "console=ttyAMA0"
exception level: EL1
devices: the firmware's tree, 1052672 bytes at 0xffff800044699000
devices: memory 0x40000000..0x50000000 (256 MiB), every region the record calls usable is inside it
devices: PL011 at 0x9000000, GIC at 0x8000000/0x8010000, timer on PPI 14 (INTID 30)
devices: 32 virtio-mmio windows, all inside the 0x8000000..0xc000000 device window
M4_DEVICES_OK
pmm: 5 zones, 47944 frames (187 MiB of 187 MiB usable)
interrupts: GICv2 (distributor 0xffffffff40000000), vector table at 0xffffffff8000e000, timer on INTID 30
page tables: 165 frames (660 KiB), TTBR0 0x434e6000, TTBR1 0x434e6000 (the kernel's own, read back from the registers)
plan: 7 mappings, all 63626 pages translate as the plan says (image r-x / r-- / rw-, direct map rw-)
direct map: 0xffff800043458000 -> 0x43458000, rw and it stores
device window: PL011 0xffffffff41000000 -> 0x9000000, Device, through the kernel half at 0xffffffff40000000
no low half: 0x447da000 is unreachable, and so is the device window at 0x8000000 that used to live there
M3_TABLES_OK
virtio: 32 transports, every one answering virtio 1.0 through the kernel half; 1 carries a device
virtio: the first is at 0xa003e00, device 4, vendor 0x554d4551
M5_VIRTIO_OK
timer: CNTFRQ_EL0 62500000 Hz, tick at 250 Hz (250000 counts apart)
M1_TIMER_OK: 25 interrupts in 100 ms (~25 at 250 Hz), 109192656 ns of counter
contexts: two kernel tasks alternating
ABABABABABABAM2_CONTEXT_OK: 13 hand-overs, both directions, every callee-saved register and v31 back where its task left it
space: 4 pages mapped, walked in order and read back through the MMU; two protected, one copy-on-write and resolved into a frame of its own (0x43476000) with the parent's 0x4346a000 left alone; two shared between spaces and written through one of them; two unmapped and given back
space: dropping an active space gave back all 4 frames its tables took, and the page it mapped not among them
M6_AS_OK
paging: active/kernel roots told apart; a named-root walk user-gated and unmapped-refused; uaccess read+write vs unmapped vs the kernel half; the copy-on-write marker the CPU cannot see; a u32 written through a space's root and read back from its frame; leaving to the kernel and re-installing
M6_PAGING_OK
wallclock: PL031 reached through the kernel half; boot anchored to a live UTC epoch; the counter read-only, so write_epoch refuses; the software anchor moves on set_boot_epoch
M6_WALL_OK
cpu: CPACR.FPEN full access after enable_user_simd; a distributor line enabled then masked and read back; idle() woken by the clock and returning with interrupts off
M6_CPU_OK
tls: set_thread_pointer(0xdeadbeef00001234) read back through thread_pointer(); cleared to 0
M6_TLS_OK
EL0: the kernel was on SP_EL1 (SPSel 1)
EL0: root 0x43479000 active, program at 0x200000 (40 bytes), stack at 0x210000, answers at 0x220000
EL0: the program wrote 0xaaaa, called a number no system call has, read 0xffffffffffffffda back and left with status 7
M6_EL0_OK
```

The `devices:` lines are stage 5a, and they are a *check* rather than a
printout: every number in them was read out of the firmware's device tree and
compared against the constant the kernel is compiled with for that device - the
PL011's base, both of the GIC's frames, the timer's line, and the RAM the boot
record calls usable. A tree is not trusted to be right; it is what asks whether
this kernel's idea of the machine still is. The `187 MiB` and the `47944 frames`
against the last stage's `191 MiB`/`48954` are 3.66 MiB of this harness's own
device, 264 KiB of loader page tables and 16 KiB of kernel that stage 6b's check
added (text 62 -> 74 KiB, rodata 16 -> 20: the failure lines are most of it), and
all three are named below rather than
folded into "the tree costs a megabyte": the tree is 1052672 bytes for 7160 bytes
of structure and 569 of strings, and the loader copies what the tree says it is.

Stage 5b is the two lines between `M3_TABLES_OK` and `M5_VIRTIO_OK`, and it is
the first time this machine has *used* a number the firmware gave it rather than
checking one. The device window - PL011 and GIC included - moved out of `TTBR0`
and into the kernel half at `0xffffffff40000000`, which is where x86_64's
`mm::MMIO_BASE` already was, so the low half now holds nothing at all: the
`no low half:` line is the check that says so, and it asks about the device
window's *physical* address as well as the image's. `M5_VIRTIO_OK` then reads
four identity registers out of each of the tree's thirty-two `virtio,mmio`
windows through that new alias, which is what makes the tree actionable: a driver
now has somewhere to go. Three of those four registers are load-bearing
measurements rather than decoration. **The magic and version are checked** -
`0x74726976` and 2, the only layout this kernel's driver crate reads - and
**version is a choice the harness has to make**: measured, `virt`'s
`virtio-mmio` transports answer version **1**, the legacy register map, unless
QEMU is told `-global virtio-mmio.force-legacy=false`, which is now in the
command line beside `acpi=off` and for the same kind of reason. And **the tree
cannot say which window carries a device**: QEMU publishes all thirty-two nodes
whether or not anything is attached (measured by decompiling the tree with and
without `-device virtio-rng-device` - the two differ only in the firmware's own
random seeds), and the device does not land in the first of them either; it
answers at `0xa003e00`, the *last*. So a driver must read each transport, which
is the whole reason the check reads them.

Stage 6a is the last four lines of the log, and it is a change of kind rather
than of degree: everything above this stage ran at EL1, and this is the first
time this kernel has been *below* itself. What makes EL0 different is not the
privilege level as such but where the address space comes from - a program's code
and stack are in the half `TTBR0_EL1` names, the kernel's are in `TTBR1_EL1`'s,
and the two registers are what this stage is the first to tell apart. Until now
both pointed at the *kernel's* root, which worked only because stage 5b had left
the low half empty; so a user space is not an arrangement imposed on the kernel,
it is the state the kernel was already in being handed to somebody else. The
check is built the way stage 1c's and stage 2's were - the smallest program that
can be wrong in every way this mechanism can, and nothing that can only be wrong
for a reason the mechanism does not have yet. That program is forty bytes: it
puts the answer page's address in a *callee-saved* register, writes its own value
through it, calls a number no system call has, writes the answer back through the
same register, and leaves through `exit_group`. The verdict is read out of a page
the *program* wrote, because a trap is only observable by the code that answers
it and the honest way to check an answer is from the side that receives it: the
kernel reads back `0xaaaa` (the value the program wrote before it called
anything) and `0xffffffffffffffda` (`-ENOSYS`, the value the kernel put in `x0`
and the program read back). Nothing on the kernel side can fake either number.

**One measurement came out of that and changed the gate, and it is a difference
between the two machines rather than a bug.** `ELR_EL1` for a Supervisor Call
already holds the address of the instruction *after* the `svc`, so a gate that
steps over it - which is what the x86_64 twin's shape suggests, where `SA_RESTART`
subtracts the two bytes back - skips an instruction the program never got to run.
The first run of this check did exactly that, and the machine said so: the `svc`
is at `0x200010` and the trap reported `pc` `0x200014`, so the program's next
instruction (the store of the kernel's answer) was stepped over and the page came
back with a zero where the gate's answer should have been. The gate does not
advance `pc` at all now, and that is written down beside it.

Stage 6b is the two `space:` lines before `M6_AS_OK`, and it is the difference
between an address space a program can be *put into* and one it can *live in*.
What 6a did not have is any way to change a space once it existed, and that is
most of what a `fork`, an `mmap` and a `munmap` are made of: taking a mapping
back, changing one's permissions in place, walking what is there, and the
copy-on-write half that lets two processes share a frame until one of them writes
it. All of it lands in `paging.rs`'s `UserSpace`, and the encoding does not change
at all - what changes is that the file now writes *and erases*. The signatures are
deliberately `crate::hal::AddressSpaceOps`' rather than the type's own, because
writing them to the trait's shapes is what will make the impl delegation when
`hal` reaches this machine; the trait itself cannot be implemented yet, since
`crate::hal` is x86_64-only until all six of its concern traits exist.

Three things in those two lines are worth reading. The resolved frame,
`0x43476000`, is a frame the *child* space took for itself when it first wrote a
page it was sharing with the parent - and the parent's frame, `0x4346a000`, is
printed beside it because "the child copied" and "the parent was left alone" are
two claims and only one of them is about the child. Four frames is what a space's
tables cost here (a root and three levels down to the page), and dropping an
*active* space gives all four back and not the page it mapped: measured as an
exact count, because both ways of getting it wrong are off by a specific number,
and a leak would otherwise look like a machine that simply works.

The check reads every answer back through three different doors, and that is
deliberate: the space's *own* tables for what the kernel intends, a store to a
user address with that space switched in for what the MMU enforces, and the
frame's own bytes through the direct map for what actually happened. An address
space is exactly the thing where the first door alone is not enough - 6a's own
measurement, where a table the kernel had written was not the table the CPU
walked, is the same lesson one register down. It also has to be able to *fail*,
so all three of the mistakes it exists to catch were made on purpose before this
stage was accepted: a `resolve_cow` with the copy removed, a `Drop` that leaked
its tables, and a copy-on-write page marked without the read-only bit. Each one
stopped the boot with its own `M6_AS_FAILED` line naming the claim that broke -
and the first run of the check, on code that was correct, failed on the check's
own arithmetic instead (three pages left where it had counted two), which is why
the count it prints is a count rather than a list of addresses.

The `plan:` line is the strongest claim either machine's boot makes about its
own page tables: every mapping `mm::vm::kernel_mappings` names is walked page by
page and each page is asked to translate to the frame and permissions the plan
gave it - so a disagreement is a failure whichever side is wrong, and the direct
map coming back `rw` is the same plan that gives x86_64's `rw`.

The `-kernel` bring-up of 1b-1d is **deleted**, not left beside this: the
`aarch64-unknown-none` block is `relocation-model=pic` + `-pie` like the x86_64
one, the linker script has no base address and no `.stack`, and
`src/board/aarch64.rs` is an `extern "C" fn _start(x0)` with no assembly stub.
Nothing above `arch` is ported yet; `src/lib.rs`'s header carries the stage
ledger, which is the record of which module un-gates when.

This section used to say "kernel ELF compiles but boot flow incomplete", which
was false in both halves. Nothing compiled for aarch64 - V-8 (§11.1.8) removed
the `[target.aarch64-unknown-none]` block that named a linker script which had
never existed - and "incomplete" hid the real shape of the work, which is not
one boot sequence but a second ABI. Measured: the x86-specific surface is
*concentrated* (port I/O in seven files, no ACPI consumer, no IOAPIC, no HPET,
no i8042), but `src/syscall/linux/` is 11028 lines and `src/drivers/` 14481, and
that is the long tail, not the machine layer. `src/` also carried only 17
`target_arch` gates, so everything else was gated on `target_os = "none"` alone
- true for both bare-metal targets - and tried to compile for aarch64 and could
not.

So the order below is the measured one. Each stage is its own commit and is
verified by the means named; none of them claims a green from compilation alone.

**Why stage 2's user-entry half is at stage 6.** The table used to put
`UserFrame` (`x0..x30`/`pc`/`pstate`) and the `eret` that enters EL0 in stage 2
with the kernel switch. They moved, and the reason is not tidiness: every field
of that struct gets its order and its meaning from `user_regs_struct`, which is
x86_64-only until stage 6, so a frame written at stage 2 would be a struct
nothing could check against anything - and its `eret` sequence has no user
address space to enter until stage 6 builds one. Writing them early would have
produced exactly the shape this port refuses: code that looks finished and has
never been run. What stage 2 keeps is the half that *is* checkable at this stage,
and it is checked - the boot prints `M2_CONTEXT_OK`. **Stage 6a is where both
arrived, and neither was written before it could be run**: the frame's field
order is the ABI's own `user_pt_regs`, and the `eret` enters a space that the
same stage builds and the same boot walks. The aarch64 numbers are not the
x86_64 ones - `exit_group` is 94 here against 231 there - so a `nr.rs` written
at stage 6a would still have been a table nothing could check against; the one
number the gate answers is spelled beside it and named as such.

| # | Stage | Verified by |
|---|---|---|
| 1a | Build plumbing: `rust-toolchain.toml` targets, `src/arch/aarch64/linker.ld`, `[target.aarch64-unknown-none]`, the gate list and the stage ledger | **landed**: `cargo build --target aarch64-unknown-none` links; the x86 gate stayed green |
| 1b | The machine body: PL011 at `0x0900_0000`, the `_start` stub, the banner | **landed**: `qemu-system-aarch64 -M virt -kernel` printed the banner - the bring-up instrument 1e deletes, named as such at the time |
| 1c | Exception vectors (`VBAR_EL1`), GICv2, the generic timer (`CNTPCT_EL0`/`CNTP_CVAL_EL0`) | **landed**: `M1_TIMER_OK: 25 interrupts in 100 ms (~25 at 250 Hz)`, three runs of three, and the check's failure branch names the timer's, the distributor's and the CPU interface's registers rather than hanging |
| 1d | 4-level page tables, `TTBR0_EL1`/`TTBR1_EL1`, `TCR_EL1`/`MAIR_EL1`/`SCTLR_EL1` | **landed**: `page tables: 12 frames (48 KiB)`, `M3_TABLES_OK` (image `r-x`/`r--`/`rw-` at both its addresses, direct map written and read back, null page absent), and the clock still counts to 25 with the MMU on. **What this row cost first, and is now a machine-wide rule:** the bring-up carried the page-table arena as a `static` array with a zero initializer and no writer — which lands it in `.rodata`, read-only on a machine whose page tables enforce it, and cost 128 KiB of `.rodata`; it was deleted with the `-kernel` path. Stage 2's task stack was the same class twice over: its first `stp` took a data abort *with the fault address inside the kernel image*. The fix is not a section attribute — it is to take frames from the allocator, which is what the code wanted anyway. `-C link-arg=-zmax-page-size=0x1000` does not save you either; it belongs to the list of things that look like a fix for a wrongly-placed static and are not. |
| 1e | The aarch64 loader: `boot/src/vm/aarch64.rs` (the descriptor format, `MAIR`/`TCR`/`SCTLR`), `boot/src/main.rs`'s aarch64 `enter` and PL011, `boot/src/elf.rs` learns `EM_AARCH64`/`R_AARCH64_RELATIVE`, the kernel relinked as a higher-half PIE, `src/arch/aarch64/paging.rs` becomes a reader of the loader's tables, `cargo xtask boot-aarch64` | **landed**: AAVMF boots the real ESP image, the kernel reaches the higher half and prints `M3_TABLES_OK` (the loader's `TTBR0`/`TTBR1`, the image's W^X, the direct map written and read back, the PL011 device) and `M1_TIMER_OK: 25 interrupts in 100 ms`; the `-kernel` instrument is deleted |
| 1f | The kernel's own tables: `src/arch/aarch64/paging.rs` regains a builder, built from `mm::vm::kernel_mappings(info, sections)` out of frames from `mm::alloc_frames`; `mm::init` runs on aarch64; the boot switches `TTBR0_EL1`/`TTBR1_EL1` onto it | **landed**: `pmm: 5 zones, 49209 frames`, `page tables: 166 frames (664 KiB)`, `plan: 7 mappings, all 63616 pages translate as the plan says (image r-x / r-- / rw-, direct map rw-)`, `no identity map: 0x44bcf000 is unreachable, only the device window lives low`, then `M3_TABLES_OK` and `M1_TIMER_OK` |
| 2 | Kernel contexts: `src/arch/aarch64/context.rs` - `Context` with the callee-saved `x19..x28`/`fp`/`lr` on the task's own stack, `FpuState` (`q0..q31`, `FPCR`, `FPSR`), `bootstrap` and its trampoline, `switch` | **landed**: `M2_CONTEXT_OK: 13 hand-overs, both directions, every callee-saved register and v31 back where its task left it`, and the letters above it alternate `ABABAB` |
| 2b | *Not a stage* - the **user** frame (`x0..x30`/`pc`/`pstate`) and its `eret` entry moved to stage 6, where the ABI that gives those fields their order and their meaning is. See the note below the table | - |
| 3 | Un-gate `sched` (the TLS `MSR` write moves behind `hal`/`arch`) | `cargo build --release --target aarch64-unknown-none` with `sched` in - and see the correction below the table: this row is **not** one edit, and it waits on 4, 5 and 6 |
| 4 | Un-gate `console` and `vfs` (serial-only console first) | a ramfs root with the initramfs untarred |
| 4a | The first cut into the knot above `hal`: the serial console. The sink moves behind a new baseline `hal::Console` (`init`/`write_bytes`/`interrupt_driven`) with the interrupt-TX-ring behind an optional x86-only `hal::SerialRing` (`tx_space`/`tx_submit`/`tx_wait_now`/`tx_stop_waiting`), mirroring `Msi`/`FpuTrap`/`Segments`; `console.rs`'s framebuffer paint, ring-mirror and queue-and-park loop move verbatim into `#[cfg(target_arch="x86_64")]` helpers (inert elsewhere); aarch64 implements `Console` over the polled PL011 (CRLF at the sink, `interrupt_driven()`=`false` until stage 5); `console_list` recognises the aarch64 serial wire `ttyAMA0`; `console` and `print!`/`println!` un-gate to `target_os="none"` | **landed**: `cargo clippy --target aarch64-unknown-none` compiles `console` (the first subsystem above the machine layer to build for aarch64); `cargo xtask test`'s 142 user tests still exec through `console::write_bytes` and the parked/flood TX path (the behaviour-identity pin for the rewire); `cargo xtask boot-aarch64` still prints ten markers; host `console_list` tests pin `ttyAMA0` and `ttyAMA1`-unknown; depgraph holds at 12 pairs (`console -> hal` is one-way new, the `drivers`/`sched` names moved inside `#[cfg]` bodies and were already one-way) |
| 5a | The device tree: `crates/fdt` (the reader, host-tested), `Handoff.dtb_phys`/`dtb_len` appended after `regions`, the loader copying the firmware's blob out of the UEFI configuration table, and `arch::aarch64::check_devices` holding the tree up against the machine's own constants | **landed**: `cargo xtask boot-aarch64` prints `devices: PL011 at 0x9000000, GIC at 0x8000000/0x8010000, timer on PPI 14 (INTID 30)`, `devices: 32 virtio-mmio windows, all inside the 0x8000000..0xc000000 device window` and `M4_DEVICES_OK`, and the tree's memory node is compared with every region the record calls usable |
| 5b | The device window in the kernel half, and the tree's transports read back: `paging::DEVICE_PHYS`/`DEVICE_VIRT` split, `boot/src/vm.rs` mapping the window at both, `pl011`/`gic` compiled against the alias with their physical bases kept for the tree check, and `arch::aarch64::check_virtio` reading every `virtio,mmio` window's MagicValue/Version/DeviceID/VendorID | **landed**: `M3_TABLES_OK` now stands under `no low half: ... and so is the device window at 0x8000000 that used to live there`, and `M5_VIRTIO_OK` under `virtio: 32 transports, every one answering virtio 1.0 through the kernel half; 1 carries a device`, across three consecutive green runs |
| 5 | Drivers: virtio-MMIO found through the tree, PL011 as the serial device, GIC line routing | virtio-blk and virtio-net up - **the discovery half of this row is 5b and is done; what remains is the drivers themselves**, whose transport is `virtio_drivers::transport::mmio` reached at `paging::device_virt(phys, len)` |
| 6a | The entry to EL0: `paging::UserSpace` (a root of its own for `TTBR0_EL1`, user page mappings, the switch that installs it), `context::UserFrame` (`x0..x30`/`sp`/`pc`/`pstate`, in the ABI's own `user_pt_regs` order) with `enter_user` and `leave_for_kernel`, and the vector entry for a trap from a lower EL - which saves the whole register file and can *return*, unlike the fatal synchronous one | **landed**: `EL0: root 0x4346c000 active, program at 0x200000 (40 bytes), stack at 0x210000, answers at 0x220000`, `EL0: the program wrote 0xaaaa, called a number no system call has, read 0xffffffffffffffda back and left with status 7` and `M6_EL0_OK`, across four consecutive green runs |
| 6b | The address-space operations, in `crate::hal::AddressSpaceOps`' shapes: `map`/`try_map`/`unmap`/`protect`/`map_cow`/`map_shared`/`resolve_cow`/`walk_user_pages`/`set_cow_bit`/`set_cow_bit_quietly`/`flush_if_active`/`phys_for_virt` and the `Drop` built out of `unmap`; the copy-on-write marker in descriptor bit 55 (software-reserved there, `RES0` to the walker) with the read-only bit the CPU enforces set beside it, and `USER_END` as `TTBR0`'s own span rather than x86_64's bit-47 split | **landed**: `M6_AS_OK` under `space: 4 pages mapped, walked in order and read back through the MMU; ...` and `space: dropping an active space gave back all 4 frames its tables took`, across three consecutive green runs - and each of the three mistakes the check exists to catch (a resolve with no copy, a `Drop` that leaks its tables, a marker set without the read-only bit) was made on purpose and stopped the boot by name |
| 6c | The `hal::Paging` methods that need a *running* process, at module level so the `impl` is delegation: `active_root`/`kernel_root` (the `TTBR0`/`TTBR1` statics), `switch_to_root`/`switch_to_kernel` (over the private `load_root0`), `phys_for_virt_of`/`user_phys_for_virt_of` (over `walk_from`, the second gating on `USER`), `user_accessible`/`is_cow_page` (over the leaf walk + bit 55), `write_user_u32` (through a named root, no live switch), and `map_mmio` (asserts the fixed window covers the range - `virt` has no dynamic MMIO) | **landed**: `M6_PAGING_OK` under `paging: active/kernel roots told apart; ... the copy-on-write marker the CPU cannot see; a u32 written through a space's root and read back from its frame; leaving to the kernel and re-installing` - each claim read back against the hardware it names, and a kernel-half address rejected by `user_accessible` because a user pointer is a `TTBR0` address |
| 6d | The `hal::WallClock`: a PL031 driver reached through the kernel-half window (`0x0901_0000`), `boot_epoch` anchored to a read of the counter at init, `set_boot_epoch` moving the software anchor, `write_epoch` refusing (the counter is host-shadowed and read-only on QEMU's model, so there is no persistent register) | **landed**: `M6_WALL_OK` - and the design changed by measurement: the plan said this machine had no wall source, but reading `RTC_DR` at boot gave the host's real UTC epoch (`CR=1`, already counting) and a programmatic write did not stick, so `boot_epoch` reads it like x86 reads the CMOS and `write_epoch` answers `false` rather than pretending |
| 6e | The `hal::Cpu`/`hal::Interrupts` primitives the machine lacked: `enable_user_simd` (`CPACR_EL1.FPEN=0b11`, read-modify-write), `idle` (`daifclr;wfi;daifset`), `gic::disable` (clear `GICD_ICENABLER`) and `is_enabled`, the serial line's `SERIAL_INTID=33`, and the `Line -> INTID` mapper | **landed**: `M6_CPU_OK` - FPEN read back, the serial's distributor bit set then cleared and read back, and `idle` woken by an armed timer tick (28->29) returning with interrupts masked |
| 6f | `hal` compiles for aarch64: the arch-neutral split moves x86's optional capabilities (`Msi`, `FpuTrap`, `Segments`) out of the baseline traits, `Current` un-gates to `target_os = "none"`, and `arch/aarch64` writes `impl {Clock,WallClock,Cpu,Interrupts,Paging,Contexts,Platform} for Current` plus `AddressSpaceOps for UserSpace`, the ops traits, and a full-register `enter_frame` that `check_el0` and the trait `enter_user`/`resume_user` share | **landed**: `cargo clippy --target aarch64-unknown-none` compiles `hal` for aarch64 (the deliverable; the debug aarch64 *build* cannot link, so clippy is the check - `AGENTS.md` §3), boot-aarch64 still prints all ten markers, and the x86 gate + depgraph ratchet are unchanged. The `to_user_regs`/`from_user_regs` `user_pt_regs` field-order pin and the COW-fault resolver wait on stage 6's ABI caller and stage 3's scheduler - named below, not stubbed |
| 6g | The kernel's own ELF loader honours the machine it is given: `mm/elf.rs` reads `e_machine`, accepts `EM_X86_64`/`EM_AARCH64` (refuses any other, where it used to validate nothing), selects the arch's `R_*_RELATIVE` kind from the file rather than hardcoding `R_X86_64_RELATIVE`, and reports `Image.machine` so the per-arch `execve` caller can reject a foreign-but-known ISA. An aarch64 static-pie now has its relocations read instead of silently dropped | **landed**: host unit tests `an_aarch64_image_gets_its_relative_relocations_read`, `an_image_for_an_unknown_machine_is_refused`, `a_file_that_names_no_machine_is_refused`, `an_x86_64_image_reports_its_machine` pass (509 unit tests total), and `cargo xtask test`'s 142 user tests still exec real x86 binaries through this parser - so the x86 path is unchanged and the aarch64 path is pinned by a test that fails on the old type-8-only filter. `USER_LIMIT` is still x86_64's `1<<47` (aarch64's user span is `paging::USER_END`); taking it from the machine is a named follow-on, conservative for now because aarch64 statics place far below it |
| 6h | The `hal::Cpu` thread-pointer capability the scheduler needs to un-gate: `set_thread_pointer`/`thread_pointer` join `Cpu`, x86's `IA32_FS_BASE` `wrmsr` moves out of `sched`'s private helper into the x86 `Cpu` impl (behaviour identical), and aarch64 writes `TPIDR_EL0`. This is the first slice of row 6's "`TPIDR_EL0` TLS" - landed because `sched` could not name an x86 machine register and still compile here. The `Cloexec` inversion and the arch-neutral ABI-const leaf are the other two things standing between `sched` and the aarch64 build, and land beside it. | **landed**: `cargo clippy --target aarch64-unknown-none` compiles the `Cpu` impl with the two new methods; `M6_TLS_OK` under `tls: set_thread_pointer(0xdeadbeef00001234) read back through thread_pointer(); cleared to 0` (a sentinel the register must echo, then a clear the register must take - the write is not trusted until it reads back); the x86 behaviour-identity pin is that `cargo xtask test`'s 142 user tests still pass with `sched` now reaching FS_BASE through the forward, depgraph holds at 12 pairs (`sched` no longer names an `msr`), and boot-aarch64 now prints twelve markers |
| 6 | The rest of the aarch64 Linux ABI: `nr.rs` per arch (the numbers are the machine's - `exit_group` is 94 here and 231 on x86_64), signal frames, `TPIDR_EL0` TLS, `PTRACE_GETFPREGS`'s view of `FpuState`, the `Paging` methods that need a running process, and the `hal` impls those two complete | the `t-*` conformance programs run on aarch64 |
| 7 | aarch64 userspace and the image: `aarch64-unknown-linux-musl` builds, ESP layout for AAVMF, arch-aware `xtask` | `cargo xtask test` passes on aarch64 |
| 8 | The gate: `xtask test`/`run` grow an arch selection | a green aarch64 gate quoted with its summary line and exit code |

**The order below the machine layer is not the order of the table, and that was
measured rather than argued.** Row 3 reads as one edit - "un-gate `sched`: the
TLS `MSR` write moves behind `hal`/`arch`" - and it is not. Widening that gate
alone and building reports **50 errors, of which three are the `wrmsr`**; the
rest are 23 missing `vfs`, 9 missing `syscall`, 7 missing `hal`, 2 missing
`drivers` and a `println!`, because `sched` names all four directly. So `sched`
cannot be un-gated before 4 (`vfs`), 5 (`drivers`) and 6 (the ABI) have landed,
and `hal` - which `sched` wants for the TLS write and which everything else
wants for the machine - cannot compile for aarch64 until all six of its traits
exist, which needs stage 6's `Paging` user half and a `WallClock` this machine
has no driver for. The next work is therefore **5, not 3**, and the table's
numbering is the plan's order rather than the order it can be done in.

**And stage 5's drivers turned out to be on the same side of the knot**, which
is stage 5b's own measurement rather than a guess: un-gating `drivers` alone is
**190 errors** whose long tail is `crate::vfs` (eight imports, from the device
nodes, `uinput`, `rtc_dev` and the DRM card), `crate::uaccess`, `crate::sched`
and `mm::map_mmio`; un-gating `console` alone is **18**, and they are
`drivers::serial`, `drivers::video::render` and `sched`, so the console cannot
move without the drivers it writes through. The only cut in the knot is `hal`,
and the only thing standing between `hal` and compiling is the *user* half of the
machine - which is stage 6. That is why 6a and 6b came next rather than 5's
drivers, and why the drivers' turn is after the rest of 6 rather than before it:
the tree walk 5b landed is the half of "virtio-MMIO found through the tree" that
does not need `vfs`, and the transport that opens a device is the half that does.

**That cut has since landed, and the console side of the knot is now cut too.**
`hal` compiles for aarch64 (stage 6f), and stage 4a routed the console's sink behind
a new `hal::Console` baseline capability (its interrupt-TX-ring behind an optional
x86-only `hal::SerialRing`), cfg-partitioned the framebuffer/park arms, and un-gated
`console` + `println!` to aarch64 - so the 18 errors above are gone. The framebuffer,
ring and parking arms stay x86-only by `#[cfg]`. **Stage 4b then closed the one pin
4a left open** - a boot-observed `M4_CONSOLE_OK`. It did *not* need to relocate the
milestone checks: `arch::aarch64::boot` gained a `milestone: fn()` argument it calls
once the PL011 is up, and `board/aarch64.rs` hands in a function that writes through
`crate::console` (`init` + two `println!`) - so `arch` only invokes a pointer and the
`console` name stays on the `main` side of the seam (no `arch -> console` edge). It is
the first log bytes this machine sends through the console service rather than the
machine's own `pl011::Writer`, printed early (before `M4_DEVICES_OK`), so it is a
required marker and not a terminator - EL0's `M6_EL0_OK` stays last. `boot-aarch64`
now prints twelve markers (stage 6h's `M6_TLS_OK` joins the `M6_*_OK` capability family
between `M6_CPU_OK` and `M6_EL0_OK`). `cargo xtask run`/`test` still build and boot x86_64 only;
the next cut into the knot is a subsystem above `console` (`vfs`/`sched`/`drivers`),
each its own measured stage.

**Stage 5's terminal un-gate is DONE: `sched`+`vfs`+`uaccess` now compile for
aarch64** (`cargo clippy --target aarch64-unknown-none` is clean, and the x86 gate
is byte-identical - `cargo xtask test` still 142/142). Measured, not argued: the
knot is strongly connected, so the only observable test was flipping all three gates
and reading the compiler. The last blocker after the leaf/`MMAP_BASE`/lazy-FPU work
was the ptrace/stop register-snapshot subsystem (`group_stop`/`exec_trap`/`fork_trap`/
`park`/`trace_regs`, and the `[u64; uregs::COUNT]` field on `Tracing`). My first read
judged it a stage-6/7 dependency; a second, careful read of the *call graph* showed it
runs one-way into the gated x86 syscall/`proc.rs` + `#NM` layer - aarch64 has neither
the `svc` trap nor ptrace yet, so those functions have **no caller** on aarch64. Gating
the whole block to `target_arch = "x86_64"` (the field, the uregs-typed fns, and
`attach_new_child`/`fork_event`, whose core fork call-site takes the honest
no-tracer-equals-ready path) drops `sched -> syscall` to 0. This is not a faked
aarch64 ptrace: the functions are *absent*, not stubbed to return success, and the
uregs-free state (`Stop`, `task.trace`, core `wait`) stays for both machines. aarch64's
real stop/signal-frame/ptrace view still arrives with stage 6/7's `svc` gate +
signal-frame ABI - only now `sched`/`vfs`/`uaccess` build without it, so the syscall
stage lands on a machine whose scheduler already compiles.

**Superseded, and the later cut is the better answer** (§13.2). The gate above was an
interim: it made the edge 0 *on aarch64* while leaving it 1 on x86_64 and leaving `sched`
naming an architecture. What the block actually depended on was the *shape* of a register
file that only the x86 ABI table described. With the width moved to the machine
(`hal::Contexts::ABI_REG_COUNT`) and the `PTRACE_O_*`/`PTRACE_EVENT_*`/`SYSCALL_TRAP`
protocol words moved to the `abi` leaf beside the clone flags, the gate is gone, `sched`
names no architecture, and `sched -> syscall` is 0 on **both** machines - not only on the
one that never had a ptrace handler. The un-gated block still fakes nothing: a machine
whose syscall layer has no `PTRACE_ATTACH` has no tracer, so `attach_new_child` answers
"no" and the child simply goes ready.

**Stage 6A's prep cut is DONE: the whole `syscall` module now compiles for aarch64.**
Flipping `pub mod syscall`'s gate to include `aarch64` yields a clean
`cargo clippy --target aarch64-unknown-none` (warnings only) with the x86 gate
byte-identical (`cargo xtask test` still 142/142, host 511, boot-aarch64 twelve
markers, depgraph within ratchet). The 101 errors the first flip measured resolved
through ten `#[cfg(not(target_arch = "aarch64"))]` preps, each a provable no-op on
host (x86_64) and x86 bare metal where the predicate is simply true. What is deferred,
and why it is refused rather than faked: the socket arms and the `'K'`/`'V'`/`'E'`/`'U'`
and `SIOC`/`FBIO`/DRM ioctl families fall to `ENOTTY`/`ENOSYS` (no `net`/`drivers` on
this machine yet); `ptrace`/`rt_sigreturn`/`reboot`
dispatch arms fall to `_ => ENOSYS` (they rewrite the x86 return frame, which arrives
with the aarch64 signal-frame ABI; aarch64 has no `fork` number at all - a libc's
`fork()` is `clone(SIGCHLD)`); `process_vm_*` and `rt_sigtimedwait` were listed with
them until §13.2 un-gated both - neither rewrites a frame, and the lumping was the
stale-gate error this phase exists to find; `execve` and `clone`/`wait4` are now **served** on
aarch64 (stage 6D: both handlers were already arch-neutral through
`sched::exec_replace`/`sched::clone`, and only the x86 return-frame edges were split -
`UserFrame.pc/sp/x[0]` and the aarch64 `user_context` instead of `RegsFrame`), so a
program can `execve` a new image, `clone` a child, and `wait4` to reap it at EL0, pinned
by the `MPOS_EXEC_OK` and `MPOS_FORK_OK` markers; `arch_prctl` has no aarch64 number at all; the
seccomp *filter* path (reads the x86 RIP) and on-exit signal delivery in
`linux_dispatch` are gated to their no-signal identity (the ptrace entry/exit stops
were gated here too, until §13.2's register-ABI stage made the frame operations
machine ops - they now run on both machines and are no-ops wherever no tracer is
attached);
and `uname`'s machine field and the hostname/domainname were moved to per-arch and the
`uts` leaf so those always-live calls answer truthfully. The remaining stage-6A work is
the per-arch `nr.rs` (arm64 is a *different syscall set* - no `open`/`stat`/`fstat`/
`poll`/`select`/`fork` legacy numbers, and every shared number differs from x86) plus
routing `arch/aarch64/vectors.rs`'s `el0_sync` `svc` into `linux_dispatch` on the
aarch64 `UserFrame`, which is what makes the module *used* rather than merely built;
the gate stays x86-only until that lands so no commit ships with `syscall` compiling
but unreachable.

The per-arch `nr.rs` **has** since landed (prep 11): `nr.rs` selects a whole table by
`#[cfg(target_arch)]` via `include!` (so it stays the single file the `abitest` harness
names) - x86_64 keeps its body verbatim and its one-per-name test pins, aarch64 gets the
resolved asm-generic numbers, a different *set* rather than a renumber, with distinct
sentinels above any real number for the calls arm64 lacks so the shared `dispatch` arm
compiles yet can never be matched. With the gate flipped the whole module still compiles
clean. But routing `el0_sync`'s `svc` into `syscall::dispatch` was tried and **panicked
the boot**: `dispatch` opens on `current_is_seccomp_strict`, a scheduler read, and the
EL0 milestone runs on the boot stack *before* the scheduler has a current task ("there is
no task 63"). That is an ordering fact, not a defect to paper over. So the gate keeps its
honest inline `ENOSYS` refusal - `M6_EL0_OK` still pins the trap end-to-end by reading
`-ENOSYS` back - and the routing plus its runtime pin arrive *with* a task to answer
from: folded into the init-handoff stage (6E), which is exactly when the first real
userspace task exists, rather than a synthetic pre-scheduler probe. The `svc`->dispatch
edge is therefore no longer a standalone 6A item; the module is live and correctly
numbered, and the gate is opened to aarch64 as part of getting a program to run.


**6E reached a running aarch64 program.** `xtask/src/aarch64_init.rs` hand-encodes a
static `ET_EXEC`/`EM_AARCH64` `/init` whose whole program is `exit_group(0)` - four
instructions, every word pinned against `llvm-objdump` in a host test rather than a
cross-compiler at build time (which would make the artifact non-hermetic and add a
`clang`+`ld.lld` host prerequisite to the boot path) - and `build_esp` packs it ustar
as `\initramfs.tar`. `boot` now threads a `main`-side init handoff (`init_handoff`)
as a continuation that `el0_returned` runs after `M6_EL0_OK`: it starts the scheduler,
mounts a ramfs root, unpacks the archive, loads `/init` on pid 1 and runs it to
completion. Because `enter_frame` is terminal, control only comes back to EL1 inside
`el0_returned`, so the handoff is passed the direct-mapped initramfs pointer+length
read out of `BootInfo` *before* the address-space switch (re-reading the loader's raw
record pointer after `paging::install` faults - the identity address is not in the
kernel's tables). `cargo xtask boot-aarch64` now ends
`initramfs: 1 members unpacked` / `mpos: aarch64 machine handed to /init` / `M6_INIT_OK`
with no exception: the kernel booted AAVMF, ran a real userspace program at EL0, routed
its `exit_group` into the shared `syscall::dispatch` table, and reaped it. Thirteen
markers, x86 `cargo xtask test` still 142/142. The named deliverable `cargo xtask
run --arch aarch64` now exists too (interactive serial via `mon:stdio`, reusing the
one `aarch64_qemu` builder `boot-aarch64` uses); verified headless it streams the whole
boot to `M6_INIT_OK` on the terminal. An *interactive shell* is still gated on the
drivers stage (serial RX + the park/idle path below), not on this command.

**The idle/park switch is now sound (fixed the stage-6 boot fault).** The old follow-on
said parking to the idle task "faults on an uninstalled idle kernel stack." Measured, that
was wrong about the cause: the fault is `sched::Table::enter`'s `set_kernel_stack` step
emitting `msr SP_EL1, Xt`, which is **architecturally undefined while `SPSel` selects
`SP_EL1`** - and that is exactly how this kernel runs (`EL1h`, `SPSel`=1, confirmed by the
`SPSR_EL1.M` = `0b0101` the fault carried, `EC 0x00` at `ELR` inside `enter`, `far`=0). The
dedicated `MRS/MSR SP_ELx` can only reach the *non-selected* stack, so writing the one the
kernel is on traps as an undefined instruction. AArch64 has no separate `TSS.rsp0` to arm
here: the trap-landing stack *is* the current `sp`, and `context::switch` already sets it to
the incoming task's saved kernel `sp` (`mov sp, x2`) before that task is ever dropped to
EL0, so `set_kernel_stack` is both undefined and redundant on this machine. Making it a
no-op removes the fault (verified: the park that used to abort now switches boot→idle→boot
cleanly, and the boot reaches its terminal halt with no exception). An interactive shell is
still gated - but only on the genuine prerequisites, serial **RX** (stage 7) and a `/init`
that does not exit one-shot - not on this switch any more.

**Stage 7a's first cut is DONE: the PL011 receive half reads, pinned by a loopback
probe.** Every marker through 6E was about *transmit* - the boot talks, it does not
listen. `arch::aarch64::pl011` now has a non-blocking `read_byte` (polls `FR.RXFE`,
returns `Some`/`None`) and a `loopback_echoes` that sets the UART's `CR.LBE` (which ties
TX back to RX inside the PL011 and takes the wire output off) and checks that a written
probe byte comes back through `DR`. `check_serial_rx` runs between `check_cpu` and
`check_tls` and prints `M6_SERIAL_RX_OK`/`_FAILED`; `M6_SERIAL_RX_FAILED` joins the
`TERMINATORS` list (its `_OK` is not a terminator - the TLS/EL0/init checks come after).
**The rule `TERMINATORS` obeys, learned four times over** (`xtask/src/aarch64.rs`): the list holds
the *last* check's success marker and *every* check's failure marker — never the success marker of
a check that another check comes after. A terminator stops the run, so a success marker listed too
early kills the machine mid-boot and the log simply ends, which is indistinguishable from a hang.
The measured history: the list once ended at `M1_TIMER_OK`, which stage 2's checks come after
(killed mid-line); stage 5a added `M4_DEVICES_OK`, which the timer and context checks come after —
a boot that finished in 5.7 s instead of the 60 s deadline and stopped three lines into a machine
that was working; stage 6a is the first check to land *after* the last one, so `M2_CONTEXT_OK` had
to come **out** when `M6_EL0_OK` went in; stages 6b/6c/6d/6e/6h each sit before 6a's EL0 check, so
their `_FAILED` markers are terminators while their `_OK` markers are not; stage 6E's handoff runs
after the EL0 probe, so `M6_INIT_OK` came out when it went in; stage 7a's serial-receive check sits
in the same pre-EL0 slot as 6h; stage 7b's console-read check runs after the EL0 probe but before
the init wait, so only its `_FAILED` is a terminator; and stage 7c made the handoff actually run
`/init` (waiting on `is_running`, not the always-false `has_live_child` — pid 1 has no parent), so
the terminal proof became the init's `INIT_EXITED` and `M6_INIT_OK` left the list for good. Elapsed
time is the tell: a stall is 60 s, a wrong terminator is seconds.
Boot-aarch64 is now **fourteen** markers. Measured on QEMU's model: the loopback echoes
synchronously, so the check is deterministic with no terminal. What this lays and what
remains for a typed shell: the RX *primitive* exists and is proven; the next cut wires it
into the tty/console read so a userspace `read(0)` returns typed bytes, then puts a shell
(a static musl `ash`) into the aarch64 initramfs - the `aarch64-unknown-linux-musl` target
is installable here and Alpine aarch64 packages are fetchable, so there is no environment
wall, only stages left to walk.

**Stage 7b is DONE: the receive path is wired through the tty, not just the UART.**
`check_serial_rx` proved `pl011::read_byte` reads the FIFO inside `arch`; this cut moves
the read up behind the designed seam. `hal` gains an optional `ConsoleInput` capability
(`poll_input() -> Option<u8>`, `inject_input` for the bring-up hook), implemented for aarch64's
`Current` over `pl011` and *not* for x86_64 (its serial receiver is interrupt-driven and hands
bytes straight to the tty, so it has no polled input to expose - refuse-don't-fake, the
capability is simply absent). `board/aarch64.rs` runs `check_console_rx`: inject a line through
loopback, pull each byte back through `hal::console_poll_input`, hand it to
`vfs::tty::global().receive_byte` (the step `vcmux::route_input` does on x86_64, which is
x86-only here), and read it back through `tty.read` - proving a userspace-visible read of
typed input end to end, printed as `M6_CONSOLE_RX_OK`. It also spawns the production input
*pump* task (poll the wire, feed the tty, sleep when idle) that will carry a real shell's
`read(fd 0)`. boot-aarch64 is now **fifteen** markers. What remains for an interactive shell:
a `/init` that reads `fd 0` in a loop instead of exiting, and a static-musl `ash` in the
initramfs.

**Stage 7c is DONE - and it corrected a false stage-6E proof: the scheduler now
actually runs `/init`.** 6E recorded `M6_INIT_OK` as "a real userspace program ran at EL0
via the scheduler". Measured this session with an `el0_sync` svc trace: it had not. The
handoff waited `while has_live_child(pid)`, but `has_live_child` asks whether the init has
live *children* - pid 1 has none (no parent) - so it was false on the first check and the
boot task finished the wait without ever yielding a turn; the spawned `/init` never
executed. The only program that had run at EL0 was the pre-scheduler `check_el0` *probe*
(direct `enter_frame`, `started()==false`). Fix: a new `sched::is_running(id)` (the slot's
own liveness) and the handoff waits on that, so the init is scheduled, enters EL0, runs.
To make the proof observable from the console, the hand-encoded `/init` now does
`write(1, "MPOS_INIT_ON\n", 13)` (ten `mov`/`movk`/`svc` instructions, each verified by
disassembling the built image with `llvm-objdump`) before `exit_group`. boot-aarch64 then
shows in order `M6_INIT_OK` (handed to pid 1), `MPOS_INIT_ON` (the init's own syscall
reached the wire - the first time a scheduler-run userspace program produced console output
on this machine), and `INIT_EXITED` (its `exit_group` routed to `sched::exit`). `INIT_EXITED`
- not `M6_INIT_OK` (which now prints before the wait) - is the last `TERMINATOR`. This is the
objective's core - "boots the kernel to a working userspace" - now genuinely demonstrated
end to end. The `/init` still exits one-shot; a typed shell needs a read-loop `/init` + a
real shell binary (stage 7 continues), but the run-and-output path is proven.

**Stage 7d is DONE: a scheduler-run userspace program reads console input and echoes the
bytes it read back out.** The hand-encoded `/init` now does `read(0, buf, 64)`, then
`write(1, "R:", 2)` and `write(1, buf, n)` - the *bytes it just read* - then
`exit_group(0)`: twenty-three `mov`/`movz`/`movk`/`svc` words, all verified through the
`llvm-objdump` encode/disassemble round trip. Writing back what it read (not a fixed
string) is what makes the proof unambiguous: the tty's canonical echo of the queued input
also prints `echo-test`, so only the init's own output carries the `R:` prefix. The boot
queues a line (`echo-test\n`) on the tty before the handoff (the same receive path stage
7b wired; `cargo xtask run --arch aarch64` supplies it live from typed input), so the
init's `read` returns real console input. boot-aarch64 now shows, in order: `echo-test`
(canonical tty echo of the queued input), `M6_INIT_OK`, `R:echo-test` (the userspace
`read(0)` returned the console bytes and the userspace `write(1)` echoed them through
`syscall::dispatch`), and `INIT_EXITED`. That is the full interactive path - typed bytes
in at EL0 via a userspace `read` syscall, the same bytes back out via a userspace `write` -
demonstrated on the second machine. What remains for a typed *shell* rather than this
read/echo proof: a `/init` that loops read/echo (or busybox `ash`, which additionally
needs the aarch64 `execve`/`clone` return-frame path - the named stage-6D gap), then the
`drivers` disk root and `net`.

**Stage 7e is DONE: `cargo xtask run --arch aarch64` is an interactive read/echo REPL.**
The image is now two hand-encoded programs sharing one assembler (`aarch64_init.rs`):
the one-shot above (the headless `boot-aarch64` verdict's, which exits so the marker
poll stops on `INIT_EXITED`), and a **looping** `/init` whose body is the same
`read(0)`/`write(1,"R:")`/`write(1,buf,n)` with `exit_group`/`b .` replaced by a branch
back to the top - twenty words, the backward branch derived from the code length so it
cannot desync, pinned by a host test that decodes the final word's `imm26` and asserts it
is `-(20-1)`. `build_esp` takes the image to pack; `run --arch aarch64` packs the loop,
`boot-aarch64` keeps the one-shot. The loop is why this belongs to `run` and not the
verdict: with no typist its *second* `read(0)` parks - `read_wait`'s canonical
`UntilInput`, woken only when a line lands - so `INIT_EXITED` never comes and a
marker-poll would hang; at a terminal it is a REPL. Verified headlessly by booting the
ESP under `-serial stdio` (plain, not `mon:stdio`, which swallows a piped monitor mux)
and feeding `hello\nworld\n`: the log shows the tty's canonical echoes `hello`/`world`
*and* the loop's own `R:hello`/`R:world` after the initial `R:echo-test` - a
scheduler-run userspace program that read a typed line, echoed it, parked, and did it
again. That is the objective's "reaches init/shell on the serial console" in the
minimal genuine form: a persistent interactive userspace loop on the second machine.

**And the loop is now a shell (§13.8/§13.9).** The REPL stand-in is gone.
`cargo xtask run --arch aarch64` builds an in-tree `/init`
(`aarch64_init::build_shell_init`) that makes `/tmp` and `execve`s `/bin/sh`, stages the same
multi-call `/bin` the sweep uses (`aarch64_bin`, now six applets), and drops the machine at a
prompt on the serial console. Three things had to land with it and none is cosmetic: the `sh`
applet became a line-at-a-time reader with a prompt (`is_terminal`, so a piped script stays
silent and an interactive session does not wait for `Ctrl-D`); `cat` gained its file
arguments, because a `cat` that copies nothing but standard input does not fail on a
terminal - it *blocks* on it, which is how a shell's first real command turned into a hang;
and the second machine's `uaccess` learned what the first machine's had always known, that a
buffer the process has *mapped* is a valid one even before the tables describe it, with the
EL1 fault entry that the copy then needs (§13.9). Measured, with the shell's own lines fed to
it over the serial line:

```
mpos: init handing over to /bin/sh
#
sh: echo-test: not found        <- the line the handoff queued, run as a command
# echo hello-from-guest
hello-from-guest
# ls /etc
conf.d  doas.conf  fish  inittab  local.d  network  profile.d  resolv.conf  xdg
```

Before §13.9 that session was one line long: `sh: debug err Bad address (os error 14)`,
because the shell's first `read(0)` - into its `BufReader`'s heap buffer, allocated and
unread - was refused as `EFAULT`, and it exited. **The userland around that shell has since
arrived** (§13.12): the machine installs and boots a real Alpine `aarch64` tree -
`busybox` init on `/etc/inittab`, `openrc` running its runlevels, a `getty` printing
`localhost login:` - with `/proc` and `/sys` mounted (the kernel serves both now; the mount
factory's architecture gate named a deferral that was over), a dynamic loader that works
(the machine's `USER_LIMIT`, not x86_64's, so a shared library's `MAP_FIXED` segments map),
and its console tty published under its own serial name, `ttyAMA0`. `cargo xtask
alpine-aarch64` is the verdict. What was still x86_64's was the *shape* of the console: the
virtual consoles `tty1`..`tty4` needed the `vcmux` port, and **§13.16 landed it** - the mux is
both machines', so a getty on `tty1` prints where a login can read it.

**Named gap now closed on the way:** `USER_LIMIT` was the follow-on §9.1 wrote down here -
"still x86_64's `1<<47`, aarch64's user span is `paging::USER_END`; taking the limit from the
machine is a named follow-on" - and it was not conservative, it was wrong: `MMAP_BASE` on this
machine (`0xff00_0000_0000`) sits **above** `1<<47`, so the first dynamically linked shared
library any program loaded made `handle_mmap` answer `EINVAL` to its `MAP_FIXED` segments
(`openrc`'s `librc.so.1: Invalid argument`). `mm::elf::USER_LIMIT` is now the machine's, and
`syscall` reads that one rather than a private copy.

**Stage 6D (execve) is DONE: `execve` transfers a running aarch64 program to a new
image at EL0.** The handler's body was already arch-neutral - it reads the path and
`argv`/`envp` through `uaccess`, checks `X_OK`, resolves any `#!` interpreter, honours
set-uid, and calls `sched::exec_replace`, which returns `(entry, new_rsp)` exactly as
`spawn_init`'s loader does (the same `new_layout`/`initial_stack_rsp`/`write_arg_block`
the aarch64 boot already proves). Only the *return-frame patch* was x86: it rewrote
`RegsFrame.rcx`/`user_rsp` for `sysretq`. That patch is now `#[cfg]`-split - x86 keeps
`RegsFrame`, aarch64 rewrites the saved `UserFrame`'s `pc=entry`/`sp=new_rsp`/`x[0]=0`
that `el0_sync`'s `eret` restores - and the `EXECVE` dispatch arm lost its
`#[cfg(not(target_arch = "aarch64"))]` gate so `svc 221` reaches it. Pin: the headless
verdict's `/init` is now an exec *launcher* that echoes the queued line (so `R:echo-test`
still proves the read/write path) then `execve`s a second hand-encoded image `/t` that
writes `MPOS_EXEC_OK`; `boot-aarch64` asserts the literal `MPOS_EXEC_OK` between
`R:echo-test` and `INIT_EXITED` (`INIT_EXITED` is now `/t`'s own `exit_group`, exec
keeping pid 1). Two bugs found and fixed while landing it, both honest failures the pin
turned red on: the `EXECVE` dispatch arm was still gated out on aarch64 (`svc` fell to
`ENOSYS`), and the packed initramfs files had no exec bit, so `may_access(X_OK)` denied
even root - `boot-aarch64` now `chmod 0755` the staged files.

**Stage 6D (clone/wait4) is DONE: aarch64 can spawn and reap a child process.**
aarch64 has no `fork` syscall number (`nr::FORK` is an unreachable sentinel); a libc's
`fork()` is `clone(SIGCHLD)`, so the real path is `clone`. `handle_clone`/`handle_clone_impl`
were already arch-neutral through `abi::validate_clone_flags` + `sched::clone`/`spawn_copy`
(which build the child from a `UserFrame` and start it ready, with no tracer to attach), so
only two x86 edges needed splitting: `user_context` now reads the trap `UserFrame` directly
on aarch64 (copy it, clear `x0` for the child) instead of reconstructing one from x86
`RegsFrame` + `user_segments`, and the post-`clone` tracer announcement is `#[cfg]`-guarded.
The `CLONE` dispatch arm lost its gate; `wait4` was already served. Pin: the exec target
`/t`, after writing `MPOS_EXEC_OK`, does `clone(SIGCHLD)` and a `cbnz x0` splits parent from
child - the child writes `MPOS_FORK_OK` and exits, the parent `wait4`s so the child's line
lands before the machine halts, then exits (`INIT_EXITED`). `boot-aarch64` asserts the
literal `MPOS_FORK_OK` between `MPOS_EXEC_OK` and `INIT_EXITED`. So the wire now shows the
full chain - read, echo, `execve` to a second image, `clone` to a third process, `wait4` to
reap it - all at EL0 through `syscall::dispatch`. `CLONE_VM`-without-`CLONE_THREAD` (the
spawn shape) and the tidptr writes are honoured; `CLONE_THREAD`/TLS-threaded `clone` and the
ptrace stops remain gated as the honest `ENOSYS`/no-tracer answer. With execve+clone+wait
served, a real shell (busybox `ash`) needs only a static-musl aarch64 `/init`/`ash` in the
initramfs; the disk root (`drivers`), `net`, and `cargo xtask test --arch aarch64` remain.

**Aarch64 EL0 page-fault / demand paging - DONE; the gap it explained is closed.** The
`aarch64-unknown-linux-musl` target installs and `rustc -C target-feature=+crt-static -C
linker=rust-lld` produces a real *unmodified* statically-linked `ARM aarch64` ELF (4 `PT_LOAD`s
+ a `PT_TLS`). First run of one: it reached EL0 and took a **data abort** (`ESR_EL1` EC `0x24`,
`pc` in the mapped text, `FAR_EL1` a wild value) before `main`. Root cause, found by reading the
EL0 vector rather than guessed: `el0_sync` had **no fault path at all** - any non-SVC EL0 trap
halted with "EXCEPTION: from EL0, and not a system call". x86's `page_fault` answers the three
non-error faults - a `fork` COW write, an untouched anon page, and a stack push past the mapped
end - via the arch-neutral `crate::sched` resolvers (`resolve_cow_fault`/`resolve_anon_fault`/
`resolve_stack_growth`); aarch64 never called them, so a real binary's first unbacked page was
fatal. The pinned hand-encoded images (`memsz == filesz`, tiny stack) never touched an unbacked
page, so this was invisible until a real libc ran. Fix: `el0_sync` now, on a data/instruction
abort from EL0, runs the same three resolvers and returns to retry the faulting instruction; an
unresolvable fault delivers `SIGSEGV`'s default action (the task `exit(-11)`), since aarch64
signal-frame delivery is still the named later gap - death is the correct default, not a stub.
Re-measured: the same unmodified musl binary now runs to completion and prints its output
(`MPOS_MUSL_OK`) on the second machine. Pin committed: the headless `/init` now first does
`sub sp, #0x1000` + `str x19,[sp]` (a fault past the mapped stack) and writes `MPOS_FAULT_OK`
only if the store survives - `boot-aarch64` asserts `MPOS_FAULT_OK` before `R:echo-test`, so the
demand-paging handler is pinned hermetically without a cross-toolchain dependency. **Stage 8
static-musl image - DONE.** `xtask` now builds a *real* unmodified `aarch64-unknown-linux-musl`
helper (`rustc -C target-feature=+crt-static -C linker=rust-lld`; the target is a listed prereq of
`boot-aarch64`, symmetric with the x86 test path's musl prereq), packs it as `/musl`, and the exec
target's parent `execve`s it last - so `MPOS_MUSL_OK` (that binary's own `println!`) is a permanent
`boot-aarch64` assertion. The committed chain is now `MPOS_FAULT_OK -> R:echo-test -> MPOS_EXEC_OK
-> MPOS_FORK_OK -> MPOS_MUSL_OK -> INIT_EXITED`: demand paging, read/write, `execve`, `clone`/
`wait4`, and an unmodified musl/Rust ELF all run at EL0 - the project's central promise on the
second machine. **The `drivers` disk root is landed too** (`NOTES.md` §13.3, `M7_DISK_OK` +
`M7_ROOT_OK`): the MMIO virtio-blk is a registered kernel disk (`vda`, `/dev/vda`), `xtask`
lays the repository's `rootfs/` overlay and the init images on it, and the machine now boots
with `/` on that disk - the init is read from it and its whole file lifecycle
(create/rename/hard-link/symlink/ftruncate) runs on ext4. What remains is a fuller shell
(`ash`), `net`, and `cargo xtask test --arch aarch64`.

**Measured follow-on RESOLVED: a heap-using musl binary now runs to completion - the fault
was `SCTLR_EL1.A`, not the loader.** Extending the `/musl` helper to `vec![0u8; 256*1024]`,
filling and summing it, then printing the checksum, made the real binary take a Data Abort
(EC `0x24`) at `FAR 0x20171e` before printing. Two guesses written down while chasing it -
a `PT_LOAD` the real path tears down, and a bad heap/`brk` placement - are both refuted by
the evidence: a temporary diagnostic in `el0_sync` (since removed) that walks the faulting
VA's page table reported the page mapped `READ` (so it is not a translation fault and not
unmapped), and the fault's DFSC is `0x21`, an **Alignment fault**. Symbolizing the `pc` (built with symbols) put it inside musl **`memcpy`** at
`ldp x6, x7, [x1]` - a pair load that *requires 8-byte alignment*, here reading the format
string at `0x20171e` (low three bits `110`, not 8-aligned). Root cause: the kernel enabled
`SCTLR_EL1.A` (alignment-check enable, bit 1), which governs EL0 too. Linux boots `A=0`, so
real programs' unaligned accesses - which `memcpy` produces constantly - simply work; with
`A=1` an unmodified binary faults in `memcpy`. Fix: drop `A` from `SCTLR_ENABLE` **and**
`bic` it in `enable_mmu`'s asm - the `orr` can only *set* bits and AAVMF leaves `A` set, so
removing it from the OR-set was measured insufficient on its own. The committed `/musl` pin
is now that heap binary and `boot-aarch64` asserts its literal output `MPOS_MUSL_OK
sum=33423360` (= the correct `32640 * 1024` checksum, so a wrong number or any fault fails
the verdict - the pin is a claim the run backs, not a string that prints regardless). With a
real binary that allocates, does an unaligned `memcpy`, formats and prints now running to
completion, the concrete gate to a fuller shell is cleared: what remains is the Alpine
aarch64 `ash`/`busybox` binary in the initramfs (a fetch and wiring, not an environment
wall), the `drivers` disk root, `net`, and `cargo xtask test --arch aarch64`.



**And the profile matters: the aarch64 debug build does not link.** `cargo
build --target aarch64-unknown-none` fails with `relocation R_AARCH64_ABS64
cannot be used against local symbol` out of the *sysroot's* `liballoc`, which is
not built for a PIE; only `--release` links, and `--release` is what `xtask`
builds, so no gate has ever noticed. Row 3's verification command used to be the
bare one; it says `--release` now, and `AGENTS.md` §3 carries the same warning.

Named gaps the port carries rather than hides, each with its home:

- **aarch64 `drivers` and `net` are the next full stage, with their surface measured.**
  Flipping the `drivers` gate to include aarch64 compiles to **31 errors**, and they
  cluster entirely into the x86-only subsystems the drivers sit on: the `x86_64` crate
  (port I/O in `qemu.rs`/`pci.rs`/`serial.rs`), `uart_16550` (the x86 serial chip -
  aarch64's console is the polled PL011, already wired), `crate::mm::map_mmio` (x86 BAR
  windows), and `crate::vfs::vcmux` (the x86 virtual-console mux, deferred since stage 5),
  reached from `ahci`/`drm`/`video`/`virtio/input`. So the port is not a flip: it is the
  established method - gate those x86-only modules out on aarch64 (a no-op there and on
  x86/host, the stage-6A prep shape), then ADD the aarch64 path the crate lacks: a
  `virtio-drivers` **MMIO transport** bound over the device tree's `virtio,mmio` windows
  the boot already reaches through `paging::device_virt` (`M5_VIRTIO` reads their identity
  registers today), reusing the arch-neutral `HalImpl` (its `dma_alloc`/`mmio_phys_to_virt`/
  `share` are pure `crate::mm`), then `blk` (a disk root), `rng` (entropy), and `net` over
  the same transport, plus GIC line routing to turn the polled `Console` into `SerialRing`.
  First commit is the MMIO
  transport with the simplest device driven to a boot marker (**done**; so is the
  GIC line routing that turned the polled `Console` into a `SerialRing` - §13.7, whose
  queue is now the shared `drivers::tx_ring` both machines' UARTs drain). The architectural
  prerequisite found when starting it: a driver must reach a device's physical MMIO
  window, but `crate::drivers` naming `crate::arch` is a layering violation (the rule is
  every machine call above the machine layer goes through `crate::hal`), and aarch64's
  device-window mapping today lives in `arch::aarch64::paging::device_virt` with no `hal`
  seam. So the stage opens by adding a `hal` capability - "give me a kernel-virtual
  address for this physical MMIO range" (x86 answers it with `map_mmio`, aarch64 with the
  device window) - which is a design-record change to `ARCHITECTURE.md` §2/§3 in the same
  commit, before any MMIO device is driven. That seam is the true first unit of the
  drivers stage. **The seam is LANDED** (the drivers-stage first commit):
  `crate::hal::Paging::map_mmio_window(phys, len) -> Option<usize>` plus its forward, the
  x86 body `Some(crate::mm::map_mmio(..))` (bump-allocate one mapping per BAR), the
  aarch64 body `paging::device_virt(..)` (offset into the fixed device window); and
  `drivers::virtio::hal::HalImpl::mmio_phys_to_virt` now answers through it instead of
  naming `crate::mm::map_mmio` - which removes the last x86-only call from that module, so
  the `Hal` impl the aarch64 MMIO devices will build on is machine-neutral. The `drivers →
  hal` edge already exists (halt/ticks/`unmask`/MSI) and `hal → drivers` is zero, so this
  adds no mutual pair and moves no depgraph constant; it is the sanctioned route, not a new
  one. Pin: x86 `PciTransport` calls `mmio_phys_to_virt` at bring-up, so a wrong x86 body
  fails `cargo xtask test` (every virtio device); the aarch64 body is the same
  `device_virt` `M5_VIRTIO` already reads the transports through. **The `drivers` gate is
  now flipped to include aarch64 (the prep commit, landed):** every x86_64-only submodule -
  AHCI, DRM, PCI, serial, input, video, devices, qemu, rtc and the PCI virtio drivers - is
  `#[cfg(target_arch = "x86_64")]`-gated out there, the PCI half of `virtio::hal` moved
  behind a gated `mod pci_transport` (re-exported so the x86 callers' paths are unchanged),
  and `drivers::virtio::spawn_service` is gated too - so `crate::drivers` now compiles on
  both machines and on aarch64 only the machine-neutral `virtio::hal` is live, with no
  device driven there yet. **Stage 7's first driven device over MMIO - DONE (the MMIO
  commit):** `drivers::virtio::mmio` opens a device from a kernel-virtual header the
  caller hands it (discovery stays in `arch`: `check_rng` walks the tree for the
  `virtio,mmio` window whose `DeviceID` is 4 (`VIRTIO_ID_RNG`; measured on this
  machine - the `virtio-rng-device` QEMU attaches answers 4, vendor "QEMU"), maps it
  with `paging::device_virt`, and
  passes the address down - so the driver names neither `crate::fdt` nor `crate::boot`
  and adds no new edge). It builds `MmioTransport::new` over that header and
  `VirtIORng::<HalImpl, _>` on the transport, reusing the machine-neutral `Hal` whose DMA
  comes from the direct map, then `request_entropy`. The boot marker `M7_RNG_OK` asserts 32
  host-generated bytes actually came back (a short read is `M7_RNG_FAILED`, never a zeroed
  buffer reported as entropy). This is the second machine driving a real virtio device end
  to end - queue setup, feature negotiation and DMA over the memory-mapped transport -
  through the crate it already uses on x86_64. **The rng now has a stable device home and
  feeds the kernel's entropy (the seed commit):** `drivers::virtio::mmio` keeps a global
  `Arc<MmioRng>` the boot installs (`init_global`) from the window `check_rng` finds, and
  exposes `entropy()`; `check_rng` installs the device *then* reads it, so `M7_RNG_OK` also
  means the stream `rand` will seed from is live. `rand::seed` gained an aarch64 arm
  (`#[cfg(all(target_os = "none", target_arch = "aarch64"))]`) that XORs `mmio::entropy()`
  into the boot-instant mix - the same shape x86_64 uses with its `RDRAND`+PCI-rng, minus
  the CPU instruction - replacing what had been the fixed host string. `getrandom(2)` (nr
  278) and `/dev/urandom` were already served on aarch64 off `rand::fill`; they now draw a
  hardware-seeded ChaCha stream. A machine with no rng window falls back to clock-only
  (still unpredictable to a process on it), the honest refusal, never a fake entropy claim.
  **Stage 7's second driven device over MMIO - virtio-blk, DONE (the block commit):**
  `drivers::virtio::mmio::read_blocks` opens the tree's block window (`DeviceID` 2,
  `VIRTIO_ID_BLOCK`) as an `MmioTransport` and hands it to the crate's `VirtIOBlk`; the
  boot attaches a raw 1 MiB image whose every byte is `i as u8` over a `virtio-blk-device`
  MMIO transport, and `check_blk` reads sector 0 and *verifies* the pattern - so `M7_BLK_OK`
  means a real block read came back with the right bytes, not merely that a request
  returned. It also reads the device's reported capacity and checks it equals the 2048
  sectors of that image, pinning the geometry path (a configuration-space read distinct from
  the data read) so a transport that read a sector but mis-sized the device would fail here.
  It then **drives the write path**: `mmio::write_readback` hands a distinct
  pattern to sector 3, flushes, and reads it back through a fresh open, requiring
  the round trip to match - so `M7_BLK_OK` attests both the read and write virtqueues
  move data over MMIO (a mismatch reuses `M7_BLK_FAILED`, already a terminator). The
  filesystem mounted over it (the disk root) is the next step; this is the
  block driver working, both directions. **Stage 8's net transport over MMIO - DONE (the net
  commit):** `drivers::virtio::mmio::read_mac` opens the tree's net window
  (`DeviceID` 1, `VIRTIO_ID_NET`) as an `MmioTransport` and hands it to the crate's
  `VirtIONet`; xtask attaches a `virtio-net-device` on a user-mode backend and
  `check_net` reads the adapter's 6-byte MAC and asserts it is not all-zero, **and
  then drives the transmit virtqueue**: `mmio::tx_probe` builds a DMA-reachable TX
  buffer through the neutral `Hal`, places a 60-byte frame in it, and requires the
  device to accept it (`send` returns `Ok`). `M8_NET_OK` prints only after that, so
  it proves the multi-queue MMIO *data path* moves a buffer to the device over the
  rings - the same class of proof the block milestone made by reading a real sector
  back - not just a config-space MAC read. It still does not claim the frame left the
  machine: the smoltcp stack and socket syscalls over this device are the next cut.
  (A rejected frame reuses `M8_NET_FAILED`, already a terminator, so it aborts
  honestly rather than printing a false `M8_NET_OK`.)
  **How the smoltcp stage actually has to start (measured, not assumed).** An
  attempt to cfg-prep `net` for aarch64 (move `smoltcp` to the shared `none` dep
  block, flip the `net` gate) left only 8 residual errors - `crate::drivers::virtio::net::VirtioNet`
  (imported at `net/mod.rs:74`), `crate::drivers::virtio::vsock`, and four
  `crate::vfs::procfs` references - which looks trivially gatable. It is not:
  `VirtioNet` is not a peripheral in `net/mod.rs`, it *is* the smoltcp `Device` the
  whole stack is built on (interface construction, the poll loop, the `Rx/TxToken`
  impls), so gating it to x86 removes the spine and leaves nothing to compile. The
  correct first cut is therefore not a cfg-gate but an **abstraction**: introduce a
  device trait `net` depends on instead of the concrete PCI `VirtioNet`, give x86 the
  PCI impl and aarch64 an MMIO impl (the `drivers::virtio::mmio::MmioBlk`-style
  adapter, this time satisfying smoltcp's `Device`), and only then attach a device
  and route sockets. `vsock`/`procfs`-backed nodes stay cfg-gated (absent on
  aarch64, honest gaps). The attempt was reverted, and the run above is why the
  next commit was an abstraction rather than a gate. **That ladder is now complete
  (the net flip commit).** The seam is `drivers::virtio::NetDevice` (landed first),
  the MMIO adapter is `mmio::MmioNet` - the same shape as `MmioBlk`, a device that
  *stays open* so the receive ring it posts is the one the stack's frames arrive
  on - `check_net` opens it once and **publishes** it (`mmio::init_net_global`),
  `board/aarch64.rs`'s `init_net` hands it to `net::init` and calls
  `net::spawn_service()` (byte-for-byte the pair `board/x86_64.rs` boots with), and
  `net` is now gated by `target_os = "none"` with `mpos_net` declared by both
  machines. `M8_NET_STACK_OK` prints when that wiring reached both interfaces and
  the service task - a narrow claim on purpose, because the round trip is pinned by
  *programs*: the sweep now stages 10 of the 14 std-side programs and passes
  **10/10**, including a full TCP connect/accept/read-write round trip over the
  stack (`t-tcp`, `t-tcp-listen`), UDP over the loopback (`t-udp-loopback`), and an
  `AF_UNIX` name (`t-unix-rustix`) - the pin job 4 chose instead of a kernel-side
  ping hook.
  **The flip found three stale assumptions in shared code, all of the same shape
  ("`net` is x86_64's").** `close(2)`'s socket teardown (`close_last_socket`) and
  `unlink`'s `unix::unbind_node` were gated `not(target_arch = "aarch64")`;
  measured, the first was a *leak*: `t-socket` bound 34567 and exited, and the next
  program's bind of that port answered `EADDRINUSE` on a machine that had just
  booted, because a socket left in the stack's set keeps its port - `t-tcp` and
  `t-tcp-listen` both failed for it and both pass with the gate gone. The third was
  `ioctl`'s `ifreq_index_named`/`ifreq_flags`, whose aarch64 arms answered `None`
  (so every interface ioctl fell to `ENODEV`) because "no net stack to report link
  facts on this machine yet"; they now read the live `net::link_facts` on both
  machines. The harness had to learn the same lesson from the other side: `/tmp` is
  a **tmpfs** on the second machine now, exactly as the x86 `/init` mounts it, and
  that is not decoration - an ext4 directory (the disk root) cannot hold a socket
  node, so `t-unix-rustix`'s bind answered `EOPNOTSUPP` from
  `create(name, SOCK_FILE)` until the mount was there (measured both ways). What the
  `aarch64` group still names is harness-only: the two programs that run another
  program out of `/bin`.
  **Stage 8's input transport over MMIO - DONE (the input-transport commit):** the
  same MMIO path reaches a *fourth* device class. xtask attaches a
  `virtio-keyboard-device` (a self-contained virtio-input device with a built-in
  keyboard backend, so it presents its event queue and config headless with no
  external chardev); `check_input` finds the window by `DeviceID` 18
  (`VIRTIO_ID_INPUT`) and `mmio::probe_input` opens it as a `VirtIOInput`, requiring
  the transport to negotiate the queues **and read its configuration-space identity**
  (`ids()`), so the milestone proves the MMIO transport moves config bytes for a fourth
  device class - the same config-read step the net milestone took with its MAC.
  `M8_INPUT_OK`. Like `check_net`'s MAC read
  this proves the *transport reaches the class*, not that events flow: the evdev
  nodes, `/dev/input` registration and the input pump stay x86_64's (`drivers::virtio::input`
  is PCI), so `/dev/input` remains absent on aarch64 (a named gap in the `/dev` set),
  and a rejected/unfound input device reuses `M8_INPUT_FAILED` (a terminator) to
  abort honestly.
  **Stage 7's disk-root primitive - an ext4 mounted over the MMIO block - DONE (the
  fs commit):** `mmio` also implements `crate::blk::BlockDev` for the MMIO block
  (`MmioBlk`: sector read/write/flush + capacity through the neutral `Hal`), and
  `check_fs` opens every block window and takes the one `Ext4Fs::mount` accepts - so
  `M7_FS_OK` means the whole block-driver -> filesystem path reads over MMIO. xtask
  lays the ext4 image down with `mkfs.ext4 -d <dir>` (unprivileged, no mount) and
  `^has_journal,^metadata_csum` - the same two features the driver refuses on the
  x86_64 root - making e2fsprogs a `boot-aarch64` prereq. The two block devices are
  told apart by content (a pattern sector vs a mounting filesystem), not QEMU's slot
  order. `check_fs` then reads the image's `/hello` through the mount (root ->
  lookup -> read_at on the `FileSystem`/`Inode` traits - no fd table or task needed
  at boot) and asserts its bytes, so `M7_FS_OK` covers the full block-driver ->
  ext4 -> file path, not just a mount. What remains is pivoting to that filesystem
  as the root (§13.3, done), the smoltcp stack over the MMIO net device and
  its socket syscalls (§13.4, done), and then the aarch64 `/bin` and
  `cargo xtask test --arch aarch64` (§13.5/§13.6, done) - the console's own
  interrupt arriving with §13.7. (Debt to name: this boot-time mount reaches from the machine
  layer into `crate::vfs`; `depgraph` does not flag it - there is no `vfs -> arch`
  back-edge, so no new mutual pair - but the disk-root check is a candidate to move
  to the `main` wiring layer, where the initramfs root is already mounted.)

  **Stage 7's `/dev` - the device set a real init mounts - DONE (the devfs commit).**
  The prior *attempt* reached for `standard_devfs()` from an early boot `check_dev`
  and **KERNEL-PANICed in `vfs::tty::global()`** ("ttys exist only after
  `vfs::tty::init()`") - the console tty is registered during the init handoff, not
  at that milestone. The lesson was ordering, not a pty/`vcmux` wall: the `tty1..N`
  and `ptmx` nodes mint ordinary pty pairs (`vfs::pty::vc`), which are
  arch-independent, so the only x86-only references in `standard_devfs()` are the
  virtio-PCI `hwrng`/`hvc`/`dsp`/`fb0`, `/dev/input` + `uinput` and the DRM card.
  Those five registrations (and the `HwrngDev` adapter that wraps the PCI rng
  handle) are gated into `#[cfg(target_arch = "x86_64")]` blocks, `pub mod devices`
  is opened to aarch64 the way `virtio` was, and the `mount -t devtmpfs`/`devfs`
  dispatch arm in `syscall/linux/fs/file.rs` drops its `#[cfg(not(aarch64))]` - so
  a real init's `mount(2)` now builds `/dev` on this machine instead of getting
  `ENODEV`. The pin is `check_dev` moved into the **init handoff**, after
  `vfs::tty::init()`. It goes one step further than building the set: it creates
  a `dev` directory on the ramfs root, mounts `standard_devfs()` there, and then
  reads `/dev/urandom` and `/dev/hwrng` by *path* through the live mount table
  (`vfs::resolve -> read_at` - the same walk a userspace `open` makes), so the
  pin proves `/dev` is actually on the filesystem, not merely that the builder
  runs. This is the kernel-side mirror of Linux's `CONFIG_DEVTMPFS_MOUNT`: the
  machine that builds a devtmpfs also hangs it at `/dev`, so a userspace init
  finds the device set already mounted rather than having to make it. Both reads
  assert their full buffer - `/dev/urandom` (the pool `rand` seeded from the MMIO
  rng) 16 bytes, `/dev/hwrng` (the MMIO virtio-rng `check_rng` installed, reached
  through the same `fill`) 8 host-generated bytes. `M7_DEV_OK` is on the wire.
  `/dev` is then reached by *userspace*, not just the kernel: the `/musl`
  static-musl ELF that `boot-aarch64` already runs at EL0 now `open`s and
  `read_exact`s `/dev/urandom` and `open`s (write) `/dev/console` through libc file
  I/O, printing `MPOS_DEV_URANDOM_OK` / `MPOS_DEV_CONSOLE_OK`, and traverses it with
  `read_dir`/`metadata` (getdents64 + fstat), printing `MPOS_DEV_GETDENTS_OK` /
  `MPOS_DEV_STAT_OK` — all four asserted. It then runs a full **regular-file
  lifecycle** on the mounted root ramfs (`write` → `read` back → `metadata` →
  `remove_file` → confirm gone), printing `MPOS_FILE_OPS_OK` (also asserted) — the
  create/write/read/stat/unlink a shell and any init issue against the filesystem
  proper, not only against device nodes. A `getcwd`/`chdir` round-trip (`cd /dev`
  and back) prints `MPOS_CWD_OK` (also asserted) - the `pwd`/`cd` pair a shell uses
  at startup and on every directory change. A `symlink`/`readlink` round-trip
  (`ln -s /dev`, read the target back, remove) prints `MPOS_SYMLINK_OK` (asserted) -
  the SYMLINKAT/READLINK handlers a loader or shell completion performs. A
  `mkdir`/nested-file/`rmdir` round-trip (`create_dir /mdir`, write+read a file
  inside it, remove the file, remove the directory, confirm it is gone) prints
  `MPOS_MKDIR_OK` (asserted) - the MKDIRAT/UNLINKAT(+AT_REMOVEDIR) directory half an
  init uses to set up `/tmp`, `/var`, and the empty-directory gate (rmdir refusing a
  full one) held on this machine. A `rename`/`hard_link` round-trip (write `/mrn_a`,
  rename to `/mrn_b`, confirm the old name is gone and the body moved; hard-link
  `/mrn_c` to `/mrn_b`, then remove `/mrn_b` and confirm the bytes still read through
  `/mrn_c`) prints `MPOS_RENAME_LINK_OK` (asserted) - the RENAMEAT/LINKAT `mv`/`ln`
  pair, and the surviving read after the first name's unlink is the counterfactual that
  proves linkat shared one node rather than copying one (which is what rename does). An
  `ftruncate` round-trip (write ten bytes, `set_len(4)` then `set_len(0)`, each
  confirmed by the stat length) prints `MPOS_TRUNCATE_OK` (asserted) - the `: >file`
  and `truncate` length handler, so a trim that left stale bytes or a stale length
  would fail rather than pass. A **1 MiB heap buffer** (past musl's malloc threshold, so
  served by `mmap`) written one byte per page and read back prints `MPOS_MEMORY_OK`
  (asserted) - the anonymous/mmap demand-paging path proven from a running process, which
  is a *memory* handler rather than a filesystem one, closing the last facet of the
  "core fs/memory handlers" deliverable that was still only inferred from dispatch. An
  **fd-duplication** round-trip (`File::try_clone`, which is `fcntl(F_DUPFD_CLOEXEC)`,
  reading the file's bytes back through the duped descriptor) prints `MPOS_DUP_OK`
  (asserted) - the fd-table primitive behind a shell's `>` redirection and `|` pipes. A
  **struct-stat ABI** check (chmod `0o640`, then `st_mode` reads back `0o640`, the file
  reports `is_file()` with the right `len()`, `st_nlink`/`st_ino`/`st_blocks`/`st_size`
  read back at the aarch64 offsets, `/dev` reports `is_dir()`, and a symlink reports
  `is_symlink()` (`S_IFLNK` via `lstat`)) prints `MPOS_STAT_OK` (asserted) - pinning that `write_stat` lays the struct out at the
  **aarch64 asm-generic offsets**, not x86_64's. This fixed a real ABI bug: the previous
  x86-only offsets put `st_mode` where musl reads `st_nlink`, so every stat-based
  file-type/permission read on aarch64 returned another field's value (a regular file
  reported `st_mode == 1`) - a wrong-shape answer that no earlier marker exercised, because
  the file-lifecycle and `/dev` proofs read only `st_size` (offset shared by both ABIs) and
  existence. `abi.rs` `write_stat` now branches the field order on `target_arch`, x86_64
  byte-for-byte unchanged (the host `write_stat_lays_out_struct_stat_at_the_x86_64_offsets`
  test and the 142-suite `stat` users still cover it); aarch64 is pinned on the machine.
  This is the same `open`/`read`/`write` a shell issues against the device set - so the
  mounted `/dev` is proven usable from a running program, not merely resolvable
  kernel-side.
  x86's devfs is byte-for-byte unchanged (host 511, `cargo xtask test` 142/142),
  and `boot-aarch64` runs every marker through `INIT_EXITED` with `M7_DEV_OK`
  among them. The set is still the honest subset: no `/dev/input`/`uinput` (the
  virtio-PCI input bus and `uinput` are x86_64's), no `fb0`/`dri` (DRM is x86_64's),
  no `/dev/rtc` (CMOS is x86_64's) - each a named gap, not a stub. What still has
  to land for a typed shell is a *userspace* init that execs `ash` against the
  `/dev` the kernel now mounts. That init must exec a *static* shell binary: the
  kernel does not load a `PT_INTERP` dynamic interpreter (a measured fact -
  nothing in the exec path acts on `interp`), so Alpine's shared-musl busybox will
  not run; a static aarch64 busybox (cross-built or a static prebuilt) is the
  prerequisite. `/proc` is a further, separate blocker (it is gated behind the net
  and PCI subsystems - see the next note), not needed for `ash` to start.

  **`std::process::Command` spawn data-aborts on aarch64 - cause narrowed by a
  reproduced fault trace.** An
  attempt had the running musl helper call `std::process::Command::new("/leaf").output()`
  - the pipe + `dup2` + fork + `execve` + `waitpid` sequence a shell uses to run a
  command. The markers printed up to the spawn (`MPOS_DEV_*_OK`, `MPOS_DEV_STAT_OK`) and
  then the process died with a userspace **data abort** (`user fault ... ESR 0x9200004f`),
  not a clean errno. The change was reverted (no crash line shipped; boot-aarch64 stays
  clean). An earlier draft of this note blamed missing fork-COW - that was **wrong**:
  the aarch64 machine already carries the full COW machinery (`sched::fork` uses
  `map_cow`/`set_cow_bit`; `arch/aarch64/paging.rs` implements `map_cow`/`resolve_cow`/
  `is_cow_page`, and `el0_sync` already calls `sched::resolve_cow_fault`). So the abort is
  a *narrower, unresolved* fault on a specific address, not absent COW support.

  A throwaway re-run captured `el0_sync`'s own fault line, which pins the region rather
  than theorising about it: `user fault at 0x3fe1bf80 (ESR 0x9200004f, pc 0x276350)`.
  Decoded, `ESR`'s `DFSC` is `0x0f` (a **level-3 permission fault**) with `WnR` set (a
  **write**), and `pc` is inside the static-musl binary's own `clone` trampoline.

  A third re-run instrumented `handle_clone` itself and that corrects the two earlier
  drafts, which wrongly blamed an *unmapped* caller-supplied child stack. The request is
  `raw_flags=0x4111` - `CLONE_VM|CLONE_VFORK|SIGCHLD`, i.e. **vfork** - with
  `child_stack=0x3ffd05e0` *inside* the process's already-mapped stack window and
  `parent_sp=0x3ffcef90`; the faulting address is `0x3ffcef80`, exactly `parent_sp - 16`.
  So it is the **parent's own stack** page that faults, not the child and not an unmapped
  region - and the `MPOS_MEMORY_OK` 1 MiB `mmap` proof from the same run had already ruled
  out any general `mmap`/VMA breakage.

  The mechanism: `clone()` routes a `CLONE_VM`-without-`CLONE_THREAD` request to
  `spawn_copy` (the fork path), which does a full **copy-on-write** fork - it builds a
  private child space and read-protects the *parent's* writable pages via
  `set_cow_bit_quietly` (`runtime.rs:7480`) - and the design deliberately does **not**
  suspend the vfork parent (the doc at `runtime.rs:7994` assumes the `posix_spawn` caller
  waits on the child itself). A real vfork must *share* the address space and hold the
  parent until the child execs; here the child instead gets a private copy while the
  parent keeps running and immediately writes its now-read-protected stack just below
  `sp`. The earlier hypotheses are now eliminated with evidence. (1) COW-split *math*:
  `set_cow_bit_quietly` ORs `COW_BIT | AP_READ_ONLY` onto a user page whose `AP` is
  `AP_EL0_RW (0b01)`, yielding `AP_EL0_RO (0b11)` - EL0 keeps read access, only writes
  split (`paging.rs:92-98`, `1237`); `map_cow` maps the child copy `EL0_RO | COW_BIT`
  (`1106-1124`); the hand-encoded `fork` child exercises that and reaches EL0. (2) "parent
  `TTBR0` never reloaded after the child's turn": plain fork also runs the child first and
  works, so resume-reload functions. (3) a stack-growth / `stack_top` window bug: the
  `current_stack_range` value that looked far from `sp` is just the task's *kernel* stack
  (`runtime.rs:5958`), not the user window.

  The instrumented fault dump settles it: the fatal fault is a **read** (`is_write=false`,
  `ESR` DFSC `0x0f` = level-3 **permission**) at `sp - 16`, and the faulting leaf descriptor
  is `AP = 0b10` (**EL1 read-only, EL0 no access**) with `COW_BIT` set. That is a *symptom*,
  not a paging one-liner: a page carried into the child with `AP 0b10` never had the EL0
  (`USER`) bit, so the earlier "confirmed aarch64 paging fix" idea is wrong - the copied
  page faithfully reflects a non-user mapping the child should never have been reading in
  the first place. Why the vfork child touches a non-user page is the real cause: **`vfork`
  requires `CLONE_VM` sharing** - the child runs the parent's C code against the parent's
  live stack and pointers until it `execve`s - but `clone()` routes `CLONE_VM`-without-
  `CLONE_THREAD` to `spawn_copy`, which gives the child a *private* COW copy and does not
  suspend the parent (`runtime.rs:7407-7434`; comment at `7994`). Under a private copy the
  child's parent-relative pointers resolve against the wrong tables and land on a kernel
  page, faulting on read. So this is not a paging bug at all: it is **`CLONE_VM`/
  `CLONE_VFORK` fork semantics that are unimplemented** - a genuine multi-step feature in
  the shared fork/scheduler path (share the address space for `CLONE_VM`; suspend the parent
  on `CLONE_VFORK` until the child execs/exits), which x86 does exercise (its std forks, not
  vforks) and so the aarch64 gap is invisible there.

  Concrete implementation route (worked out, not yet executed): reuse the thread
  sharing mechanism rather than invent one - give the vfork child `space: None` and an
  `mm` whose `leader()` is the parent, so `space_owner` (runtime.rs:2134) resolves its
  address-space access to the parent's tables, and the parent alone owns/frees that space
  (no double-free when the child exits, since `Task.space` is `None`). Give the child its
  own `tgid`/pid so it stays a separate, `waitpid`-collectable process. This alone is NOT
  sufficient and NOT safe: with `CLONE_VFORK` the parent must be *suspended* until the
  child `execve`s or exits (otherwise both mutate the shared stack - the isolation break
  that makes a half-fix worse than the gap), which needs the scheduler to not `push_ready`
  the parent and a wake-parent hook in the child's `execve` and `exit`. On `execve` the
  child must detach: `execve` already builds a fresh address space for a process exec, so
  the child's `mm.leader` must be repointed to the child at that point (a thread never
  execs, so today's exec path may not exercise this case - it must be verified, not
  assumed). The gate is: a `MPOS_SPAWN_OK`-style proof from the running musl helper
  (std::process spawn returns), plus `cargo xtask test` 142 (x86 untouched - it forks, not
  vforks) and `boot-aarch64` green through `INIT_EXITED`. It was not attempted here only
  because that cross-subsystem work cannot be completed *and* shown correct on both gates
  within the budget, and a partially-correct version would break `fork` isolation or
  double-free a shared space.

  This is the named remaining blocker for running a *subcommand* (a shell's core) from
  `cargo xtask run --arch aarch64`, and is orthogonal to the static-busybox prerequisite
  above.

  Reading further confirms the COW path is not the bug: `resolve_cow`'s split leaf
  (`VALID | TABLE | attributes(flags)`, `paging.rs`) is byte-for-byte the same encoding
  `map_page` uses for a normal leaf, and `COW_BIT` is a software bit (55) the walk
  ignores - so the aarch64 paging COW primitives are internally consistent. The notable
  consequence is that aarch64 COW-*split* is **untested by the port itself**: the
  hand-encoded `clone(SIGCHLD)` milestone only proves a child reaches EL0 and issues a
  syscall; it never *writes a heap/stack page of the forked address space*, and every
  musl fork test runs on x86. The concrete next cut is therefore a hand-encoded aarch64
  milestone in which the fork child writes a COW page and the parent verifies isolation -
  that both pins COW-split and, if it faults, localises the std abort to COW with a
  traceable region instead of leaving it inferred.


  **`/proc` stays honestly refused on aarch64 - and it is gated, not one line from
  serving (corrected by an attempt).** An earlier note here said `vfs::procfs`
  compiled on aarch64 and the `mount -t proc` arm was a single `#[cfg]` from opening.
  Measured false: `procfs` is `#[cfg(target_arch = "x86_64")]` in `vfs/mod.rs`,
  because it reads `crate::net`'s socket table and `crate::drivers::pci` config - two
  x86_64-gated subsystems - so the module is absent on aarch64 entirely. Attempting to
  route `/proc/cpuinfo` through a new `hal::Cpu::cpuinfo` capability (to avoid the
  hardcoded `cpu family : x86_64` string, a wrong shape on ARM) hit a second wall:
  `hal` is `#[cfg(target_os = "none")]`, so `crate::hal` is absent in the host build
  where `procfs` *does* compile (host is x86_64-linux-gnu) - a procfs body cannot call
  the machine capability. The change was reverted (no shipped partial-`/proc`, no
  broken build). The real gap, now precisely named: **`/proc` on aarch64 is blocked
  behind the `net` stack and PCI/virtio enumeration that `procfs` depends on** - i.e.
  largely the same un-gating work as the `net` stage - not merely a `cpuinfo` fix. Once
  `procfs` is made to compile without net/pci (cfg-gating `/proc/net` and the PCI-backed
  nodes, the same shape as `drivers`'s prep commit), the `cpuinfo` needs a `MIDR_EL1`
  read reached *without* naming `arch` from `vfs` (a per-arch `cpuinfo` inside procfs
  itself, reading `MIDR_EL1` via `core::arch::asm` behind `target_arch`, or a capability
  that also exists on host). Named, not papered over.




- **The `-kernel` bring-up was an instrument, and it is gone.** Stages 1b-1d
  entered at a fixed physical address with the MMU off, because that is the
  smallest thing that proves the toolchain, the linker script and the machine
  body agree. Stage 1e deleted it and boots from the ESP through AAVMF, which is
  the promise this section makes: a second machine with the *same* boot path,
  not a shortcut past it. Deleting it meant more than removing a QEMU flag -
  the image had to become relocatable (`relocation-model=pic`, `-pie`, a script
  with no base address), the stack had to move from the script's `.stack` to one
  the loader allocates, and `_start` had to stop being an assembly stub. What is
  left of the instrument is one entry in `AGENTS.md`'s trap table saying not to
  bring it back.
- **The exception level is measured rather than assumed, and it survived the
  move to the real boot path.** QEMU 11.1.1 with `-M virt` and a bare aarch64 ELF
  entered the `-kernel` image with `x0 = 0` and `sp = 0` (the banner printed
  `x0`, and QEMU's own `-d cpu` log agreed). The AAVMF path is a different
  machine in every respect - a UEFI application is entered with the firmware's
  calling convention, not a register of our choosing - and it lands at **EL1**
  as well, which is what the kernel's `CurrentEL` check now reports and what its
  EL1-only vectors and physical timer need. A machine that arrived at EL2 would
  stop with a line saying so rather than configure the wrong one.
- **The tree is read now - and it does not arrive in a register, nor under the
  name this section used to assume.** `x0` is the handoff record's register on
  both machines. Under `virt` + AAVMF the tree is published in the **UEFI
  configuration table**, which is firmware data that does not survive
  `exit_boot_services`, so the loader reads the header's `totalsize` there and
  copies that many bytes into frames of its own (stage 5a). Two corrections came
  out of doing it, and both were measured rather than reasoned. First, that
  table has **no entry for a tree at all** when QEMU builds ACPI tables, which
  `-M virt` does by default on aarch64: the firmware advertised `ACPI2`
  (`8868e871-e4f1-11d3-bc22-0080c73c8881`) among the nine tables the loader
  prints, and no device-tree GUID. The boot therefore selects
  `-machine virt,acpi=off`, which is the same machine with the other
  description of itself, and the tree is the one this port reads. A real aarch64
  server would hand over ACPI and no tree; reading *that* is a gap this port
  names rather than half-does, and `check_devices` failing by name is what makes
  it visible instead of silent. Second, the cost: `totalsize` is 1052672 bytes
  for a tree of 7160 bytes of structure and 569 of strings, and copying what the
  tree says it is costs one MiB of the machine - the difference between `192
  MiB` and `191 MiB` of usable RAM in the log above.
- **The GICv2 bring-up is three bits across two frames, and the wrong value is
  silent.** Measured on this machine, and recorded because the register values
  are load-bearing: the timer's line needs `GICD_CTLR` *and* `GICC_CTLR` bit 1
  set - group 1 is what an IRQ *is*, so a line left in group 0 with only bit 0
  written is pending forever and raises nothing - and `GICC_CTLR.AckCtl` (bit 2)
  as well. Without AckCtl the acknowledge reads **1022** instead of 30, which by
  the GICv2 specification is its answer when the pending interrupt is group 1
  and the acknowledging access is secure; the interrupt is therefore never
  retired, `nIRQ` stays asserted, and the vector is re-entered every few hundred
  nanoseconds. That is not "the clock is broken" from the outside - it is a
  clock that looks *very fast*, and it cost a 1022/30 reading to tell apart.
  Linux programs the same `0x7`. The check's failure branch now prints the five
  registers that separate the three causes.
- **The machine addresses are hardcoded, and now they are checked at every
  boot.** QEMU virt puts PL011 at `0x0900_0000`, the GIC at
  `0x0800_0000`/`0x0801_0000` and RAM at `0x4000_0000`, and those are what the
  kernel is compiled with: the tree does not yet *supply* them. What stage 5a
  made it do is ask whether they are still right - `check_devices` reads each
  node back and compares it with the *physical* bases the kernel keeps for that
  (`pl011::PHYS`, `gic::DIST_PHYS`/`gic::CPU_PHYS`, `gic::TIMER_INTID` - the
  addresses the kernel *reaches* those registers at are stage 5b's
  `pl011::BASE`/`gic::DIST`/`gic::CPU`, and they are deliberately different
  numbers now), and the boot stops with `M4_DEVICES_FAILED` on any
  disagreement. A machine that moves a device is now a boot failure rather than
  a driver faulting on its first register. What is still hardcoded is everything
  with no constant to compare against, and stage 5b took the direction that
  matters for discovery: the 32 virtio-MMIO windows, whose addresses are in the
  tree and nowhere else, were **counted and bounds-checked** and are now
  **read** - `check_virtio` walks them, translates each into the kernel half and
  reads the transport's own identity registers, so a window with nothing behind
  it is a named `M5_VIRTIO_FAILED` rather than a driver that times out later. Two
  things about that read are measurements rather than assumptions, and both are
  recorded because a driver inherits them: the transports answer **version 1**
  (the legacy register map) unless the machine is told
  `-global virtio-mmio.force-legacy=false`, and the tree cannot say which window
  has a device - all thirty-two nodes are published either way, and the attached
  device answers from the *last* window, not the first. Still missing is the
  routing of anyone's interrupt line: the PL011's own is SPI 1 (INTID 33) and
  nothing decodes it, because the console is a polled writer until stage 4.
  The window deliberately does not start at zero - a null pointer that silently
  works is the one thing x86_64's own `check_arch` asserts cannot happen.
- **The kernel's address space is its own now, and the window that was open
  across 1e is closed.** Two things went together and are worth keeping as one
  measurement: the loader builds its identity map and its direct map with a
  single call and a single set of flags, so on the loader's tables both come out
  `rwx` - a window x86_64 closes the moment its kernel installs its own tables
  and stage 1e could not. Stage 1f closes it: the plan gives the direct map
  `READ | WRITE`, the boot log prints `direct map ... rw and it stores`, and the
  check walks every one of the 63616 pages the plan names to say so. What the
  loader's tables are still needed for is the length of the jump: the kernel
  cannot build its own until the frame allocator is up, and the frame allocator
  needs the direct map.
- **The device window is in the kernel half now, and the low half is empty.**
  Stage 5b closed this one, and it was closed by changing where the registers are
  reached rather than by naming the collision better: the kernel mapped 64 MiB
  from `0x0800_0000` - PL011 and GIC - at the addresses the devices physically
  are, which is inside `TTBR0`, the half a process is going to own. It maps them
  at `0xffffffff40000000` instead, the same hole x86_64's `mm::MMIO_BASE` uses,
  and `check_paging` now asserts both halves of that: the alias translates to the
  right frame as Device, and neither the image's physical address nor the device
  window's is reachable any more. What is *not* closed is the mechanism: the
  window is a fixed 64 MiB range mapped whole at boot, because `mm::map_mmio`
  goes through `hal::Paging` and `hal` cannot compile for this machine until the
  last of its six traits exists (`Paging`'s user half and a `WallClock` are what
  6b leaves), so the dynamic allocator cannot exist.
  A device outside the tree's window is refused by `paging::device_virt` rather
  than mapped, which is the honest answer for a machine whose devices are a
  firmware-described range - and not the general one a PCIe aarch64 server needs.
- **Stage 1d's RAM window is gone; the map is the firmware's and the plan's.**
  The 1 GiB ceiling it mapped "because the code that would care had not arrived"
  is replaced twice over: stage 1e's loader walks the EFI memory map region by
  region, and stage 1f has the kernel rebuild the address space from that record
  by way of `mm::vm::kernel_mappings`. This boot reports `14 regions, 192 MiB
  the allocator may use` out of 248 MiB of RAM, `pmm: 5 zones, 49209 frames`.
  The floor the whole thing stands on is `mm::init`, which is machine-neutral
  and simply had no aarch64 caller until 1f gave it one.
- **`hal` compiles for aarch64, and the "two blockers" this note used to name were
  an understatement.** Until stage 6f the module was x86_64-only for a *structural*
  reason this ledger missed: `hal::Current` gated on `target_arch = "x86_64"` so the
  module could not even be named here, and four capability groups were x86-shaped
  *inside* the baseline traits. A caller survey found them (MSI, segment selectors,
  the lazy-FPU trap protocol, and the first-user-entry shape) and that every one of
  their callers lived in a module aarch64 does not compile yet. So rather than make a
  second machine fake answers (`AGENTS.md` §3 rule 4), stage 6f split them into three
  *optional* capability traits - `Msi`, `FpuTrap`, `Segments` - that only x86's
  `Current` implements, un-gated `Current` to `target_os = "none"`, and had
  `arch/aarch64` write the six baseline impls. The two things this note claimed were
  the *last* blockers were in fact the easy half: the running-process `Paging` methods
  (6c), the `WallClock` (6d), and `Cpu`/`Interrupts` primitives (6e) - each a real
  body read back against the hardware it names, not a stub. `crate::hal` now compiles
  for aarch64 under `cargo clippy --target aarch64-unknown-none`.
  What remains is *below* the gate, not blocking it: the trait `enter_user`/`resume_user`
  and `set_kernel_stack` share the `enter_frame` that `check_el0` now exercises, but
  the running-process caller that schedules tasks is stage 3; `to_user_regs`/`from_user_regs`
  write the aarch64 `user_pt_regs` order but their field-order pin against `ptrace` is
  stage 6's ABI; the copy-on-write fault resolver (the next bullet) needs a faulting
  task. The then-x86-only `Msi`/`FpuTrap`/`Segments` are *not implemented here on purpose* -
  a `virt`+GIC machine has no MSI controller, no lazy FPU and no segments, and the
  sched/runtime code that wants them un-gates at stage 3 with its own arch branch. (Of
  those, `Segments` was removed outright in §13.2 - `hal::Gate`'s frame ops read the
  selectors from the GDT themselves - so `Msi` and `FpuTrap` are what this sentence still
  describes.)
- **A copy-on-write page can be resolved, and nothing faults into the resolver
  yet.** Stage 6b landed the operation - `resolve_cow` copies the frame, rebinds
  the descriptor and answers `(old, new)` - and the check drives it in the order a
  `fork` would, but the other half is deliberately absent: a data abort from EL0
  on a read-only page has to be recognised as a copy-on-write fault, resolved and
  the faulting instruction retried, and today the EL0 vector treats anything that
  is not an `svc` as fatal. That belongs with the `Paging` methods that take a
  *running* process - `is_cow_page` is what such a handler asks first - which is
  the other remaining `hal` blocker, so no code was written for it here and a page
  in a user space cannot yet be shared by two processes that are both running.
- **No ACPI consumer exists to reuse.** `virt` + AAVMF passes a *device tree*,
  so device discovery is new DTB-parsing code in stage 5, not a port of an
  existing table walk.
- **`rand` has no aarch64 hardware source yet**, so `seed()` takes its
  deterministic arm there; `RNDR` (where present) and the boot-epoch mix land
  with stage 5's drivers.
- **A host-compiled module is excluded from the aarch64 build without being
  excluded from the host.** `sched`/`vfs`/`syscall`/`uaccess` are the host unit
  suite's, so their gate is `any(not(target_os = "none"), target_arch =
  "x86_64")` - "the host, or x86_64 bare metal" - not `target_arch =
  "x86_64"`, which would have silently deleted 434 host tests while compiling
  cleanly.

**The older prerequisite list, kept for the record** (all six still stand, and
the stage table above is where each lands): the `VBAR_EL1` exception vector
table, a GICv2/v3 driver, 4 KiB pages through `TTBR0_EL1`/`TTBR1_EL1`, PSCI for
reboot/shutdown, the generic timer, and a PL011 or SBSA UART.

### 9.2 RISC-V Support

- SBI interface for boot/runtime services
- PLIC interrupt controller
- Sv39/Sv48 page tables
- `mtime`/`mtimecmp` timer
- UART via SIFIVE_UART or NS16550

---

## Phase 10: Performance

One number framed this phase: `ping 127.0.0.1` measured 20 ms on the 100 Hz
build (7.5 ms at the current 250 Hz) against the host's 0.03 ms. The cause
was architectural rather than slow code - every blocking network wait slept a
whole clock tick per round, arrived frames were discovered only by tick-rate
polling because the virtio-net adapter was never interrupted, and an AF_INET
socket had no wakeup source at all (pipes and AF_UNIX already did). Each
subsystem below names the same shape of fix: make the event end the wait.

### 10.1 Network Latency — **DONE**

- Blocking socket calls (`recv`/`send`/`connect`, TCP/UDP/raw) park on their
  own readiness through `block_until_deadline` instead of sleeping a tick per
  round; signals still end them as `EINTR`. Sockets register with the stack
  through `poll_subscribe`, and every poll wakes the registered list.
- `poll(2)`/`select(2)` park the same way, with the tick kept as the bound
  for descriptors that have no waker behind them (uevent, signalfd,
  eventfd). An unclocked `epoll_wait` over a network socket - which
  previously would sleep forever, having no wakeup source - now ends with
  the event (`t-epoll-socket`).
- The virtio-net adapter is armed for MSI-X: the vector table entry is
  programmed by hand and every queue bound to vector 0 through the common
  configuration, whose BAR is 64-bit and sits above 4 GiB (reading only the
  low dword maps ordinary RAM, and register "writes" round-trip through it
  while the device hears nothing). The handler polls, wakes the waiters,
  EOIs the local APIC, and preempts so the woken task runs immediately
  rather than at the next tick. The tick-rate polls all remain, so a machine
  where the interrupt cannot be armed runs exactly as it did.
- `sendto`/UDP/TCP emit immediately after queueing rather than at the next
  poll; smoltcp's clock reads the TSC instead of the tick counter; `xtask`
  builds release by default (the loader gained the `wcslen` its release
  codegen calls for).
- Measured by `t-netlat` (20 ICMP echo rounds to 127.0.0.1): 7.5 ms → 0.3 ms
  average, 74 µs floor - the remainder is slirp's own path through the host.

### 10.2 Context-Switch and Fork Cost — **PARTIAL** (three of four landed, measured)

**Why**: every task switch saved and restored 512 B of FPU state
(`fxsave`/`fxrstor` unconditionally), rewrote CR3 with a full TLB flush, and
`fork` walked the entire address space to mark pages COW with a flush after
every page. A busybox fork+exec paid all three.

| Change | Where | State |
|---|---|---|
| Save FPU state lazily: `CR0.TS` on the task entered, its `#NM` restores it | `arch/x86_64/context.rs`, `idt.rs`, `sched/runtime.rs` | **done** |
| One TLB flush per fork instead of per-page `protect` calls | `sched/runtime.rs`, `arch/x86_64/paging.rs` | **done** |
| Kernel stacks smaller than the fixed 64 KiB per task | `sched/runtime.rs` | **done** |
| Enable PCID (`CR4.PCIDE`), keep per-process TLB entries across switches | `arch/x86_64/paging.rs` | **not done** — the argument is below |

**The lazy FPU.** `mpos_context_switch` no longer touches the 512 bytes of
`FpuState` at the head of each `Context`; it sets `CR0.TS` on the task it
enters, and that task's first floating-point instruction traps into the `#NM`
handler, which clears the bit and loads the image. The saving half stays eager
and moved into Rust (`Table::hand_over_fpu`), because that is the last moment
at which the outgoing task's registers are uncontroversially its own. A task's
`fpu_live` flag says whether the hardware holds its state, so the FPU is written
out only when it was actually used: a program that never executes a vector
instruction - which is most of this image, and all of a shell's `fork`+`exec` -
pays one `mov cr0` per switch and nothing else. `fork`/`clone` read the
creator's image through `Table::inherit_fpu` (registers when live, its saved
block when not) and `execve` claims the registers before `fninit`, because a
task that never used the FPU is running with `TS` set. The property the whole
scheme rests on - that the register file belongs to a task and not to the CPU -
was already pinned by `t-fpu-across-switch`, which stores two distinguishable
bit patterns to `xmm7` across a `sched_yield`; it passes unchanged, and it
exercises the deferred restore on both sides.

**The fork flush.** `parent_space.protect(..) + set_cow_bit(..)` per page became
`set_cow_bit_quietly(..)` per page and one `flush_if_active()` after the loop.
What that removes is 16384 CR3 writes for a parent with 8192 pages, and the
per-page `update_flags` round trip that re-derived the PTE's flags from the
walk's own report - the quiet form sets the COW bit and clears `WRITE` in one
write and leaves the rest of the entry alone, which also stops the fork from
dropping `ACCESSED`/`DIRTY` on the parent's pages.

**What the machine says.** `t-forkcost` (new, and in the suite) runs the *same*
fork/exit/wait loop twice: once with a 32 MiB arena mapped in the parent, and
once after `munmap` has given it back. The second parent has a few pages of
address space instead of 8192, so anything that scales with the mapping shrinks
by three orders of magnitude.

| 220 cycles, 200 timed | `fork` | `wait4` |
|---|---|---|
| 32 MiB parent, per-page flush (the old shape) | 349.0 µs | 7650.8 µs |
| 32 MiB parent, one flush (this change) | 350.9 µs | 7648.9 µs |
| no arena at all, one flush | **17.0 µs** | 7982.8 µs |

Three things fall out of that, and the first two are corrections to what this
section said before the measurement existed.

**The fork flush change is not measurable, and the reason is not subtle.** Sixteen
thousand CR3 writes moved the fork by 0.5% - inside the noise - because a CR3
write is not a TLB flush on this machine: QEMU runs with `-accel kvm`, the
guest's translations are tagged (`VPID`), and loading a CR3 is a tag switch
rather than an invalidation. The change is kept because it is strictly less work
and the smaller of the two shapes, but nobody should read a speedup into it.

**The cost of a `fork` is the mapping; the `wait4` after it was the clock.**
Taking the arena away cuts the fork from 351 µs to 17 µs - twenty-one times -
and used to leave the `wait4` where it was, at *eight milliseconds either way*.
An earlier draft read the second row as the child's exit and its 8192 frame
references coming home (`release_user_frames`), and the third row refuted it: a
parent reaping a child that owned nothing paid exactly the same 8 ms.

**What that 8 ms was, and why it mattered more than anything in this section.**
One tick of this build's 250 Hz clock, on *every* blocking wait, and the cause
was the boot task. `park()` - where the boot task spends the machine's life -
halted the core with `wait_for_interrupt` instead of blocking in the scheduler,
which left a permanently *runnable* task at NORMAL priority: the picker handed
it the CPU, `hlt` stopped the core while holding it, and a task that has halted
can only be taken off it by an interrupt - of which the one that reliably
arrives is the clock. Anything woken while the idle task was in that state (or
behind it in the ready queue) waited for the next tick. A probe in the scheduler
recording the wake-to-resume latency in whole ticks is what named it: 604 of 640
wakes took *exactly one* tick, never two, while the scheduler itself never went
idle, so the task was ready and the picker was choosing somebody else first.
Parking the boot task in the scheduler puts the idle halt back in the
scheduler's own loop, which is where the virtio-net handler's comment already
assumed it was - "in the scheduler's own context ... the scheduler's next turn
picks the woken task on its own". That is the whole fix: one call site.

| `t-forkcost`, 200 cycles | before | after |
|---|---|---|
| `wait4`, 32 MiB parent | 7 648 682 ns | 1 760 ns - 100 µs |
| `wait4`, no arena | 7 980 378 ns | 13 046 ns - 100 µs |
| `fork`, 32 MiB parent | 351 052 ns | 223 983 - 298 868 ns |

The `fork` row is the control and it is the mapping: it did not move. The `wait4`
rows are now the child's own exit and a picker turn instead of a clock tick, and
the *range* is the honest shape of that figure rather than noise in it: a parent
that reaches `wait4` after its child has already exited does not block at all and
reads ~2 µs, one that gets there first blocks and reads ~100 µs on a loaded
machine - and one of them can print *below* the clock's resolution, where a
negative figure is the TSC fraction's noise floor and not a negative time. What
matters for both is the same: they are tens of microseconds against a tick of
4 ms, which is what `t-forkcost` now *asserts* (§10.7). §10.4 carries the same
fix's effect where it was largest.

**Why PCID is not done, and it is a decision rather than an oversight.** Three
things have to be true before `CR4.PCIDE` pays for itself, and only the first is
straightforward. (1) Each address space needs a stable PCID and a way to
invalidate a *recycled* one - manageable, with 4096 of them and 64 task slots.
(2) The kernel half must be mapped `GLOBAL`, or every process carries its own
copy of the kernel's translations in the TLB: the `G` bit is only honoured on
the entry that maps a page, so making the kernel half global means setting it on
the bootloader's huge-page leaves, and `page_flags` deliberately leaves it off
today for exactly this reason. Without it, a PCID switch trades one flush for a
per-address-space replication of the kernel's working set. (3) Every existing
CR3 write has to be re-audited for *which* PCIDs it must invalidate: with
`PCIDE` on, `activate(root)` invalidates the PCID written and leaves the others
alone, so `map_mmio` and the kernel-half table walks - which is where a write
adds a mapping every address space must see - would leave stale entries in every
*other* PCID, which is a wrong translation rather than a slow one. On top of
that, the workload here is `fork`+`exec`, and every `exec` builds a fresh
`AddressSpace` with a fresh PCID, so the reuse PCID is paid for is rare by
construction: the win would be the parent's entries surviving a child's
lifetime, which the table above suggests is not where the time goes anyway.
Recorded rather than attempted, so the next person starts from the audit list
rather than from the idea.

### 10.3 Syscall Path — **PARTIAL** (the cheap half landed)

**Why**: every scheduler turn and every clock tick scanned all 64 task slots
for an armed interval timer whether any process had armed one, and every
user-buffer operation walked four page-table levels per page.

| Change | Where | State |
|---|---|---|
| A global "any itimer armed" flag; `check_itimers` skips the scan when clear | `sched/runtime.rs` | **done** |
| Validate user buffers with the copy itself (fault handler catches `EFAULT`) | `arch/x86_64/paging.rs`, `syscall/mod.rs` | **not done** — the argument is below |

**The itimer flag.** `ITIMER_ARMED` is set by `itimer_swap` - which is the only
place a timer is ever armed, because no task inherits one and `exec` clears them
- and recomputed *by the scan itself* when it runs. That ordering is the point:
a task that exits while armed disarms a timer without going through
`itimer_swap`, so a flag that was only ever "set on arm" would stay set for the
rest of the boot and quietly restore the full scan. Recomputing costs one extra
scan after the last timer goes away and makes the flag wrong only in the
direction of running the scan - never in the direction of skipping a timer that
is due. Only `ITIMER_REAL` is tracked, because it is the only one that is a
wall-clock deadline the scheduler has to honour.

**Why the fault-driven copy is not here.** The plan's premise does not survive
reading the code. There is no pre-walk separate from the copy: `read_user` and
`write_user` translate one page at a time *as they copy*, through the direct
map, and a read is therefore one walk per page, not two. Only the write
direction walks twice, and the second walk is the copy-on-write probe
(`is_cow_page` before `translate`) - which is a handful of loads against the
page-table entries the walk just touched, next to a `memcpy` of the same page.
Deferring all of it to the CPU's own address translation would mean the kernel
dereferencing a user pointer, and then a `#PF` taken in **ring 0** on a bad
pointer has to become `EFAULT` rather than a panic: that needs an
exception-table/fixup mechanism (identify the faulting RIP as inside the copy
routine, redirect the trap frame to a landing pad, resume), and getting the RIP
range check wrong turns a real kernel bug into a silent `EFAULT`. The safe shape
is kept: the kernel never dereferences a user pointer, so a bad one cannot fault
at all. What is left as a possible future is the one-walk dedup of the write
path, which is a strict reduction in work with no measurable claim attached.

### 10.4 Pipes and Per-Frame Allocations — **PARTIAL** (two of three, measured)

**Why**: pipe reads and writes moved bytes one at a time through a `VecDeque`
(`pop_front`/`extend(iter().copied())`), and the network path allocates and
frees a fresh `Vec` per frame in each direction.

| Change | Where | State |
|---|---|---|
| A ring buffer with slice copies for pipe data | `vfs/pipe.rs` | **done** |
| The raw receive temporary sized from the actual datagram, or on the stack | `net/socket.rs` | **done** |
| A pool of frame buffers behind the smoltcp device seam | `net/mod.rs` | not done |

**The pipe.** `PipeState::data` is now a [`Ring`] over a `Vec` whose live bytes
are `bytes[start..]`: storage taken lazily, reclaimed when the reader catches the
writer, and the dead prefix dropped with one `drain` on the pass where the
arrival would otherwise pass `CAPACITY`. A read is one slice copy and a write is
one `extend_from_slice`, where the old shape was 8192 deque operations for a
`PIPE_BUF` round trip. `t-pipecost` (new, and in the suite) measures it in two
phases, because the two turn out to answer different questions:

| 16 MiB through one pipe | before | after |
|---|---|---|
| one process, 4096-byte units (data path only) | 61.7 ms - 259 MiB/s | 6.1 ms - **2611 MiB/s** |
| two processes, 64 KiB chunks (a pipeline) | 2047.5 ms - 7 MiB/s | 2047.4 ms - 7 MiB/s |

The first row is the change: ten times the throughput, which is what two slice
copies per 4096 bytes instead of 8192 deque operations buys.

**The second row was the finding, and it was bigger than the change.** A
two-process transfer did not move at all when the ring landed - 2047.5 ms to
2047.4 ms - and the arithmetic said why: 2047 ms over 256 chunks is **8.0 ms per
64 KiB chunk**. The reading taken at the time was "two ticks per handover", and
the count was right while the unit was wrong: a chunk is not one handover but
*two blocking waits* - the writer waiting for room in a full pipe, the reader
waiting for data in an empty one - so the figure was **one tick per wait**, and
one tick was the cost of *any* wake on this machine (§10.2). The bytes were
never the cost. `t-pipecost` is the instrument, and it was deliberately in the
suite so the fix had a number waiting for it:

| 16 MiB through one pipe | before | after |
|---|---|---|
| one process, 4096-byte units (data path only) | 6.5 ms - 2 478 MiB/s | 1.6 ms - **10 206 MiB/s** |
| two processes, 64 KiB chunks (a pipeline) | 2047.3 ms - 7 MiB/s | 1.6 ms - **10 168 MiB/s** |

The second row is the whole point of the pair: the same 16 MiB, through the same
pipe, now crosses 256 task boundaries at *the same speed as the copy that
crosses none*. Nothing about the pipe changed for this - the fix is the boot
task's park (§10.2) - and it is the clearest statement of what a wake used to
cost, because the pipeline was previously 1 300 times slower than the data path
it was built out of.

**The raw receive.** `raw_recv` filled a fresh `alloc::vec![0u8; 1500]` and then
copied it into the caller's buffer. It now receives straight into `out`: no
allocation, no second pass over a full MTU, and the "packet larger than the
caller's buffer" case is `recv_slice`'s own `Truncated` - the same `EMSGSIZE`,
detected before anything has been allocated for it. Behaviourally identical for
every packet the MTU admits, which is all the old 1500-byte temporary could
hold either.

**Not done: the frame pool.** The three allocations the plan names for the
network path are the TX build, the RX copy and the raw temporary; the raw one is
gone, and the other two want a pool behind the smoltcp device seam that hands
buffers back rather than freeing them. That is a change to the seam itself
rather than to a call site, and it wants the same treatment as the two above: an
instrument first - a datagram-per-second count with and without the pool - so
that it is landed with a number rather than with a diff.

### 10.5 Filesystem and Block Read Path — **PARTIAL** (the verifiable one landed)

**Why**: ext4's cache was a 512-slot direct-mapped array that thrashes on large
working sets, path resolution reads a directory block per component on a miss,
and each 4 KiB write leaves for the device as eight 512-byte synchronous
requests. Write-through stays: crash visibility is a deliberate semantic, and
only the read side is being tuned.

| Change | Where | State |
|---|---|---|
| Set-associative cache with victim selection | `vfs/ext4/cache.rs` | **done** |
| Sequential readahead past the block being asked for | `vfs/ext4/` | not done |
| virtio-blk interrupt-driven completions and multi-request pipelining | `drivers/virtio/blk.rs` | not done |
| A SATA probe that ends in an answer instead of a budget | `drivers/ahci.rs`, `crates/ahci` | **done, measured** |

**The probe's cost, before and after.** Not a throughput change but the same
ledger, because it is a device wait that the boot pays: on a `q35` machine whose
built-in SATA controller has no disk on it, the gap between `virtio-blk: vda, ...`
and `blk: no data partition ...` in the boot log was **28.412 s** and is now
**0.001 s**, measured on the same image copy with a timestamped serial log
(`-machine q35,sata=off` gives the same 2 ms, which is what named the probe as the
cost). The healthy path is unchanged and still measured - a command on a port
that really has a device retires at try 0-2 - and `t-ahci` is the pin that says
so. `NOTES.md` §7.3 holds the register dump, why a count of MMIO reads is still
the shape rather than a tick deadline, and the two gaps that were left open there
and are now closed.

**The cache.** Four-way set-associative, least-recently-used within a set, over
fast-lazily-allocated slots - and the set count kept at the 512 that the original
`ls` measurement justified, so the change is *only* about collision and not about
size. A block's set is `block % CACHE_SETS`; an empty way takes it first, and
otherwise the way whose block was asked for longest ago, dated on every access
including hits (dating only misses would make the policy "first loaded" rather
than "least recently used"). Slots no longer allocate their 4 KiB buffer at mount
- a slot takes its buffer the first time it holds a block - which is what makes
`CACHE_SLOTS` 2048 total slots (8 MiB, if a filesystem ever touches all of them)
affordable, and which also removes a fixed 2 MiB per mount that used to be paid
whether or not anything was read.

The instrument is a host unit test, not a clock, because a cache's quality is a
*count*: a mock `BlockDev` that counts reads per block and answers with a pattern
derived from the block number, so a slot handed back for the wrong block fails as
content rather than as slowness. The numbers are the same on every machine and
under TCG or KVM, which is why they are worth writing down:

| Trace | direct-mapped (before) | 4-way LRU (after) |
|---|---|---|
| four blocks a set-count apart, accessed round-robin, 4096 accesses | **4096 reads** (0% hits) | **4 reads** |
| a 2048-block sequential scan | 2048 reads | 2048 reads |
| one hot block read between 2048 scan blocks | <=5 reads of it | <=5 reads of it |

The first row is why the ways are there: a working set of four blocks that share
a set - four groups' inode tables, say - made a direct-mapped cache *not a
cache*, with every access evicting what the next one wanted. The second and third
rows are the traces that must not regress, and the second is also the honest
limit of this change: a sequential scan has no reuse in it, so no cache can help
it, and the two runs read the same 2048 blocks either way. 63 ext4 host tests
cover the driver through the new cache, including a byte-for-byte read of a large
file and a mount of an image the host's `mke2fs` made.

**Why the other two are not done, and it is one reason between them.** They
cannot be measured, and they are each other's prerequisite. `BlockDev::read_at`
is capped at one 4 KiB page per call by its own contract - "drivers bounce through
a single page-sized, physically contiguous buffer" - so readahead cannot ask the
device for fewer, larger reads; it can only issue the same reads earlier, and
this kernel's block driver is synchronous and polled, so "earlier" overlaps
nothing. Readahead becomes worth having exactly when item 3 gives the device
asynchronous completions it can be pipelined behind, and until then it is a
prefetch into a cache that the reader is about to miss anyway.

The measurement gap is the real prerequisite, and it is worth stating plainly:
the test machine boots the conformance suite *without a disk*, so no `t-*` program
exercises the block driver or the ext4 read path at all - the numbers above exist
only because the cache policy is pure enough to drive from a host test. Either
item would be landed blind. What both want first is a disk-backed test path: an
ext4 image attached to the test machine plus a program that reads a large file
from it and reports throughput, which is a `xtask` selftest change and a new
`t-*` - and which would give readahead and pipelining the same kind of instrument
`t-pipecost` gave the pipe. Recorded as the prerequisite rather than skipped.

### 10.6 Serial Output — **DONE**

**Why it mattered**: UART transmit used to be polled with the console lock held;
on real hardware a 60-byte line costs ~5 ms of machine-wide stall (QEMU's
emulated 16550 never blocks, which is why it does not show here). The kernel's
own hot paths log nothing, so this only bit on the day they did - which is what
a `bat` over a minified stylesheet turned into a machine that looked dead.

| Change | Where |
|---|---|
| A transmit ring drained by the TX-held interrupt; `write` only enqueues and parks on a full ring, re-arming before *every* park so a write larger than one drain burst cannot sleep unannounced | `drivers/serial.rs`, `console.rs` |
| The framebuffer console's serial **mirror** is best-effort: it hands bytes to the ring and returns, so a host terminal that cannot keep up drops bytes rather than throttling the window or the console mux | `console.rs` (`serial_mirror_now`) |

### 10.7 Sub-Tick Sleeps — planned (the wake half is done, and was not this)

**The wake latency was filed here and did not belong here; it is fixed.** Two
measurements said a *woken* task - not sleeping on a deadline, but parked in
`block_until` and made runnable by an event - did not resume for what looked
like two ticks:

| measured | before | after | instrument |
|---|---|---|---|
| `wait4` reaping a child, whatever that child owned | ~8.0 ms, unchanged when the child's 32 MiB address space is taken away | 1.8 µs | `t-forkcost` (§10.2) |
| a 64 KiB pipe handover between two processes | 8.0 ms per chunk, 7 MiB/s | 6.1 µs per chunk, 10 168 MiB/s | `t-pipecost` (§10.4) |

Neither was a sleep rounding to a tick, which is what this section is for: the
woken task was *ready* and the picker was not choosing it, because the boot
task's `park()` halted the core while remaining runnable at NORMAL priority and
a halted task is only taken off the CPU by an interrupt. The scheduler never
went idle once during the whole suite, which is the observation that separates
"the task is not runnable" from "the task is runnable and somebody else is
running" - and a probe recording wake-to-resume in *ticks* named the last one
directly: 604 of 640 wakes took exactly one tick. §10.2 has the fix and the
numbers.

**The bound is now written, in the one place it is honest.** `t-forkcost`
asserts that *both* of its `wait4` rows - each a 200-round average - are under
one clock tick. A wake must be a scheduler turn and not a deadline, and the
before-figures above are *clock-quantized*: they sat at two ticks (7.6-8.1 ms)
in every run, because a wait that ends on the clock cannot land between ticks.
So the bound sits ~40× above the fixed figure (tens of microseconds) and below a
broken one that cannot drift down to meet it. `t-pipecost` still only *reports*,
deliberately: its figures are throughputs, and a bound that holds under KVM can
be a flaky failure under TCG, which is its own stated rule. The wake is where
the quantized claim can be made, and it is the claim that mattered.

**Why**: the tick is the smallest unit a sleep can name (4 ms at 250 Hz).
Event-driven waits ended the network's dependence on it; the remaining
callers - `nanosleep`, timerfd, TCP retransmit pacing - still round to whole
ticks.

**What is already there, and what is actually missing**: reading time is *not*
the gap. `timer::since_boot_nanos()` already resolves below the tick from the
TSC, with `CYCLES_PER_TICK` calibrated from two observed ticks - so a
`nanosleep(200µs)` knows precisely when it is late. The gap is the *wake*:
`sleep_ticks` parks the task on the tick counter, so the smallest unit of
sleeping is still one PIT interrupt. Design the change around arming a deadline,
not around a finer clock.

**The order that stays bootable** - the timer is what the whole scheduler rests
on, so each step must be green before the next:

| Step | Change | Where |
|---|---|---|
| 1 | Program the LAPIC timer once, in **one-shot**, from a calibrated count: divide by 16, measure the bus cycles per tick against the PIT *before* enabling, and refuse to go sub-tick at all when the calibration says the LAPIC timer is not ticking (KVM/TCG differ here). | `arch/x86_64/lapic.rs`, `arch/x86_64/timer.rs` |
| 2 | A vector for it in the IDT, and a handler that wakes only the task whose deadline armed it - not the whole tick pass, which would put the cost back. Keep the PIT as the periodic floor so a lost one-shot cannot stall the machine: the tick still advances time, the one-shot only sharpens the wake-up. | `arch/x86_64/idt.rs`, `sched/` |
| 3 | `sleep_ns`, and have `nanosleep`/timerfd/`block_until_deadline` take it: below one remaining tick arm the one-shot, otherwise park on the tick as now. The deadline a wait already carries is in ticks today - widening it to nanoseconds is the edit everything else hangs on. | `sched/policy.rs`, `sched/runtime.rs`, `vfs/timerfd.rs` |
| 4 | Measure it: a guest test that sleeps 1 ms twenty times and asserts the elapsed `since_boot_nanos()` is near 20 ms and *not* near 80 ms (twenty 4 ms ticks). Without the number the change is a claim. | `user/abitest/src/bin/` |

**The trap to watch**: one-shot timers and *interrupt coalescing* fight. If a
one-shot is armed while another is pending, arming must not push the earlier
deadline out - a task whose wake-up got postponed by somebody else's sleep is a
latency bug that only shows under load, which is where it will be found first.

---

## Phase 11: Architectural Debt — **AUDIT DONE; STEPS A–G LANDED, AND V-1/V-4/V-13 CLOSED** (D.2b evaluated & rejected as no-value; surviving `drivers ↔ vfs` pair is its own finding)

Everything above this phase is about *features*: what the kernel cannot yet do. This phase is
about the *shape* of the kernel that does them, and it opens with a measurement rather than an
opinion, because "bad architecture" is not a finding until something counts it.

`ARCHITECTURE.md` is the companion document: it draws the module graph, the boot sequence and
the syscall path, and its §9 mirrors the findings below. This section is the work list.

**The audit itself was written without editing a file** — every finding below was read out of the
tree, and each says where. What has landed since is marked on the finding and on the step: **A**
(the dead modules and the leftover trace), **E** (the seven pure leaves as crates) and **F** (the
gate's written criterion and the check for it). B, C, D and G are open.

---

### 11.0 How the audit was made

Reproducible in a few seconds, and worth re-running after any structural change:

```
# module-level edges: map every file to its top-level module, collect crate::<mod> refs
python3 - <<'PY'
import os,re,collections
files=[os.path.join(r,f) for r,_,fs in os.walk('src') for f in fs if f.endswith('.rs')]
top=lambda p:(lambda q:q[0][:-3] if len(q)==1 else q[0])(os.path.relpath(p,'src').split(os.sep))
edges=collections.Counter()
for p in files:
    for m in re.finditer(r'crate::([a-z_][a-z0-9_]*)',open(p,encoding='utf8',errors='ignore').read()):
        if m.group(1)!=top(p): edges[(top(p),m.group(1))]+=1
for (a,b),w in sorted(edges.items()): print(f"{a:12} -> {b:12} {w}")
for (a,b) in sorted(edges):
    if (b,a) in edges and a<b: print("CYCLE",a,b)
PY

# god files
find src -name '*.rs' | xargs wc -l | sort -rn | head -12

# orphan modules: a file under arch/x86_64 that mod.rs does not declare
cat src/arch/x86_64/mod.rs
```

The three numbers that came out of it, re-measured after Step A (§11.2):

| Measurement | Result |
|---|---|
| Kernel size | 80 239 lines across 114 `.rs` files (plus `boot/` 1 225 in 4 files and `xtask/` 4 396) |
| Host unit tests | 501 `#[test]` in `src/`, of which 498 run in the host build - the other three are `net/socket.rs`'s, behind the gate V-11 names - and 29 in `xtask/` |
| Mutual module pairs (a cycle of length 2) | **21**, among the eleven modules that appear in one: `arch`, `console`, `drivers`, `mm`, `net`, `pci`, `rand`, `sched`, `syscall`, `vfs`, `uevent` |

The audit was published with 79 095 lines across 116 files, 491 + 25 tests and *23* pairs. Re-running
its own script corrects two of those and the correction is worth keeping: the tree has grown since
(the batch that lands with this document), and **the pair count was one too high** - the list
`ARCHITECTURE.md` §6 prints has 22 entries, which is what the re-measurement finds before Step A.
Step A then takes it to 21, for the reason §11.1.7 records: one of those pairs came from a file the
compiler never saw.

**The same extraction runs as a check now** (§11.2 Step G, `cargo xtask depgraph`), and running it
that way found a third correction of the same kind: this script matches *text*, so an edge it
reports can be a doc comment. Measured with `//` comments stripped — i.e. who names whom in code —
the tree had **20** mutual pairs, not 21: `uaccess → syscall` is a mention in a doc comment, and
`vfs → uaccess` with it. The ratchet holds the code number — **19** since Step C, which took
`sched → net` out and with it one pair — and the three baselines it records (re-measured
2026-10-02) are in §11.2 Step G.

That last row is the finding. At module granularity the kernel is one strongly connected
component: there is no layer that can be compiled, tested or refactored on its own. Every
other item below is a *cause* of that number, which is why the fixes are ordered to reduce it
rather than to make any single file prettier.

---

### 11.1 The findings

Severity is about cost-to-change, not about user-visible bugs: none of these is a defect a
program can observe today.

#### 11.1.1 — V-1: the `Inode` downcast chain (severity: high) — **CLOSED**

`src/vfs/mod.rs:453–546` gave the central VFS trait a method per concrete device and network
type:

```rust
fn as_fb(&self) -> Option<&crate::drivers::virtio::gpu::FbDev> { None }
fn as_evdev(&self) -> Option<&crate::drivers::virtio::input::EvdevNode> { None }
fn as_uinput(&self) -> Option<&crate::drivers::uinput::Uinput> { None }
fn as_drm(&self) -> Option<&crate::drivers::drm::DrmCard> { None }
fn as_vsock(&self) -> Option<&crate::net::vsock::VsockSock> { None }
fn as_unix(&self) -> Option<&crate::net::unix::UnixSock> { None }
```

(The list grew to nine while the audit was being written — `as_syncobj_fd`, `as_sync_file` and
`as_packet` are the same shape and arrived with the DRM sync-object and AF_PACKET work.)

This is the textbook abstraction leak: the VFS — which is supposed to know only that a node has
a name, a kind, bytes and an owner — names six device drivers and three network socket types,
and the *syscall* layer reaches through these to drive a device. Adding a seventh device type
means editing the VFS trait, so the VFS can never be compiled without `drivers` and `net`, and
neither can anything that depends on the VFS. It was the largest single contributor to the
23-cycle count.

**Closed by Step B1 (§11.2)**: all nine are gone and `src/vfs/mod.rs` names `crate::drivers::`
and `crate::net::` zero times. What replaced them is one type-erased handle that the *caller*
names a type through — `core::any::Any` as a supertrait plus a single upcast — so the question
"is this node an `FbDev`?" is asked by the layer that knows what an `FbDev` is, and the trait
that answers it never hears the name. Adding a tenth device type now touches no VFS code.

**What it deliberately did not fix**, so the next reader does not expect it: the
`drivers ↔ vfs` mutual pair stands. `vfs` still names `drivers` 17 times and `net` 4, from
`vcmux`'s render/drm calls, `procfs`/`sysfs`'s PCI config reads and `p9`'s virtio-9p device —
each of which is a *different* finding (V-4's remaining half, and the PCI-config and 9p doors),
with its own step. V-1 was the downcast cause and it is the one that is closed.

#### 11.1.2 — V-2: device ioctl ABI lives in the syscall layer (severity: high)

`src/syscall/linux/fs.rs` is 6 311 lines and is the second-largest file in the tree. It is not
only path-carrying file calls; it is also the switchboard for every device's ioctl contract:

| Device | Where its ABI is parsed |
|---|---|
| fbdev (`FBIOGET_*`, `FBIOPAN_DISPLAY`, `FBIO_WAITFORVSYNC`) | `fs.rs` around `:3561` |
| RTC (`RTC_RD_TIME`, `RTC_SET_TIME`) | `fs.rs` around `:3677` |
| DRM/KMS (`drm_ioctl` dispatch) | `fs.rs` around `:3751` |
| uinput (`UI_SET_*`, `UI_DEV_SETUP`, `UI_DEV_CREATE`) | `fs.rs` around `:4001` |
| TTY, block, evdev, termios | `fs.rs` throughout |

A driver change therefore lands in the syscall layer, and the driver cannot state its own
contract next to the code that implements it. The dispatch routing counts make the split
explicit: 135 of the 247 names are handled by `fs.rs`, 50 by `proc.rs`, 50 by `posix.rs`, 12 by
`abi.rs`.

#### 11.1.3 — V-3: the scheduler does other subsystems' cleanup (severity: high) — **CLOSED**

`sched/runtime.rs` is 6 397 lines — the largest file in the tree — and on task teardown it
reaches into four unrelated subsystems:

```rust
crate::net::socket::close_sockets_at_exit(&files);
crate::vfs::inotify::close_at_exit(&files);
crate::drivers::virtio::input::close_at_exit(&files);
crate::drivers::uinput::close_at_exit(&files);
```

plus `vfs::locks::release_process` and `vfs::mount::fs_id_of_inode` in the same path, and
`binfmt::shebang`, `vfs::read_file`, `vfs::tty::global` and `vfs::tty::by_dev_id` in the exec
path. The scheduler owns *tasks*; it should not know that inotify exists. Each new
per-open-file resource adds a line here, and each line makes `sched` unbuildable without `net`,
`vfs` and `drivers`.

**Closed.** Step C turned the exit path into a registry (`sched::exit`) each subsystem
registers into from `main.rs`, zeroing `sched → net`/`vfs`/`drivers` on that path. The one
edge it left standing was `execve`'s `FD_CLOEXEC` pass, which named the two input drivers
directly. That pass now walks a *second* registry, `sched::exit`'s cloexec list, into which
`drivers::virtio::input` and `drivers::uinput` register the same hooks (from `main.rs`); the
two lists are deliberately not merged — `execve` tells fewer subsystems than `do_exit`
because the process lives on — so the distinction Step C refused to flatten into a flag is
now carried in two typed lists instead. `sched → drivers` is 0, the `drivers ↔ sched`
mutual pair collapses, and **mutual pairs fell 12 → 11**: the last ratchet drop of Phase 11
and the precondition for un-gating `sched` for aarch64 (§9.1 stage 6h-adjacent work). Pinned
by a host test that an exit hook stays silent under `run_cloexec` and vice versa, and by
`cargo xtask test`'s 142 user tests still closing CLOEXEC descriptors on `execve`.

**A related `sched → syscall` reduction (the `abi` leaf).** V-3 was about naming
*subsystems*; the `sched → syscall` edge (Step C/F's `BASELINE_LOWER_LAYER_TO_SYSCALL`
row, recorded at 10) was about naming *ABI values* the scheduler needs but does not
dispatch — the default `umask`, the `clone` flag bits, and the resource-limit model.
These are arch-neutral (asm-generic `CLONE_*` and `RLIMIT_*` carry the same number on
x86_64 and aarch64), so they moved to a new `crates/abi` leaf that `sched` names
downward and that `syscall::linux::{nr,abi}` re-export rather than redefine (one table,
not two). `MMAP_BASE`, the other former `sched → syscall` read, moved to `crate::mm`
(an address-space layout fact beside `USER_LIMIT`, not a dispatch one) with a per-arch
value: x86_64 keeps `0x7f00_0000_0000`, aarch64 gets one derived from `paging::USER_END`
(`1 << 48`) as *this* kernel's layout choice, to be measured against Linux's aarch64
mmap base at stage 7 when a process runs. `sched → syscall` fell 10 → 1: the last
reference was the ptrace/stop block's `abi::uregs::COUNT` (the x86 `user_regs_struct`
field count sizing ptrace state). **It is 0 now** (§13.2): that field count is the
machine's (`hal::Contexts::ABI_REG_COUNT` — 27 on x86_64, 34 on aarch64), the block's
`PTRACE_*`/`SYSCALL_TRAP` protocol words moved into this same leaf, and the
`target_arch` gate that briefly stood in for the dependency is gone — so the edge is
empty on both machines, and the `sched ↔ syscall` mutual pair with it. The leaf is `#![no_std]`, host-tested (`clone` values and
the `setrlimit` soft/hard/root rule pinned), and added to the host-test crate list in
`xtask/src/selftest.rs` and the `ARCHITECTURE.md` §1 graph.

#### 11.1.4 — V-4: the terminal emulator and console live inside the VFS (severity: medium) — **CLOSED** (the `vfs ↔ console` pair is broken; emulator + VC vocabulary + `vtmode` + active-console number in the `term` leaf; see §11.2 Step D)

`crates/term/` (the `mpos-term` crate) holds the cell grid, the key handling and an embedded 4 KiB `vga8x16.bin` font.
`src/vfs/tty.rs` (2 359 lines), `src/vfs/pty.rs`, `src/vfs/devpts.rs` and `src/vfs/vcmux.rs`
implement the console, and `src/console.rs` — the kernel's own logging console — reaches *into*
the filesystem layer to draw:

```rust
crate::term::with_screen(active_grid(), |g| g.feed(bytes));
crate::drivers::video::render::notify();
```

The direction is wrong: a terminal emulator is not a filesystem. `/dev/tty` being a node is a
filesystem concern; how a cell is painted and which of six virtual consoles is active is not.

#### 11.1.5 — V-5 and V-9: user-pointer validation owned from below (severity: medium)

`drivers/drm.rs:595,741` calls `crate::syscall::buffer_ok(...)`, and `net/socket.rs` /
`net/unix.rs` call `crate::syscall::buffer_ok` and `err_of` dozens of times. A driver validating
a *user* pointer is an inversion — the driver is below the syscall layer in every other respect
— and it exists because the helper happens to live there. The helper itself is fine; its home
is not.

The re-audit at the AF_PACKET/sound landing measured the inversion's bulk differently from how
it was first described: most of the ~110 below-`syscall` references were not the two helpers at
all but the **errno constants** — `net/*` (100+), `sched/*` (17), `vfs/epoll.rs` /
`vfs/timerfd.rs`, `drivers/virtio/sound.rs` (7) and `uevent.rs` each reaching up for a minus
sign. That half is now **closed**: the table lives at the crate root as `crate::err`
(`src/err.rs`, re-exported through `syscall` so the layer's own paths are unchanged), and
`uevent`'s second upward edge — `self_cred` reading the task table — moved to
`net::socket::task_identity`, an allowed `net→sched` edge, with `uevent::bind` taking the
preferred port id as a parameter so the bus stays host-testable. What *remains* of V-5 and V-9
is exactly `buffer_ok`/`err_of`: 24 `err_of` calls in `net/socket.rs`/`net/unix.rs` and 2
`buffer_ok` calls in `drivers/drm.rs` plus `net/*`'s pointer checks. B3 below is that move,
and it is now the whole of what those violations are.

#### 11.1.6 — V-6: a temporary trace in the dispatch hot path — **DONE**

`syscall/linux/mod.rs::trace_msg` was self-labelled:

> TEMPORARY (udevd worker-channel hunt): ... Remove once the site is known.

It fired on every failed `sendto`/`recvfrom`/`sendmsg`/`recvmsg` and printed up to 60
`[msgtrace]` lines from inside the dispatcher. The hunt it belonged to is recorded as finished
in §4.1's udevd note, so it was leftover scaffolding standing in the one path every syscall
crosses. **Removed, and the four arms are plain calls again** (§11.2 Step A).

The class is worth watching, because it happened twice: the same hunt later left a second
instrument behind - a `[recvtrace]` wrapper around `net::socket::handle_recvmsg` (the original
function renamed `recvmsg_inner`, a logging body around it, and `console.rs`'s `DMESG_CAP`
temporarily raised so the ring would not evict the lines). It was still there when the session
that added it ended, with the bug it was hunting found by reading eudev's source instead: the
`recvmsg` that failed was `EINVAL` for a *zero-length iovec* (§4.1). **Removed**, and the
wrapper collapsed back into one function; the per-task cost of the shape is that a diagnostic
in the dispatcher or the socket layer is easy to leave, and neither is in anybody's diff when
the session ends.

#### 11.1.7 — V-7: orphan and duplicate machine modules — **DONE**

| File | Was |
|---|---|
| `src/arch/x86_64/serial.rs` (119 lines) | **not declared in `arch/x86_64/mod.rs`** → never compiled. A near-duplicate of `drivers/serial.rs` (324 lines), same doc header |
| `src/arch/x86_64/qemu.rs` (23 lines) | Not declared → never compiled. **Byte-identical** to `drivers/qemu.rs` |
| `src/arch/x86_64/pci.rs` | Declared, compiled, **referenced nowhere**. A stale earlier copy of `drivers/pci.rs` (discovery-only, one function per slot, bus 0) |
| `src/arch/x86_64/rtc.rs` vs `src/drivers/rtc.rs` | Both live and both used — but the names collided and the split was not obvious from either name |

Three modules named `pci`, two named `rtc`, two named `serial`, two named `qemu`; two of the
twelve were dead and one was a duplicate, which is how a reader learns to distrust the tree.

**All three files are deleted** and the module declaration with them; `drivers::rtc` is renamed
`drivers::rtc_dev` (the node), which leaves `arch::x86_64::rtc` as the only thing called `rtc`
and the clock as the one that owns the name. The `console.rs` module header, which described
itself as formatting over `arch::serial`, now names `drivers::serial`.

*One number fell out of the deletion that is worth keeping.* §11.0's extraction counts mutual
module pairs by reading every file under `src/`, and it read the two orphans even though the
compiler never did: the `arch <-> vfs` pair existed **only** because `arch/x86_64/serial.rs`
mentioned `crate::vfs`. Deleting it takes the count from 22 to 21 - a reminder that an audit by
text search measures the tree, not the build, which is the same lesson V-7 itself is about.

#### 11.1.8 — V-8: the aarch64 configuration points at a missing file — **DONE**

`.cargo/config.toml` set `-C link-arg=--script=arch/aarch64/linker.ld`; only
`arch/x86_64/linker.ld` exists. The target could not link. §9.1 calls aarch64 "under
development", which understates it: there is no linker script, and `src/arch/mod.rs` says the
modules will be declared "when they are written rather than now".

**The profile is gone** (`[target.aarch64-unknown-none]` in `.cargo/config.toml`, and the target
in `rust-toolchain.toml`), with a comment in its place saying what it was and where the port's
real prerequisites are (§9.1). Re-adding it is one line, next to a linker script that exists -
which is the point: the configuration no longer advertises a target that cannot be built. The
alternative the step offered - writing the linker script now - would have been a claim about a
port nobody has started, and this step is subtraction.

#### 11.1.9 — V-10: the handoff ABI is written twice (severity: accepted)

`boot/src/handoff.rs` and `src/boot/handoff.rs` are separate definitions of one record, and
`BOOT_STACK_CANARY` is spelled out in `src/board/x86_64.rs` as well as in the loader. This is deliberate
— the loader is a UEFI application on a different target and cannot link the kernel crate — and
it is documented on both sides. Recorded so it reads as a decision rather than an oversight.

#### 11.1.10 — V-11: a host test module behind a never-host-compiled parent (severity: low) — **DONE**

`src/lib.rs` gates `pub mod net` on `target_os = "none"`, so the module is absent from every
host build — while `src/net/socket.rs` annotated a test module
`#[cfg(all(test, not(target_os = "none")))]`, i.e. "run me on the host". The annotation was only
ever evaluated in a build that did not include the file, so those three tests had never run.
`net`'s doc comment says its tests "are the conformance programs that run on the machine",
which is true of the rest of `net` and was false of this file.

**Settled by Step F, and the reading that settled it was of what the three tests pinned rather
than of how to make them run.** They were `send`/`recv` on a `socketpair`, the
`sendmsg`/`recvmsg` shapes on the same channel, and its two refusals (`EISCONN` for a
destination, `EAGAIN` for `MSG_DONTWAIT` on an empty one) — and every one of those is pinned,
line for line and in more detail, by `user/tests/src/bin/t-sockmsg.rs`, a program the sweep
*does* boot (its own header names the caller it exists for: sudo's monitor channel). The module
is deleted, the pins are where they always actually were, and the criterion and its mechanical
check are §11.2 Step F's below.

#### 11.1.11 — Sizes, for the record

| File | Lines |
|---|---|
| `src/sched/runtime.rs` | 6 397 |
| `src/syscall/linux/fs.rs` | 6 311 |
| `src/net/socket.rs` | 3 642 |
| `src/vfs/ext4/mod.rs` | 3 391 |
| `src/syscall/linux/proc.rs` | 3 039 |
| `src/syscall/linux/abi.rs` | 2 903 |
| `src/drivers/drm.rs` | 2 371 |
| `src/vfs/tty.rs` | 2 359 |
| `src/vfs/procfs.rs` | 1 996 |
| `src/vfs/mount.rs` | 1 681 |
| `src/main.rs` | 1 625 |

Long files are a symptom here, not the disease: `ext4/mod.rs` is long because a filesystem is
large, and splitting it would buy nothing. `sched/runtime.rs` and `syscall/linux/fs.rs` are long
*because each holds several subsystems' concerns*, which is what 11.1.2 and 11.1.3 are about.
That distinction is what the plan below is built on: split by responsibility, not by length.

#### 11.1.12 — V-12: the in-flight refactor, and the green baseline it was blocking (resolved)

This item was written while the tree did not build: an in-flight refactor (the `user/abitest`
programs moving into `syscall/` and `integration/` subdirectories, plus edits across `vfs/ext4`,
`vfs/tty.rs` and the syscall layer) left `cargo test -p mpos --lib --target
x86_64-unknown-linux-gnu` **failing to compile** — `Tty`/`TtyOutput` unresolved in `vfs/tty.rs`
and a borrow error in `vfs/ext4/super.rs:501` — and it said no step below should begin until
that work was finished or reverted, because every acceptance criterion here is "the host tests
still pass".

**It is finished and committed, and the baseline is green.** `cargo xtask test` reports `498
passed` host unit tests, `29` from xtask, `=== SUMMARY pass=120 fail=0` from the guest suite, and
QEMU exit `33` (the clean debug exit). The refactor is what the batch of commits ending at this
one carries: the suite split into two tiers, and the session's kernel work (seccomp's filter
mode, the terminal's window, ramfs link counts, the ext4 host tests, a cross-task user copy that
could reach a supervisor page, and the boot task's park that cost a clock tick on every wake).

So the prerequisite is met and **Step A may begin**. It is the one item in this phase that needs
no green baseline anyway — it is a deletion — but it has one now, which is what makes its own
acceptance criterion ("the host test suite and a boot are unchanged") checkable.

---

### 11.2 The remediation plan

Ordered so each step is independently verifiable, behaviour-preserving, and leaves the machine
bootable. Every step states its acceptance criterion as something a test can decide.

The dependency direction to converge on (also in `ARCHITECTURE.md` §9):

```
drivers, net, blk, pci, rand, uevent   ->  may use arch, mm, sched, sync
      vfs, sched                       ->  may use mm, arch, sync, blk, pci
     syscall                           ->  may use vfs, sched, mm, net, drivers
       main                            ->  may use everything
```

Two hard rules: **nothing below `syscall` names `syscall`**, and **nothing outside `vfs` names
the VFS's concrete device types**.

---

#### Step A — Subtract: dead modules, the leftover trace, the phantom target (V-6, V-7, V-8) — **DONE**

*No behaviour change. Do it first: it is a deletion, and it removes the clutter that makes the
rest of the audit hard to read.*

| Change | Where |
|---|---|
| Delete `src/arch/x86_64/serial.rs`, `src/arch/x86_64/qemu.rs` (uncompiled) and `src/arch/x86_64/pci.rs` (compiled, unreferenced) | `src/arch/x86_64/` |
| Delete `trace_msg` and its call site from the dispatcher | `src/syscall/linux/mod.rs` |
| Either add `arch/aarch64/linker.ld` or drop the `aarch64-unknown-none` rustflags and target, so the configuration stops naming a file that does not exist | `.cargo/config.toml`, `rust-toolchain.toml` |
| Disambiguate the colliding names: `drivers/rtc.rs` → `drivers/rtc_dev.rs` (the *node*), leaving `arch::x86_64::rtc` as the clock; `drivers/qemu.rs` becomes the only `qemu` | `src/drivers/` |

**Acceptance:** `git grep 'arch::x86_64::serial\|arch::x86_64::qemu\|arch::x86_64::pci'` returns
nothing (the one stale doc-comment mention in `console.rs` is corrected); the host test suite and
a boot are unchanged apart from the absence of the `[msgtrace]` lines; `.cargo/config.toml` no
longer references a nonexistent path.
**Risk:** the only real risk is deleting something referenced through a macro. Grep first; for
two of the three files the absence of a `mod` declaration is proof enough.

**Done, and what the run said.** All four rows landed, one commit, no behaviour change.

- The three files are gone and `pub mod pci;` with them.
- `trace_msg` and its four call sites are gone; the socket arms are plain calls again, and the
  only sign the trace existed is that a failed `sendto` no longer prints a line.
- The aarch64 profile is gone from **both** files rather than the linker script being written:
  §11.1.8 has the reasoning, and the short form is that this step subtracts.
- `drivers/rtc_dev` is the rename, done with `git mv` so the history follows it, and its module
  header now says why the name carries `_dev` (`arch::x86_64::rtc` is the clock).

`git grep` for the three old paths returns nothing; the host suite is still **498 passed, 29 in
xtask**; `cargo xtask test` ends the same way it did before the change - `=== SUMMARY pass=120
fail=0` and QEMU exit **33** - which is the "unchanged apart from the `[msgtrace]` lines"
criterion, checked by running it rather than by reading the diff.

The one number that moved is the pair count, from 22 to 21, and §11.1.7 says why: the extraction
reads files the compiler never saw.

---

#### Step B — Give devices a seam the VFS can hold without naming them (V-1, V-2, V-5) — **DONE (B1, B2, B3)**

The largest step, and the one that pays for the others: it removes the largest cause of the
23-cycle count.

**B1 — Replace the nine `as_*` downcasts with one type-erased handle. — DONE.** The plan offered
a `vfs::DevNode` carrying an `Arc<dyn DevOps>` with `read`/`write`/`ioctl(cmd, arg)`, installed at
boot, or a single `as_any`/`downcast_ref` pair over `core::any::Any`, and said either was fine
because "the point is that `vfs` stops naming the types". **The second shape was taken, and the
reason is worth recording because the first reads like the better design.** `DevOps` does not fit
the nine it had to replace. Six of them are device downcasts the syscall layer uses to reach an
ioctl *the device already owns* (B2 gave every family its own `ioctl`), and the other three are
`net` recovering its own `UnixSock`/`VsockSock`/`PacketSock` from an `Inode` it minted itself —
not an ioctl question at all, so a `DevOps` covering them would have to be a god-trait. Worse,
`dev().ioctl(...)` *reorders the dispatcher*: every device would have to answer `ENOTTY` for every
family it does not own, instead of the dispatcher routing by family first. That is the plan's own
named top risk, "this touches every device path at once", and it buys nothing the downcast does
not.

The shape actually taken, in `src/vfs/mod.rs`:

```rust
impl dyn Inode {
    pub fn as_any(&self) -> &dyn core::any::Any { self }
}
pub trait Inode: Send + Sync + core::any::Any { ... }
```

Two properties make it nearly free. `Any` is implemented for every `'static` type already, so
none of the tree's 52 `impl Inode for` blocks grew a method — and it could not have been a default
body on the trait, because coercing `&Self` to a trait object needs `Self: Sized`, which a `dyn
Inode` is not. And the body is a plain trait upcast (stable since Rust 1.86), so it is the
vtable's own supertrait entry rather than a lookup. Call sites read
`inode.as_any().downcast_ref::<FbDev>()`, which answers `Some` exactly where the `as_*` method it
replaced answered `Some` — the same `TypeId` comparison the old override (`Some(self)`) and
default (`None`) made, just moved from the trait into `core`.

Migration order followed the plan — one device per commit, fbdev first and DRM last — which is
also the order of the seven commits: fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the
DRM card's three node types (`as_drm`, `as_syncobj_fd`, `as_sync_file`) together, since they are
one device. Each commit deleted its `as_*` method and its `impl` overrides, so the ratchet fell
as it went: `vfs/mod.rs -> drivers|net` 9 → 8 → 7 → 6 → 5 → 4 → 3 → 0, and the constant in
`xtask/src/depgraph.rs` is now 0. Every commit was verified with a bare-metal build, fmt, clippy on
both targets, and the full gate; five of the seven have their device's conformance program in the
default sweep (`t-fbdev`; `t-evdev`/`t-ioctl`; `t-unix`/`t-unix-dgram`/`t-unix-scm`; `t-drm`/
`t-drm-args`/`t-drm-lease`/`t-drm-sync`; `t-prime`). Two do not: AF_PACKET's is `t-rawicmp`, which
`tests.toml` puts in `external`, and was run by name (`--groups external`, green); AF_VSOCK has
**no** conformance program at all — a test boot deliberately gets no host vsock bus — so its
guarantee is the type check plus the equivalence above, and saying so is better than implying a
test ran.

**Acceptance — met.** `crate::drivers::` and `crate::net::` do not appear in `src/vfs/mod.rs`;
`cargo xtask depgraph` prints `vfs/mod.rs -> drivers|net 0/0` and `Within the ratchet: no edge has
appeared and no count has grown.`; and the full gate on the finished tree (2026-10-02, after all
seven device commits) is:

```
Unit tests: the kernel and its leaf crates (host)   528 passed (kernel) + 161 in the twelve
                                                   leaf crates + 47 xtask = 736 passed,
                                                   0 failed
self-test guest: === SUMMARY pass=140 fail=0 (140 built)
self-test guest: 10 of 150 user tests left out by tests.toml
self-test guest: exit 33 (33 is the clean debug exit)
```

Two of the original criteria are **not** met, deliberately: `wc -l src/syscall/linux/fs.rs` did not
drop from B1 (it *rose*, by the imports that now name the device types — the 212-line drop was
B2's, and a downcast is not a line count), and the mutual pair count stayed 18 *then* (D.2a later
took it to 17 — see Step D), because `drivers`
and `net` still name `vfs` and `vfs` still names them from `vcmux`/`procfs`/`sysfs`/`p9`. Those
references are V-4's and the PCI-config/9p doors' — different findings, different steps; B1 closed
the downcast cause, which is what V-1 was.

**B2 — Move each device's ioctl contract down to its driver.** For each family in 11.1.2
(fbdev, DRM, RTC, uinput, tty, evdev, block), move the request-number constants, struct layouts
and validation into the driver module as `pub fn ioctl(req, arg, from_user) -> isize` (or a small
table), leaving `syscall/linux/fs.rs` with the dispatch to that entry point. The *pure* parts stay
host-compiled and keep their tests; `drivers/drm.rs` already carries most of the DRM UAPI, so it
is closer to this shape than it looks.

**B3 — Move the user-pointer helper out of `syscall`.** `buffer_ok`/`err_of` answer "is this range
readable/writable by the current task", which is an `mm` question. Add
`mm::user::{validate, copy_in, copy_out}` and have `syscall`, `net` and `drivers` all call that.
This deletes V-5 and V-9 at once. (Done ahead of it, at the AF_PACKET/sound landing: the *other*
thing below `syscall` was after — the errno table — moved out to `crate::err` (§11.1.5), and
`uevent`'s task-table reach went with it (V-14). What B3 owns now is exactly `buffer_ok`/`err_of`.)
**LANDED (this pass) — V-9 closed.** The two helpers moved to a crate-root leaf `src/uaccess.rs`
(`buffer_ok`/`user_accessible`/`MAX_IO` plus the machine-gated `err_of`), mirroring how `err` was
relocated. The shape is `crate::uaccess`, not `mm::user`: `err_of` maps a `VfsError` (a `vfs` type)
and `buffer_ok` walks `arch::paging` — neither is an `mm` concept — and a root leaf is what lets any
layer below `syscall` reach it without naming the ABI layer. The 99 below-`syscall` call-sites in
`net/{socket,unix,packet,vsock}.rs` and `drivers/drm.rs` now name `crate::uaccess::*`; `syscall`
re-exports the same names for its own handlers, so its ~350 internal sites and `CallerScan` are
unchanged. V-5's `drm` half is closed the same way (the `sound.rs`→`syscall::linux::abi` edge is the
V-2 ioctl-ABI concern, not B3's). This is the prerequisite for B2 (kd/evdev 接受-拒绝下沉到驱动), now
unblocked. Verified: bare-metal build green + host suite `570 passed` + bare-metal clippy clean; the
`cfg(target_os = "none")` split of `err_of`/`user_accessible` moved intact, applying V-11's lesson.
**B2 (evdev 下沉到驱动) — attempted, destabilized by a latent DMA bug, root-caused, then LANDED.** Sinking
`evdev_ioctl` into `EvdevNode::ioctl` (the input driver, mirroring the `'d`→`drivers::drm::drm_ioctl`
precedent, with fd-resolution kept at the syscall boundary) was byte-for-byte the same logic and
compiled for both targets, yet the guest panicked *reproducibly* mid-suite: the `assert!` in
`mm::virt_to_phys` (`src/mm/mod.rs:97`) fired on a user address handed to it from a `write`-path
caller (`virt_to_phys(0x3f…): not an address from the direct map`), aborting before the run ended
cleanly. Reverting the move restored `=== SUMMARY pass=135 fail=1` with no panic. A pure,
behaviour-preserving relocation changes no ioctl answer, so this is a **latent memory bug** *exposed*
by the code-layout change, not an evdev-logic defect — and the caller is now *located*, not guessed:
of `virt_to_phys`'s four live call sites, the one that runs on a `write` (nr 1) path with a buffer
pointer is the virtio DMA helper `drivers/virtio/hal.rs:59` `virt_to_phys(buffer.as_ptr())` (the
framebuffer callers are GPU-only, `free_frames` gets a direct-map address back); i.e. the virtio-net
TX / sound DMA path handing `virt_to_phys` a buffer that is not in the direct map — the *same*
net/audio area as the in-flight `t-dhcp`/`t-dsp` reds and the user's active `net/*` + `sound.rs`
work. **Resolved the same session:** the sound driver's wrong PCI id (`0x1055`→`0x1059`) and its
user-address DMA hand-off were fixed, the gate reached `=== SUMMARY pass=136 fail=0`, and the evdev
relocation was re-applied on that clean tree — bare-metal build green, host suite `570 passed`, and
**guest `pass=136 fail=0` with `t-evdev`/`t-ioctl`/`t-seat` all green**, proving `EvdevNode::ioctl`
answers identically to the old switchboard. So B2-evdev **LANDED**: the `'E`-typed ioctl serve is now
owned by the input driver, fd-resolution kept at the syscall boundary (mirroring `'d`→`drm`).
**B2-kd also LANDED:** the KD accept/reject serve moved to `Tty::kd_ioctl` (the console device in
`vfs/tty.rs`), with fd→tty→vc resolution and the DRM `card_grabbed_by_current()` check kept at the
syscall boundary and `owns_card` handed *down* as an argument. That is what dissolves the earlier
"gated behind V-4" worry I recorded here: because the card answer comes in as a parameter,
`vfs::tty` never names `drivers`, so no forbidden `vfs→drivers` edge is created, and the KD decode
leaves the switchboard (V-2) exactly as evdev's did. `request_arg` (its only user) went with it.
Verified **guest `pass=136 fail=0`** with `t-ioctl`/`t-seat`/`t-tty-eintr`/`t-tty-vtime`/`t-tty-query`
all green; an intermediate `pass=134 fail=2` was the `t-netlat`/`t-times` timing-flake pair (both
passed on the re-run — load flakes, not the move). The `'V'` family (`vt_ioctl`) still sits in `fs.rs`:
it too reads live console state in `vfs/vcmux`, and would follow the same hand-off shape; left as-is.
The lesson stands: a behaviour-preserving relocation can only be trusted once the gate is green on the
*current* tree, since a layout shift unmasks unrelated latent bugs.

**Acceptance:**
- `crate::drivers::` and `crate::net::` no longer appear in `src/vfs/mod.rs` (the six `as_*`
  methods are gone).
- `crate::syscall::` no longer appears anywhere under `src/drivers/` or `src/net/`.
- `wc -l src/syscall/linux/fs.rs` drops materially from 6 311.
- Every host test still passes, and `cargo xtask test` still boots with every `/bin/t-*`
  reporting `=== PASS` — with `t-drm`, `t-drm-args`, `t-fbdev`, `t-evdev`, `t-ioctl`, `t-seat`,
  `t-prime` and `t-unix*` as the programs that specifically exercise the moved paths.
**Risk:** this touches every device path at once. Do it one device at a time (fbdev first — the
smallest contract; DRM last — the largest), keeping that device's conformance program as the
gate, and never batch two devices into one commit.

---

#### Step C — Let subsystems own their per-open-file resources (V-3) — **DONE**

`close_at_exit` ×4 in the scheduler becomes a **registry**: the scheduler keeps a list of hooks
that each subsystem registers at boot, and on task exit it walks the list, naming nothing but its
own trait. A subsystem that registers nothing costs nothing.

For the exec path, decide per edge rather than abstracting for symmetry: `binfmt::shebang` and
`vfs::read_file` are genuinely needed to load an image, so either accept the `vfs` edge and
document it as allowed ("the scheduler loads images from a filesystem" is not a leak in the way
that "the scheduler closes sockets" is), or hold a narrow `ImageSource` trait. The rule to apply:
*is this the scheduler's business, or somebody else's?* — per-open-file resource ownership is
somebody else's; reading an executable is the scheduler's.

**Acceptance:** `sched/runtime.rs` contains no `crate::net::`, no `crate::drivers::` and no
`crate::vfs::(inotify|locks)` reference; a task that held a socket, an inotify watch, an evdev
grab and a uinput device is still cleaned up correctly (`t-inotify`, `t-evdev`, `t-filelock`,
`t-unix-scm` are the gates); `wc -l src/sched/runtime.rs` drops from 6 397.
**Risk:** teardown order matters — a descriptor's resource must go before the table entry. Pin
the order with a host test on the registry itself, then keep the guest tests as the end-to-end
gate.

**Done, and it took the first mutual pair out of the graph.** The registry is
`src/sched/exit.rs`: an append-only list of `fn(&[Arc<File>], owner: u32)` hooks, walked from
`runtime.rs`'s exit path before the batch is parked for dropping (the reference counts still say
which description is the last one until then, which is what `vfs::file::for_each_last` reads).
Registration happens in `main.rs`, in the order the resources have to go, because the order the
list is walked in *is* the registration order: locks first (a lock is keyed by `(device, inode)`
and the inode is reached *through* a descriptor), then the state keyed by the open file
description — the socket, the inotify watch, the evdev reader position, and the uinput device a
program made for itself. `main.rs` is the wiring, so it is allowed to know all five; the
scheduler knows none of them.

Each subsystem's hook now takes the same two things the scheduler can offer. Two of them had to
say so in their signature rather than being wrapped — `net::socket::close_sockets_at_exit` was
`pub(crate)`, which a binary crate cannot see, and is `pub` now — and `vfs::locks` gained
`release_on_process_exit`, which is the loop that used to sit in the teardown: *which of these
descriptors names an inode this process still holds a lock on* is a locking question, and
`vfs::mount::fs_id_of_inode` is a vfs one.

**Measured, and the number that matters.** `sched → net` is **gone** (1 → 0), which is what
removed the `net ↔ sched` pair: `net → sched` is 114 references of ordinary parking and stays,
but a one-way edge is not a cycle. Mutual pairs 20 → **19**, and the ratchet constant moved down
with it — a reduction is a commit that lowers the baseline, which is what makes the ratchet read
as progress rather than as a fence. `sched → vfs` also fell 26 → 22 (locks, mount and inotify),
and `sched → drivers` 4 → 2.

**The acceptance is met on the exit path and not on the exec path, deliberately.** The two
remaining `crate::drivers::` references in `runtime.rs` are on `execve`'s `FD_CLOEXEC` pass,
which tells the input drivers to forget a reader position keyed by an address the new image will
reuse. That is a *different event* with a *different* pair of subsystems to tell, and Step C's
own text says the exec path should decide per edge rather than be abstracted for symmetry — so
the `drivers ↔ sched` pair stands, named in `depgraph.rs` beside the rule, rather than being
made to disappear behind a general mechanism that would run the socket, watch and lock hooks on
an event that is not a process ending. The three `crate::vfs::(inotify|locks)` references the
acceptance names are gone.

**Pinned by the registry's own host test** (`hooks_run_in_registration_order_and_an_empty_list_is_not_a_special_case`):
two hooks that record what they were handed, asserted to run in registration order with both the
batch and the owner, plus the empty list being a no-op rather than a case the teardown has to ask
about. It is one test rather than two on purpose — the registry and the recorder are both
process-global, and the first draft's second test failed for exactly that reason. The end-to-end
gate stayed the guest's: `t-inotify`, `t-evdev`, `t-filelock` and `t-unix-scm` all report
`=== PASS`, and the whole sweep after these Phase-11 changes is `=== SUMMARY pass=140 fail=0
(140 built)` with QEMU exit `33`.

---

#### Step D — Move the terminal emulator out of the VFS (V-4) — **CLOSED** (emulator + VC vocabulary + `vtmode` + the active-console number all in the `term` leaf; the `vfs ↔ console` pair broken by D.2a; D.2b evaluated and rejected)

Create `src/term/` (grid, keys, font, escape parsing) and `src/console/vcmux`, leaving in `vfs`
only what is a filesystem fact: `/dev/tty*` and `/dev/pts/*` nodes, the line discipline as the
node's behaviour, and `termios`. `console` then depends on `term`, and `vfs` depends on neither
`console` nor the render task.

**Acceptance:** `vfs/` contains no font asset, no escape-sequence parser and no `vcmux`;
`crate::vfs::term` is gone; `console.rs` no longer reaches into `vfs` for the grid; the three
`vfs/term` test modules keep passing after the move (they are pure, so the move is mechanical);
`cargo xtask run --mode mirror` still paints the same screen and `Ctrl-A 1..6` still switches
virtual consoles.
**Risk:** `vfs/term/vga8x16.bin` is `include_bytes!`-ed — a missed path is a compile error, which
is the good outcome.

**Half of it has landed, and the half that landed is the half that was a crate.** Step E's pattern
applied to the emulator: `src/vfs/term.rs` and `src/vfs/term/` (blit, cursor, font, keymap and the
4 KiB `vga8x16.bin`) are `crates/term` — `mpos-term`, a leaf that cannot name the kernel — and
`src/lib.rs` re-exports it as `crate::term`. That is the plan's `src/term/` with the enforcement
added, and it is the acceptance criterion in the form it can be checked in: `crate::vfs::term` no
longer exists anywhere, `vfs/` holds no font asset and no escape-sequence parser, and the three
`vfs/term` test modules moved with the code and run in the new crate (32 pins, unchanged).

The one place this differs from the plan's own sketch is *who moves*: the plan said `console`
"then depends on `term`", and the extraction made that literal — `console.rs` (3 references),
`drivers/video/render.rs` (3) and `drivers/virtio/input.rs` (2) now name `crate::term`, which is
what takes 3 references off `console → vfs` (4 → 1) and 5 off the largest single edge in the tree
(`drivers → vfs`, measured 71 by the ratchet afterwards). `vfs::tty` reaches for the same crate,
which is right: the line discipline and the grid must agree about the cursor (§7.1b's whole
finding is that they must not each keep one).

**What remains is the mux, and it is not mechanical.** `vcmux.rs` looks like the same shape and is
not: it reaches *out* to `console::write_bytes`/`vc_output`/`grids_live`, to
`sched::{spawn_permanent, block_until_deadline, wake, send_signal_process}` and to
`drivers::{video::render, drm::master_held}`. Moving the file to `console/` would exchange a
`vfs → console` edge for a `console → vfs` one and leave the pair where it was; what had to move
first is the shared vocabulary — the console count and the KD mode words are console facts that
`pty`/`tty`/`vtmode` happened to define.

**D.1 — the shared vocabulary moved. LANDED (this pass, green).** `VC_COUNT` (was
`vfs::pty::VC_COUNT`) and the `KD_TEXT`/`KD_GRAPHICS`/`K_RAW`/`K_XLATE`/`K_MEDIUMRAW`/`K_UNICODE`/
`K_OFF` mode words (were `vfs::tty::*`) now live in `crates/term/src/vc.rs` as `crate::term::vc::*`.
`term` is a leaf that `vfs`, `console` and the drivers already depend on, so naming it is downward
for all of them — this is the prerequisite the paragraph above named, and it is what lets the next
sub-commit move the `vcmux` *file* without re-creating the pair. A pure relocation: `drivers/video`,
`drivers/devices` and `drivers/virtio/input` stop naming `vfs::pty`/`vfs::tty` for these constants;
`vfs::{tty,pty,vcmux}` name `crate::term::vc` instead. Verified behaviour-preserving — bare-metal
build green, host suite unchanged at `528 passed`, `cargo xtask depgraph` reports "no edge appeared
and no count has grown" (`crate::term` stays out of the mutual-pair set, and `vfs/mod.rs →
drivers|net` stays 0), and the full gate ends `=== SUMMARY pass=140 fail=0`, exit **33**, with
`t-seat`/`t-tty-eintr`/`t-tty-vtime`/`t-tty-query`/`t-ioctl`/`t-drm-lease` (the programs that read
the KD/KD-mode state through the moved path) all `=== PASS`.

**D.1b — the handover table moved too. LANDED (this pass, green).** `vfs::vtmode` (the pure
`VT_AUTO`/`VT_PROCESS` release/decision table — `VtMode`, `Decision`, `Ack`, `Promise`, `Signal`,
`Release`, `Table`, `RELEASE_TIMEOUT_TICKS`) is a self-contained state machine: it named no kernel
module at all (only `alloc::vec`), so like the `evdev`/`uinput`/`term` extractions before it it is a
leaf lifted out so a host build can argue with it. It now lives at `crates/term/src/vtmode.rs` as
`crate::term::vtmode` — the same leaf that holds the VC vocabulary, since `term`'s own header claims
"which of the six virtual consoles is active" as its business. `vfs::vcmux` and `vfs::tty` name it
there; `vfs/mod.rs` no longer declares it. Host coverage preserved: the 17 `vtmode` tests moved from
the kernel's `-p mpos` run (528 → 511) into `-p mpos-term` (32 → 49), and `cargo xtask depgraph` still
reports no edge appeared and no count grew (`vfs/mod.rs → drivers|net` stays 0; `term` is out of the
mutual-pair set).

**D.2a — the pair is broken. LANDED (this pass, green), and the premise corrected.** The plan (and
the paragraph above) assumed the pair stood on the mux's `vfs → console`/`vfs → drivers` *calls*. The
measurement says otherwise: a mutual pair collapses the moment *either* direction hits zero
(`xtask/src/depgraph.rs:104-110`), and `console → vfs` was a **single** reference — `src/console.rs`'s
`active_grid()` reading `crate::vfs::vcmux::active()` for "which console is on screen." So the cheap
inversion was not the mux's outbound calls at all; it was that one inbound read. The active-console
number — a lone `AtomicU32` — moved from `vfs::vcmux` to the `term` leaf (`crate::term::vc::active`/
`set_active`, which already declares "which of the six virtual consoles is active" its business, like
D.1/D.1b before it). `console.rs` and the drivers now read `crate::term::vc::active`; the mux writes
it there and reads it there too. `console → vfs` = 0, the pair collapsed to a one-way `vfs → console`,
and **mutual pairs fell 18 → 17** — the first number Phase 11 has lowered since Step C, ratcheted by
`BASELINE_MUTUAL_PAIRS = 17`. Behaviour-preserving: bare-metal build green, host suites unchanged
(`511` kernel + `49` term + `47` xtask), `cargo xtask depgraph` reports `console ↔ vfs` gone and no
new edge, and the full gate ends `=== SUMMARY pass=140 fail=0`, exit 33, with `t-seat`/`t-tty-*`/
`t-drm`/`t-fbdev` (the programs that read and switch the active console through the moved state) all
PASS.

**D.2b — evaluated and REJECTED. Step D CLOSED at its acceptance criterion.** The remaining item was
relocating the `vcmux.rs` *file* out of `vfs` (into `console`), inverting its outbound `vfs → console`
(`write_bytes`/`vc_output`/`grids_live`) and `vfs → drivers` (`video::render::notify`/`drm::master_held`)
calls in the hand-down/`Waker` shape Step B2 established. Measuring it this pass showed no sound,
valuable landing, so it is not done:

- **The move would regress the layering.** `console` is a designed leaf — `AGENTS.md` §3 lets it name
  only `arch`, `boot` and `term`. `vcmux` names `vfs`(`pty`/`tty`), `drivers`(`drm`/`video`) and
  `sched`; relocating it to `console` turns `console` into a mid-layer that depends on all three, the
  opposite of V-4. A bare move first *re-creates* `console → vfs` from the mux's 7 `pty`/`tty` calls
  (`lookup`/`pairs`/`master_ready`/`take_master` and `tty::global().receive_byte`), undoing D.2a.
- **A mux-only inversion yields zero ratchet gain.** Current `cargo xtask depgraph` (measured):
  `console ↔ vfs` is **already gone**; `vfs → console` is 8 references but only 5 are in `vcmux` — the
  other 3 are `tty.rs` (2) and `ext4` (1), so the mux cannot zero the edge; and `vfs → drivers` is 17,
  of which only 7 are `vcmux` (the other 10 are `procfs`/`sysfs`/`p9` device-state reads). So the
  surviving `drivers ↔ vfs` mutual pair is not `vcmux`'s to break — it is the procfs/sysfs/9p door, a
  different finding (§11.1).
- **The cost is real:** the change threads boot-registered hooks through the seat/DRM/render pump — the
  subsystem whose "nearly-right" regressions (untagged devices, a dropped pointer) this repo has
  repeatedly paid for — for no measurable improvement.

Per §4 ("refuse rather than approximate", "high cohesion — locate the module that owns the concept"),
Step D is **closed**: the dependency cycle it targeted (`vfs ↔ console`) is broken, the terminal
emulator, VC vocabulary, `vtmode` and the active-console number all live in the `term` leaf, and the
`vcmux` file stays in `vfs` where its pty/tty orchestration coherently belongs. The residual `drivers ↔
vfs` pair is recorded against its own finding, not against Step D.

---

#### Step E — Split `fs.rs` along its real seams (follow-up to B2) — **LANDED**

After B2 the file should be much smaller. What remains is still two jobs: the path-carrying calls
(`open`, `stat`, `getdents64`, …) and the device-ioctl entry point. Split into
`syscall/linux/file.rs` and `syscall/linux/dev.rs`, or leave it once B2 has removed the
switchboard — whichever the size says.

**Acceptance:** the dispatch routing counts are unchanged, no handler changed meaning, host tests
unchanged.

**Landed.** The size said split (B2 shrank the ioctl arms but the file was still 6833 lines). It went
to a directory module `src/syscall/linux/fs/` rather than two top-level files, because that is the
shape that keeps `syscall/linux/mod.rs`'s ~95 `fs::<handler>` references byte-for-byte identical — the
acceptance criterion in the form it can be checked: `fs/mod.rs` holds the module header, the one helper
both halves call (`sync_nonblocking`), and re-exports the entry points (`pub(super) use file::*;
pub(super) use ioctl::*;`), so the dispatch still names `fs::handle_read`, `fs::handle_ioctl`, … with no
change upstream. `fs/file.rs` (5523 lines) is the path-carrying handlers and their private helpers
(`Xattr`, `base_of`, `record_lock`/`lock_take` with both cfg bodies moved together, the `flock_off`
submodule); `fs/ioctl.rs` (1281 lines) is `handle_ioctl` and the contiguous per-family routers
(`family_nr`/`vt_ioctl`/`kd_ioctl`/`fs_flags_ioctl`/`uinput_ioctl`/`evdev_ioctl`/`ifreq_index`/
`ifreq_flags`). The measurement showed the seam is unusually clean: the entire device-ioctl footprint is
one contiguous region, and exactly one helper crosses it. `fs` is machine-only
(`#[cfg(target_os = "none")] pub mod fs`), so the kernel host build never compiles it — which is why
this is verified by the bare-metal build and the QEMU gate rather than the host suite. Pure relocation:
`cargo build --target x86_64-unknown-none` green with clippy back to its 29-warning baseline (no new
lint), `cargo xtask depgraph` unchanged (mutual pairs 17/17 — `fs` lives inside `syscall`, so no module
edge moves), host suite unchanged at 511, and the full gate ends `=== SUMMARY pass=140 fail=0`, exit
**33** — the path/file and every ioctl-family conformance program (`t-ioctl`, `t-drm`, `t-fbdev`,
`t-tty-*`, `t-seat`, `t-hwclock`, …) pass on the relocated handlers.


---

#### Step F — Make the target gate a rule, not a habit (V-11 + the `ARCHITECTURE.md` §8 drift) — **DONE**

*The one step in this phase that is pure rule-making: it changes no behaviour and removes no
cycle, and it exists because the gate's failures are silent.*

1. Settle the `net` case: either declare `pub mod net` for the host too — which means moving the
   pure parts of `net/socket.rs` out from under the gate, since `smoltcp` is gated in
   `Cargo.toml` — or delete the unreachable test module and say so. Pick one; do not leave the
   annotation lying.
2. Write the criterion down (`AGENTS.md` §3 and a header comment in `src/lib.rs`): *a module is
   host-compiled when its logic touches no hardware, no port I/O, no virtio ring and no real
   clock — and then its dependencies must be host-compiled too, transitively.* The transitive
   half is the part that gets forgotten.
3. Add a cheap mechanical check: a host test in `xtask` that walks `src/**/*.rs` and fails when a
   `#[cfg(test)]` module sits in a file whose nearest declaring parent is gated
   `target_os = "none"`. Crude, but it catches the V-11 class without a compiler change.

**Acceptance:** the check fails on today's `net/socket.rs` and passes after the fix;
`cargo test -p mpos-xtask` covers it.

**Done, and the three answers the step asked for.**

*Which way `net` went, and why it is the cheap way rather than the lazy one.* The plan offered
"host-compile it, or delete the module and say so" as if they were equivalents; they are not, and
the tie-breaker was coverage rather than effort. Making `net` host-compilable means a device seam
for `drivers::virtio::net::VirtioNet` and a clock seam for `arch::timer` (10 machine references
across `net/mod.rs`, `net/socket.rs` and `net/vsock.rs`, plus `smoltcp` moved out from under the
`target_os = "none"` dependency) — real Phase-11 B work, not Step F work. The three tests, read
instead of assumed, turned out to be `t-sockmsg`'s subject: `user/tests/src/bin/t-sockmsg.rs`
pins `send`/`recv` both ways, the `sendmsg`/`recvmsg` gather and its "no address, no control
data, no flags" answer, `MSG_DONTWAIT` on an empty pair as `EAGAIN`, and a destination on a
connected pair as `EISCONN` — the same four claims, in a program that boots. So the module is
deleted and §11.1.10 records where its coverage lives; a comment at the end of `net/socket.rs`
says the same to the next reader, and names the check that keeps a fourth from being written.

*The criterion is written down in one place and named from the others*: `src/lib.rs`'s header
carries it (with the transitive half spelled out and the two ways it fails silently), `AGENTS.md`
§3 points at it and keeps its "when you gate a module, check what gates its tests" instruction,
and `ARCHITECTURE.md` §8 states it and records V-11's closure.

*The check is `xtask/src/gates.rs`, and it is pinned against itself.* `dead_host_tests` walks
`src/lib.rs`'s `mod` declarations, resolves each to `dir/name.rs` or `dir/name/mod.rs` the way
the compiler does, and carries the gate down — so a file under an ungated-but-gated-child module
is caught as well as one declared gated directly. It is run at the head of `cargo xtask test`,
so the failure is a named error rather than a test count nobody reads, and its four unit tests
include the counterfactual this kind of checker needs: a two-file tree with the *same*
`#[cfg(all(test, not(target_os = "none")))]` in one gated and one ungated file, where the first
must be reported and the second must not. Two more pin the gate arithmetic — the ungated sibling,
and the boot handoff's own `#[cfg(all(any(target_os = "none", test), target_arch = "x86_64"))]`,
which names the host by way of `test` and so must *not* read as machine-only.

*Verified, in the counterfactual order the plan asked for:* with the check in place and the module
still present, `cargo test -p mpos-xtask` failed on exactly one violation — `src/net/socket.rs`,
gated by `crate::net::socket`; after the deletion the same command is `42 passed`, the bare-metal
build is green with the same single pre-existing warning, and the host kernel suite is unchanged.

---

#### Step G — Keep the graph from regressing — **DONE**

Once A–F have landed, freeze the result. Add to `xtask` a `depgraph` subcommand (or a host test)
that re-runs §11.0's extraction and asserts:

- no edge from a lower layer into `syscall` (`drivers`, `net`, `blk`, `pci`, `rand`, `uevent`,
  `vfs`, `sched` must not reference `crate::syscall::`);
- no `crate::drivers::` or `crate::net::` reference from `vfs/mod.rs`;
- the measured cycle count is at or below the recorded baseline.

The baseline is what makes it useful: a **ratchet**, not a cliff. Store it as a constant next to
the check with a comment naming the date and the number, so a reduction is a commit somebody can
make and a regression is a failing test.

**Acceptance:** `cargo xtask depgraph` reports the current edge and cycle counts and fails when
either worsens.

**Done — and one correction to the method, which is the interesting part.** The ratchet is
`xtask/src/depgraph.rs`, reachable as `cargo xtask depgraph` and also run as a host test, so it is
inside `cargo xtask test` rather than beside it. It measures the tree the way §11.0's script does
— every file under `src`, each mapped to its top-level module, every `crate::<mod>` reference an
edge — with **one deliberate difference: `//` comments are stripped before matching.** That was
not a tidy-up; it is what makes the number mean "who names whom". §11.0 matches text, which is why
the audit it published counted three edges that are prose: `uaccess → syscall` is a doc comment
(and `vfs → uaccess` with it), and the third pair `arch ↔ vfs` came from a file the compiler never
saw at all (§11.1.7's deleted `arch/x86_64/serial.rs`). Measured both ways on this tree: **23**
mutual pairs by text, **20** in code. The ratchet holds the code number, because a rule that fires
on a sentence is a rule people learn to word around.

**The rules are ratchets rather than absolute refusals, and that is a statement about where in
the plan this landed.** Steps A, E and F are done and B/C are not, so two of the three rules are
*not yet* zero, and a check born red is one nobody reads. Each constant names the call sites that
keep it non-zero and the step that removes them:

| Rule | Measured 2026-10-02 | What kept it there | Removed by |
|---|---|---|---|
| lower layer → `syscall` | `drivers` 1, `sched` 10, the rest 0 | `drivers/virtio/sound.rs`'s one `use crate::syscall::linux::abi` (V-5's remainder); `sched/runtime.rs`'s `crate::syscall::linux::{abi, nr}`, `MMAP_BASE` and `DEFAULT_UMASK` | B2 (the ioctl ABI), B/C (the mapping base and the umask are scheduler state) |
| `vfs/mod.rs` → devices | 9 | the nine `Inode::as_*` downcasts (V-1) | **Done** (B1): 0 |
| mutual pairs | 20 | the whole `vfs ↔ sched ↔ syscall ↔ net ↔ drivers` cluster | B, C, D |

`arch` and `main` are deliberately **not** in the first rule: the `syscall` gate exists to enter
the dispatcher and `main` is the wiring, so both edges are the design. A layer with a baseline of
zero — `net`, `blk`, `pci`, `rand`, `uevent`, `vfs` — fails on its *first* reference, which is
where the ratchet earns its name.

Four unit tests pin the instrument itself, because a measurement that cannot tell two shapes apart
agrees with whatever it is pointed at (§12.1 A8's lesson, in a different currency): a reference in
a doc comment is not an edge; a cycle of two is reported once and a one-way edge is not a pair; a
service naming `syscall` with no baseline fails; and the `vfs/mod.rs` rule is silent at its
recorded number and fails one past it. The check has never been red on this tree, and it has been
shown able to go red on each of the three rules.

---

### 11.3 Order, and what is deliberately not in this plan

**Do this, in this order:**

1. ~~**V-12** — finish or revert the in-flight refactor.~~ **Done**: the tree builds and the
   whole gate is green (§11.1.12), so everything below is now verifiable.
2. ~~**Step A** — pure subtraction, ~5 files, one commit, no behaviour change.~~ **Done**
   (§11.2 Step A): the three dead modules, the `[msgtrace]` trace and the aarch64 profile are
   gone, and the pair count it was measured with fell from 22 to 21.
3. ~~**Step B** — the device seam, one device at a time.~~ **Done** (§11.2 Step B): B3 moved the
   user-pointer helpers to the `uaccess` leaf, B2 put every device family's ioctl contract next to
   the state it answers from, and B1 (fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the
   DRM family last) deleted the nine `Inode::as_*` downcasts, taking `vfs/mod.rs → drivers|net`
   from 9 to 0. The pair count did not move with it — that is V-4's and the PCI-config/9p doors'
   share of the cluster, not the downcasts'.
4. ~~**Step C** — the resource registry.~~ **Done** (§11.2 Step C): the exit hooks live in
   `sched::exit`, registered by `main.rs`, and the `net ↔ sched` pair is gone.
5. ~~**Step D** — the terminal emulator move.~~ **Done** (§11.2 Step D): D.1/D.1b/D.2a moved the VC
   vocabulary, the pure `vtmode` table and the active-console number into `crate::term`, collapsing the
   `vfs ↔ console` mutual pair — **mutual pairs 18 → 17**, the first ratchet drop since Step C; gate
   green `pass=140 fail=0`. **D.2b (relocate the `vcmux` file to `console`) was measured and rejected**
   — `console` is a leaf the mux's `vfs`/`drivers`/`sched` naming would violate, and a mux-only
   inversion moves no ratchet (`vfs → console` and `vfs → drivers` each have non-`vcmux` callers). Step
   D's dependency cycle is closed; the surviving `drivers ↔ vfs` pair is tracked as its own finding.
6. ~~**Step E** — the `fs.rs` split, if B2 has not already made it unnecessary.~~ **Done** (§11.2 Step E):
   the file was still 6833 lines, so it split along the path/file ↔ device-ioctl seam into
   `src/syscall/linux/fs/{mod,file,ioctl}.rs`; `mod.rs` re-exports the entry points so `mod.rs`'s
   `fs::<handler>` dispatch is unchanged. Gate green `pass=140 fail=0`, depgraph unchanged (17/17).
7. ~~**Step F** — the gate criterion and the check for it.~~ **Done** (§11.2 Step F): the
   criterion is in `src/lib.rs`/`AGENTS.md`, the dead test module is deleted with its coverage
   named, and `xtask/src/gates.rs` fails the sweep on the shape.
8. ~~**Step G** — the ratchet.~~ **Done** (§11.2 Step G): `cargo xtask depgraph` measures the
   graph with comments stripped and holds three recorded baselines; it is also a host test, so
   `cargo xtask test` runs it.

Steps A, E, F and G were mechanical (a day each) and landed; B, C and D needed the device/scheduler/console
paths and have also landed (D's remaining D.2b file-move was measured and rejected as a layering
regression with no ratchet value — §11.2). **Phase 11 is closed except one surviving mutual pair: `drivers
↔ vfs`, which is the `procfs`/`sysfs`/`p9` device-state door (§11.1), not any Step.** Nothing here had to
land before the feature work in Phases 1–10 — but every feature lands *cheaper* after B and C, which is
the argument for having done them first.

**Deliberately not in this plan:**

- **Rewriting the `Inode` interface.** The trait itself is fine; it is the nine device and socket
  methods that hung off it that were wrong. B1 removed them, added one type-erased handle the
  caller names a type through, and stopped there.
- ~~**A generic HAL trait forest.**~~ **Reversed — see §11.4.** What this note rejected was a
  trait per function; §11.4 lands *six* traits for the six hardware concerns the kernel has,
  with the platform tag owned by `hal` so the seam adds no dependency cycle. The reasoning
  against indirection still holds and is honoured: the forwards are static, there is no `dyn`
  and no allocation, and a call monomorphises to the direct one it replaced.
- **A message bus or an async runtime.** The kernel is uniprocessor and synchronous by design,
  and `ARCHITECTURE.md` §7's locking rules exist because that is what makes the design legible.
  This phase is about *who names whom*, not about adding indirection.
- **Splitting files because they are long.** `vfs/ext4/mod.rs` (3 391 lines) stays as it is.
- **Restructuring `src/main.rs` (1 625 lines).** The boot sequence, the milestone checks and the
  panic handler do belong in the binary; if it ever becomes worth splitting, the checks move to
  `src/main/checks.rs`, and that is a tidiness commit rather than an architecture one.
- **Fixing V-10.** Accepted, documented, and cheaper to live with than a shared crate across two
  targets would be.

### 11.4 The HAL: a machine seam under `arch/` — **CLOSED (S1–S5 LANDED)**

Phase 11 is "who names whom"; this is the largest remaining piece of it, deferred above until
now. Goal: everything above the machine layer compiles against *capability traits* instead of
`arch::x86_64::*`, so a second architecture is `arch/<arch>/` and nothing else (§9.1/§9.2), and
no architecture-specific type crosses the boundary without a compile error.

**Shape** (`src/hal/mod.rs`): six traits for the six hardware concerns — `Clock`, `WallClock`,
`Cpu` (landed), then `Interrupts`, `Paging`, `Contexts` with associated types — plus the platform
tag `Current` and static one-line forwards. The tag is `hal`'s own type and `arch/<arch>/`
implements the traits *for* it, so the seam is a one-way `arch → hal` arrow and adds no cycle:
`cargo xtask depgraph` measured **17 → 15** mutual pairs on S1 (`ARCHITECTURE.md` §6).

**What it does not abstract, and must not.** The Linux ABI *is* the register layout:
`user_regs_struct`, the signal frame, the `syscall`/`iretq` gate are x86_64 by definition (on
aarch64 the same rows are `x0..x30`/`pc`/`pstate` behind `svc`). That stays in `arch/<arch>/`;
wrapping it would be the private dialect `AGENTS.md` §3 rule 3 forbids. "No leakage" means a
port writes `arch/<arch>/` and touches no shared file — not that a register frame becomes
neutral.

**Stages**, each its own commit and green alone (AGENTS §3 rule 7):

| Stage | Contents | Status |
|---|---|---|
| S1 | `hal` + `Clock`/`WallClock`/`Cpu`/`Current`/forwards; the `arch::timer`/`rtc`/`idle`/`halt` spread rerouted | **landed** — pairs 17 → 15 |
| S2 | `Interrupts` (`Line`, `unmask`/`mask`/`eoi`, `init_message_signalled`, `net_msi_vector`): `serial`, `virtio/net`, `lapic` | **landed** — pairs 15 → 14; gate `pass=142 fail=0` exit 33 |
| S3 | `Paging` (`Space` assoc + `AddressSpaceOps`, `install`/`map_mmio`/`translate`/`user_accessible`/`is_cow_page`/`write_user_u32`/roots/switch/`phys_for_virt_of`); `PageTaken` moves to `mm::vm`; migrates `sched`, `mm`, `uaccess`, `syscall`, `main` | **landed** — pairs 14 → 14 (a swap: `arch ↔ mm` out, `hal ↔ mm` in); gate `pass=142 fail=0` exit 33 |
| S4 | `Contexts`: `Context`/`Fpu`/`UserFrame` as associated types behind `ContextOps`/`FpuOps`/`UserFrameOps`; `sched` and the ABI layer stop naming `arch` | **landed** — pairs 14 → 12 (`arch ↔ sched`, `arch ↔ syscall` gone); gate `pass=142 fail=0` exit 33 |
| S5 | close the door: the depgraph rule is an allow-list (`arch`/`hal`/`main`/`lib`, everything else at 0), and `arch/x86_64`'s submodules are `pub(crate)` behind a four-item facade | **landed** |

S2–S4 touch the trap, page-fault and context-switch paths, so each needs the full QEMU gate,
not host+clippy alone. S1 was pure re-routing — host tests, both clippy targets and the depgraph
were the whole measurement, and its own reds (none) would have been a compile error, not a
machine bug.

**S4's design finding, found by measuring rather than by planning it.** The plan above said the
register-frame knowledge would move into `arch/<arch>`. Measuring what is left showed why only
half of it can: after S1–S3 exactly two modules still name `crate::arch::` — `sched` (17, the
saved `Context`, `FpuState`, `UserContext` and the switch/FPU/bootstrap calls) and
`syscall/linux/proc.rs` (4: `gdt::user_selectors` twice and the `UserContext` literal).

The frame's **constructor** can move down — building a frame from the gate's trap record is a
pure machine fact, and moving it is what removes the `SegmentSelector` from the ABI layer. But
the frame's conversion **into** the Linux `user_regs_struct` array
(`abi::uregs`, `context_regs`/`context_from_regs`/`capture_frame`) cannot: `arch` naming
`crate::syscall::linux::abi` is an `arch → syscall` edge, the one direction this whole design
forbids and the one the depgraph's first rule exists to catch. So the array conversion stays in
the ABI layer, and the frame type must instead expose the operations that layer needs
(`to_regs`/`from_regs` with the array as an associated `Regs` type, `ip`/`sp`/`set_result`,
`forked`). That also means `ContextOps` has to hand out its `Fpu` (`fpu()`/`fpu_mut()`, with
`type Context: ContextOps<Fpu = Self::Fpu>` tying the two associated types together), because
`sched` stores `Context` and pokes its FPU half on every switch.

S4's accepted shape, then: `hal::Contexts` with `Context`/`Fpu`/`UserFrame` as associated types
behind `ContextOps`/`FpuOps`/`UserFrameOps`, the aliases and `FPU_BYTES` as crate-level consts in
`hal` (an associated const in an array length needs `generic_const_exprs`), the frame
*constructor* moved into `arch`, and the ABI array conversion left where the ABI is. That
takes both remaining counts to 0 and is what lets S5 close the modules.

---

## Phase 12: ABI truthfulness — **AUDIT DONE, NOTHING FIXED**

Phase 11 is about *who names whom*: the shape of the tree, with no user-visible defect. This
phase is the other half of the review question every change here gets asked (`AGENTS.md` §6):
*is that Linux's answer, or a shape that looks like it?* A gap that is honestly refused
(`ENOSYS`/`EOPNOTSUPP`/`EINVAL`) is already Linux's answer for a kernel that cannot do the call,
so it is not a defect and takes no work here. What this phase collects is the cases that are
**not** honestly refused — a call or an `ioctl` that is *accepted*, answers success, and then
does less than Linux did. Those are the ones `AGENTS.md` §3 calls the single worst thing this
codebase can do: a program comes to believe something false and stops asking.

The audit method mirrors §11.0 — reproducible, and worth re-running: grep every syscall and
device-`ioctl` arm that returns `0`/`ESUCCESS` and read what it actually did; compare against
the Linux behaviour, which for each entry below is either measured here or marked with its
confidence. Three of the entries (A1, B1, B2) were re-confirmed against the source in the
working tree; the rest are as found by the sweep and each carries the same bar — confirm on a
real Linux before landing a change that could send a program down a fallback it would not
otherwise take.

Two hard truths the audit surfaced, before any individual line:

- **The board is not currently green.** `AGENTS.md` §3 records the working tree's in-flight tty
  read-path rewrite as red (`t-tty-vtime` hangs, `t-tty-eintr` fails two checks, `t-netlat` is a
  load flake). That is a *newer* state than §11.1.12's "green", which described the last
  committed baseline (Step A). **Nothing in this phase is verifiable until the gate is green
  again**, so restoring it is task zero, ahead of B and C.
- **Some of these are recorded as `Done` where they are not.** `NOTES.md`'s own table lists
  `getpriority` as done; it returns a hardcoded 20 with no round-trip (B5). The board being true
  is a standing rule (§7 of `AGENTS.md`), so correcting it is part of this phase, not a
  side effect.

### 12.0 The dependency direction this phase assumes

No new dependency design: the target layering is already recorded in §11.2 and mirrored in
`ARCHITECTURE.md` §9, and this phase works *within* it, not against it. The two rules that
matter for the fixes below are the ones §11.2 states: **nothing below `syscall` names
`syscall`**, and **nothing outside `vfs` names the VFS's concrete device types**. The device-
`ioctl` truthfulness fixes (A2, A3) are the same moves §11 Step B2 makes — a request number's
"accept or refuse" decision belongs with the driver that owns the device, not in the
`syscall/linux/fs.rs` switchboard — so wherever a fix here and a Phase-11 step touch the same
code, land them together rather than editing the switchboard twice.

### 12.1 Group A — silent success on a question Linux refuses

Purely narrowing changes with a clear Linux answer; each is "refuse honestly", the cheapest and
lowest-risk kind, and the most direct violation of §4. Land with a host test and a probe.

| # | Where | Linux's answer | What this kernel does | Fix |
|---|---|---|---|---|
| A1 | `abi.rs` `handle_prctl` catch-all `_ => 0` | unknown option `EINVAL`; `PR_GET_NAME` returns `comm` | **LANDED.** Name pair round-trips `comm`; catch-all now `_ => EINVAL` (Linux's `prctl` `default:`), the old blanket `0` removed. Backing individual unmodelled options with real state is optional future work. |
| A2 | `fs.rs:4362` `kd_ioctl` catch-all `_ => 0` | read-family fills the buffer; unknown `EINVAL`/`ENOTTY` | **LANDED as an honest closed-set refusal (the §4.2 fake removed); per-device serve remains B2.** `KDGKBTYPE` (`0x33`) now writes the constant `KB_101` Linux answers (`fs.rs`, pinned by `t-ioctl`): before this it fell through to the accept and returned `0` with the caller's byte uninitialised — a false-success, not a hardware gap, because its answer is a context-free constant Linux gives whatever is plugged in. **`Enumerating the `K` family (`/usr/include/linux/kd.h`) showed the `_ => 0` accept still caught the READS and the SETTERS alike**: `KDGETLED` (0x31), `KDGKBMETA` (0x62), `KDGKBENT` (0x46), `KDGKBSENT` (0x48), `KDGKBDIACR`/`KDGKBDIACRUC` (0x4A/0xFA), `GIO_*` font/cmap/scattermap — each returning `0` with the caller's `int`/`struct kbentry`/`struct kbdiacrs` left as stack garbage — plus the `KDSETLED`/`KDSKBLED`/`PIO_*`/speaker writes claiming an effect on hardware with no 8042 controller / a compiled-in keymap / a DRM-owned screen. The old catch-all was the single worst §4.2 shape the objective forbids (success-for-nothing, and success-with-garbage for the reads). This pass **replaces `_ => 0` with a closed set** (Linux's own rule): a number inside the `K` command block (`0x2F..=0x71 | 0xFA..=0xFB`) is a real console ioctl this device declines → `EOPNOTSUPP`; a number outside it is not a console ioctl → `ENOTTY`. This is a *refusal*, not a fabricated value — it sidesteps the "guess the byte width / answer EINVAL on a table Linux serves" trap the earlier pass named, because it returns no data at all. | Confirmed safe: no guest program or the boot sequence calls any kd setter/getter except the five handled explicitly (`KDSETMODE`/`KDGETMODE`/`KDSKBMODE`/`KDGKBMODE`/`KDGKBTYPE`), and the full 136-program gate stays green with `t-seat`/`t-drm-lease`/`t-ioctl` all `=== PASS` — so the prior "a seat manager treats a failure here as the console being unusable" premise was, like A3's, unmeasured and wrong (systemd-logind toggles LEDs best-effort and ignores the error). The *structural* move — serving these per-device from the console's live LED/keymap/font state — stays Phase 11 B2 (V-2 ioctl-to-driver) with the measured Linux values recorded in §12.1 A2 so B2 implements rather than guesses. **Residual after this pass: none faking success** — every remaining `K` command is honestly refused; B2 upgrades the refusals to real per-device answers. Pinned by `t-ioctl` (KDGETLED/KDSETLED → `EOPNOTSUPP`, `0x4B80` out-of-block → `ENOTTY`), all counterfactual against the old blanket `0`. |
| A3 | `fs.rs` evdev write requests | `EVIOCREVOKE` revokes access; FF/keycode answered per-device | was: `EVIOCSFF`/`EVIOCSREP`/`EVIOCREVOKE`/`KEYCODE` all return 0 doing nothing — **`EVIOCREVOKE` silently did not revoke** (a security semantic). Now: **`EVIOCREVOKE` implemented for real**; **FF + keycode refused with Linux's measured errno**; **repeat stored + reported per-device** | **`EVIOCREVOKE` LANDED + gated.** `EVIOCREVOKE` **corrected to Linux's actual contract this pass** (authoritative `evdev.c`: `case EVIOCREVOKE: if (p) return -EINVAL; else return evdev_revoke(...)`): the arg must be **NULL** (any non-NULL → `EINVAL`) and the revoke is **one-way** (no restore ioctl; the caller closes/reopens). The prior arm read `arg` as a *value* (`1`=revoke, `0`=**restore**, `>1`→`EINVAL`) - the wrong shape: a correct `ioctl(fd, EVIOCREVOKE, NULL)` (arg `0`) hit the restore branch and did the *opposite* of revoke, and MPOS exposed a reversibility Linux never had. Now `buf != 0` → `EINVAL`, `buf == 0` → revoke; the entry check still lets a revoked fd call it again (idempotent `0`). Per-ofd revoke state still lives device-side (`crates/evdev`/`EvdevNode`), `ENODEV` on read + every other ioctl; `set_revoked(false)` remains a device-internal/host-test capability, not an ioctl. Pinned by host `a_revoked_client_reads_nothing_until_it_is_revoked_back` (device abstraction) and `t-evdev` re-pinned to `EVIOCREVOKE(NULL)`→revoke→read/ioctl `ENODEV`, non-NULL→`EINVAL`, idempotent `0`, stays revoked (no reverse). **FF + KEYCODE LANDED + gated (measured).** The earlier "a refusal could stop `weston`/`libinput`" premise was *unmeasured and wrong*: probing real Linux (`/dev/input/event0` non-FF and `event3` keyboard) shows Linux itself answers `ENOSYS` to `EVIOCSFF`/`EVIOCRMFF` (guarded on `test_bit(EV_FF)`, which no evdev here sets and this kernel has no effect store for) and `EINVAL` to `EVIOCSKEYCODE`/`_V2`/`EVIOCGKEYCODE`/`_V2` on a node with no per-device keycodes array (the console keymap is compiled in, not an input-device array) — so refusing with those errnos *is* Linux's answer for this device shape, and a program that runs on Linux already handles it. `fs.rs` now returns `ENOSYS`/`EINVAL` where it used to return `0` (a §4.2 false success); `EVIOCGEFFECTS` stays `give(&0)` because Linux returns the count `0` even on a no-FF device (measured). **REPEAT LANDED + gated (measured).** `EvdevNode` now carries a `rep_delay`/`rep_period` pair (defaulted to Linux's own `250`/`33`); `EVIOCGREP` reports the device's real `(delay, period)` when it advertises `EV_REP` and `ENOSYS` otherwise, `EVIOCSREP` stores the two ints (buffer validated → `EFAULT` on a bad pointer, as Linux's `get_user` answers) so a set round-trips through a get. The old `give(&[])`/`ENOTTY` left the caller's ints untouched - success for nothing stored. Pinned by `t-evdev`: FF→ENOSYS, keycode set/get→EINVAL, `EV_FF` bitmap empty, and the `EVIOCSREP(600,50)`→`EVIOCGREP`=(600,50) round-trip (or `ENOSYS` where the device lacks `EV_REP`) — all counterfactual, gated `t-evdev === PASS` twice. **Pressed-state reads LANDED + gated (measured).** `EVIOCGKEY` (0x18), `EVIOCGLED` (0x19), `EVIOCGSND` (0x1a), `EVIOCGSW` (0x1b) now write `min(len, bitmap_bytes)` zero bytes (this kernel tracks no pressed state) and **return that count** - Linux's measured contract: KEY bitmap 96, LED/SND/SW 8, identical on a mouse and a keyboard, so it is the global `<TYPE>_MAX` `BITS_TO_LONGS(MAX+1)*8` size, not the device's used codes; a short buffer is filled short. `EVIOCGMTSLOTS` (0x0a) → `EINVAL` (measured: a node with no multi-touch; none here is a multitouch device). All replaced the old `give(&[])` (bare `0`, caller buffer untouched - success for nothing written). Pinned by `t-evdev` (KEY=96 + overwrite-the-sentinel + short-buffer=8 + SW=8 + MTSLOTS=EINVAL, all counterfactual). The evdev `'E'` family is now honest end to end (reads, state, keycode, ff, repeat, grab, revoke, unknown→ENOTTY); the *structural* relocation of the decode into the device file (V-2 "syscall ioctl-switchboard") is the Phase 11 B2 architecture move. **`EVIOCREVOKE` arg-shape verification RESOLVED** (same session): the open question above was settled against authoritative kernel source (`git.kernel.org` plain `evdev.c`) - Linux demands a NULL arg and the revoke is one-way - and MPOS's value-arg/reversible model was indeed a wrong-shape divergence, now corrected and `t-evdev` re-pinned. The host could not measure it directly (session devices grabbed, `/dev/uinput` needs root), so the kernel source - not memory - decided it, per §4.1. |
| A4 | `fs.rs` `handle_signalfd` | `SFD_CLOEXEC` sets `FD_CLOEXEC`; unknown flags `EINVAL` | only `SFD_NONBLOCK` recognised; `SFD_CLOEXEC` dropped, unknown flags accepted silently | honour `SFD_CLOEXEC`, `EINVAL` on the unknown bits. |
| A5 | `proc.rs:2158` `mmap` flags | `MAP_FIXED_NOREPLACE` → `EEXIST` on collision; `MAP_HUGETLB` with no huge pages → `ENOMEM` | unknown/reserved flags ignored; `MAP_FIXED_NOREPLACE` (`0x100000`) treated as a plain hint and re-located on collision — breaks the "this address or fail" contract; `MAP_HUGETLB` silently gives a 4K page | validate the bits; `NOREPLACE` must fail with `EEXIST`; `HUGETLB` must be `ENOMEM` here. |
| A6 | `abi.rs` `rseq` registration | registers per-thread `rseq` state; well-formed registration → `0` | (was) returned `0` for a valid call, wrote none of the `cpu_id`/`node_id`/`mm_cid` fields, no unregister | **LANDED as `ENOSYS`.** The dispatch arm is removed, so `rseq` falls through the canonical `_ => ENOSYS` (§4.2). The old arm was a §4.2 success-for-a-thing-not-done: it answered `0` while writing no field the ABI contract requires. Measured (Linux 7.2 host: `AT_RSEQ_FEATURE_SIZE` 33 / `AT_RSEQ_ALIGN` 64) that glibc registers at length 33, which the old `len != 32` arm `EINVAL`'d anyway — so the accept honoured neither real glibc nor the contract; and the libc this machine runs (musl) never calls `rseq`. `ENOSYS` is the honest "not available" a probing libc falls back from. Pinned by `t-rseq` (re-pinned to `ENOSYS`; the prior pin had encoded the false-success, so per rule 1 the test changed with the measurement). *Closing it for real* = a per-thread registration that writes `cpu_id`/`node_id`/`mm_cid`, maintains them across preemption, and honours `RSEQ_FLAG_UNREGISTER` — a sched feature, named below. |
| A7 | `proc.rs` `PTRACE_SETSIGINFO` → `0` | stores the supplied `siginfo_t` so the tracee receives it verbatim (`si_code`/`si_addr` for a planted `SIGSEGV`) | accepted and discarded for any `pid`: `PTRACE_GETSIGINFO` synthesises a siginfo from the stop, and the tracee's delivered signal is the one the *resume* names, so the old arm read nothing and answered `0` - even for a live-but-untraced pid where Linux answers `ESRCH` | **LANDED as an honest refusal + validation + gated.** Closing it *for real* needs `siginfo_t` carried through the stop/resume (a sched+ptrace feature, its own commit) - the kernel's stop model carries signal+event, not a queued siginfo. So this pass removes the §4.2 fake rather than guessing the queue: the arm now runs Linux's own error precedence - `stop_of(id)` → `ESRCH` for a not-a-stopped-tracee, `buffer_ok(data, 128)` → `EFAULT` for an unbacked struct (Linux copies it first), then `EOPNOTSUPP` for the carry it cannot do. Pinned by `t-ptrace-attach`: `PTRACE_SETSIGINFO` on a live but untraced child is `ESRCH`, **not a silent `0`** (counterfactual - the old `=> 0` answered success for it). Gated `t-ptrace-attach === PASS`. |
| A8 | `fs.rs` `RNDZAPENTCNT`/`RNDCLEARPOOL` (+ `RNDADDENTROPY`) → `0` | gate the mutating entropy ioctls on `CAP_SYS_ADMIN` (`EPERM` otherwise) and, for a privileged clear, do the thing or say so | returned `0` for the write requests while `RNDGETENTCNT` stayed pinned at a fixed 4096 (this kernel's random source is an always-seeded ChaCha20 stream, not a drainable accumulator) | **LANDED + gated (measured).** Probed real Linux on `/dev/urandom`: unprivileged `RNDZAPENTCNT`/`RNDCLEARPOOL`/`RNDADDENTROPY` all answer **`EPERM`** (gated on `CAP_SYS_ADMIN`), and the check precedes the buffer copy. `fs.rs` now (1) returns `EPERM` before touching the buffer when `euid != 0`, matching Linux exactly for the unprivileged path; (2) for a privileged `RNDADDENTROPY`/`RNDADDTOENTCNT` keeps `0` - honest, because the pool is already full so a credit is a genuine no-op the observable "entropy available" state does not contradict; (3) for a privileged `RNDZAPENTCNT`/`RNDCLEARPOOL` returns **`EOPNOTSUPP`** - there is no drainable count to zero, so §4.4 refuses rather than the old `0` that claimed a clearing `RNDGETENTCNT` (still full) contradicted one moment later. Pinned by `t-ioctl`: root `RNDADDENTROPY`→`0` (unchanged), root `RNDZAPENTCNT`→`EOPNOTSUPP`, and a `setresuid`-dropped `euid` making `RNDADDENTROPY`/`RNDZAPENTCNT`→`EPERM` then restored — all counterfactual (the old handler returned `0` regardless). `RNDGETENTCNT`→4096 and `FIONREAD`→512 pins unchanged. Gated `t-ioctl === PASS`. **But the shape this row landed was wrong, and its wrongness is a boot failure:** the family was dispatched on the command byte, and the arms named the wrong numbers - `RNDADDENTROPY` is `_IOW('R', 0x03, int [2])` = `0x40085203` (nr 3), which this row's arm answered as the drain, and `t-ioctl`'s "RNDADDENTROPY" constant was `0x40085202`, which is `RNDGETPOOL`. The whole-word contract, the measurements behind it and the seedrng evidence are in §12.1's A8-correction bullet; the row above stands as what was measured *then*, with the `CAP_SYS_ADMIN` gate and the `EOPNOTSUPP` refusals still true. |

**Landed this pass (each with its counterfactual pin, AGENTS.md checklist rule 4):**

- **A4 signalfd** — `handle_signalfd` now honours `SFD_CLOEXEC` (sets `FD_CLOEXEC`) and answers
  `EINVAL` on any bit outside `{SFD_NONBLOCK, SFD_CLOEXEC}`. Pinned in `t-signalfd` (the unknown-flag
  `EINVAL` and the `F_GETFD` close-on-exec readback both fail without the change). `t-signalfd` `===
  PASS`.
- **A5 mmap** — `handle_mmap_impl` now treats `MAP_FIXED_NOREPLACE` as "this address or fail"
  (`EEXIST` on an occupied range instead of relocating), refuses `MAP_FIXED | MAP_FIXED_NOREPLACE`
  with `EINVAL`, and refuses `MAP_HUGETLB` with `ENOMEM` instead of a silent 4 KiB page. It does
  **not** add a strict unknown-flag whitelist, because Linux accepts `MAP_STACK`/`MAP_NORESERVE`/
  `MAP_POPULATE` as no-ops and musl's thread stacks rely on that — a blunt "reject everything else"
  would itself be a non-Linux answer. Pinned in `t-mmap` (NOREPLACE exact-address success, `EEXIST`
  on collision, conflict `EINVAL`, `HUGETLB` `ENOMEM`). All four `t-mmap*` programs `=== PASS`.
- **vt family sweep (`vt_ioctl`, `'V'`) — `VT_SENDSIG` fixed, `ENOTTY` (measured).** Probed on a current console: `VT_SENDSIG` (0x04) → `ENOTTY` (the command was removed from the vt driver, so it falls through the kernel switch), while MPOS's arm returned `0` for a command that does nothing anywhere — a §4.2 false success on dead input. Now `0x04 => err::ENOTTY`; pinned by `t-seat` (counterfactual). `VT_DISALLOCATE` measured `0` on a free console (matches MPOS); the `EBUSY`-for-the-in-use-console case is an untested edge and left. `VT_RESIZE`/`VT_RESIZEX` measured `EPERM` for a non-foreground caller, but the owner case (where Linux resizes) could not be tested from here (the probe was not tty1's foreground job) and MPOS has a deliberate "the compositor owns the window size, `TIOCGWINSZ` carries it" design — so these are NOT flipped blind (§4.1); the honest non-owner `EPERM` is a candidate follow-up once the owner/handover behavior is pinned, recorded rather than guessed.

- **Flag-word audit (the A4 `signalfd` discipline, applied kernel-wide).** Swept every flag-bearing syscall for a silently-ignored or unvalidated flag bit (§4.2 — a flag the kernel accepted and dropped is a promise not kept). `dup3`, `pipe2`, `epoll_create1`, `timerfd_create`, `inotify_init1`, `renameat2`, `openat2`, `preadv2`/`pwritev2`, `splice`, `msync`, `clone`, `signalfd` all already refuse unknown bits with `flags & !(KNOWN) != 0 → EINVAL`. **One exception: `eventfd2`** dropped `EFD_CLOEXEC` (never applied close-on-exec — the exact leak A4 closed for `SFD_CLOEXEC`) and accepted any unknown bit. Fixed `handle_eventfd`: refuse an unknown bit `EINVAL` before creating, honour `EFD_CLOEXEC` via `fd_flags_set(FD_CLOEXEC)`, keep `EFD_SEMAPHORE → EOPNOTSUPP` (the whole-count-take refusal, unchanged). Pinned by `t-eventfd` (unknown bit `0x40` → `EINVAL`; `eventfd(0, EFD_CLOEXEC)` → `F_GETFD` shows `FD_CLOEXEC`) — both counterfactual (the old code returned a plain fd, no `EINVAL`, no CLOEXEC). Gated `t-eventfd === PASS`. **A second exception surfaced later: `unlinkat`** routed only on `AT_REMOVEDIR` and silently accepted any other flag bit, so `unlinkat(fd, path, 0x1000)` removed the name and answered `0` where Linux gives `EINVAL` *and leaves the file* (measured) - the wrong shape of answer, since a caller that expected a refusal got a deletion. Fixed `unlinkat` to reject `flags & !AT_REMOVEDIR != 0` with `EINVAL` first, the way Linux words it. Pinned by `t-unlinkat` (unknown flag `0x1000` → `EINVAL`, and the file survives the refused unlink) — counterfactual (the old code deleted and returned 0). Gated `t-unlinkat === PASS`.

- **`madvise` advice-vocabulary audit (the same discipline, applied to a small-integer selector rather than a flag word).** `handle_madvise` acted on `MADV_DONTNEED` and returned `0` for **every** other `advice`, including numbers Linux itself rejects with `EINVAL` — the §4.2 accept-and-drop in its clearest form (the handler's own doc claimed unrecognised numbers were "refused", but the code did not). Measured on the current host (`madvise` over a mapped anonymous page, advice `0..=110`): Linux accepts exactly `{0,1,2,3,4,8,10..24,102,103}` and gives `EINVAL` for the retired slots `5,6,7`, for `9` (`MADV_REMOVE`, invalid on an anonymous range), for `100/101` (`HWPOISON`/`SOFT_OFFLINE`, need privilege) and for everything past the block. Added the host-testable `abi::madvise_advice_is_known(advice)` gate and wired it before the handler does any work; `MADV_DONTNEED` keeps its page-drop, the recognised hints still answer `0` (truthful — best-effort with no observable durable promise here), unrecognised numbers now `EINVAL`. Pinned by host `madvise_advice_gate_matches_linux_known_set` and `t-madvise` (advice `5` → `EINVAL`, `0xffff` → `EINVAL`, `MADV_WILLNEED` `3` → `0`) — the two `EINVAL` lines counterfactual against the old silent `0`. Gated `t-madvise === PASS` (the DONTNEED page-drop and shared-preserve pins still green alongside).

- **`posix_fadvise` (`fadvise64`) advice-vocabulary audit — the same closed-set discipline.** `handle_fadvise64` looked up only that the descriptor was open and returned `0`, ignoring `advice` entirely and accepting it on a pipe — the `nr.rs` comment even read "every advice is accepted and ignored, which is what Linux does for the advice it does not act on", which is true of the *recognised* hints and false of the rest. Measured on the current host (glibc `posix_fadvise`, x86_64 `fadvise64(fd, offset, len, advice)`, answered `0`/`-errno` directly): advice `NORMAL(0)`..`NOREUSE(5)` → `0`; `6` and `99` → `EINVAL`; an unopened number → `EBADF`; a pipe → `ESPIPE`. Fixed the handler to `clone_at` (→ `EBADF`), then gate the advice on `0..=5` (`EINVAL` past it), then refuse a pipe with `ESPIPE`; a recognised hint still answers `0` (truthful, this kernel prefetches nothing). Corrected the `nr.rs` comment to name the closed vocabulary. Pinned by a new `t-fadvise` (advice `0`/`4`/`5` → `0`, `6`/`99` → `EINVAL`, bad fd → `EBADF`, pipe → `ESPIPE`) — the `EINVAL` and `ESPIPE` lines counterfactual against the old silent `0`. Gated `t-fadvise === PASS`; the guest gate is now **136 built**.

- **`clock_gettime`/`clock_nanosleep` clock-id vocabulary audit — the closed-set discipline on the clock selector.** `handle_clock_gettime` returned wall-clock time for **any** `clk_id` outside its monotonic list (a fall-through, not a refusal), so `clock_gettime(999, &ts)` answered `0` with a plausible number for a clock this kernel never named; `handle_clock_nanosleep` discarded `clk_id` (`_clk_id`) and slept on the default regardless, answering `0` for a request to sleep on a nonexistent clock. Measured on the host: `clock_gettime` accepts exactly `{0..=9, 11}` and gives `EINVAL` for the `10` gap and every id from `12` up (`42`, `999`); `clock_nanosleep(999, …)` is likewise `EINVAL`, while an unrecognised *flag* bit past `TIMER_ABSTIME` is ignored by Linux too — so only the clock id, not the flags, needed a gate. Both handlers now reject an id outside the set with `EINVAL` before any other work (in `gettime`, before the `tp` check, matching Linux's `EINVAL`-before-`EFAULT`). `clock_nanosleep`'s `TIMER_ABSTIME` → `EINVAL` is left as-is: an absolute sleep is a *feature* this kernel declines, not a success it fakes, and a refusal with a real errno is the honest direction. Pinned by `t-clock-gettime` (`clock_gettime(10)`/`(999)` → `EINVAL`, `(11 / TAI)` → `0`, `clock_nanosleep(999)` → `EINVAL`, `(CLOCK_MONOTONIC)` → `0`) — the two `EINVAL` lines counterfactual against the old silent `0`. Gated `t-clock-gettime === PASS`.

- **`memfd_create` flag-vocabulary gate.** `handle_memfd_create` validated only the name and looked just at `MFD_CLOEXEC`, creating the descriptor for **any** flag word — an unknown bit got an fd, and `MFD_HUGETLB` got an ordinary (non-huge) memfd the caller believed was huge-backed: two §4.2 success-for-a-thing-not-done answers. Measured on the host: `memfd_create` accepts `{MFD_CLOEXEC, MFD_ALLOW_SEALING, MFD_HUGETLB}` plus the `MFD_HUGE_*` size bits (only with `MFD_HUGETLB`) and rejects an unknown bit (`0x1000`) and size-bits-without-`MFD_HUGETLB` with `EINVAL`. Fixed the handler to gate `flags & !KNOWN != 0` (or size-bits without `MFD_HUGETLB`) → `EINVAL`, and to refuse `MFD_HUGETLB` with `EOPNOTSUPP` - this kernel has no hugepage backing, so the honest answer is a refusal the caller can fall back from, not a fake ordinary fd. Pinned by `t-memfd-shm` (unknown `0x1000` → `EINVAL`, `MFD_HUGETLB` → `EOPNOTSUPP`, `MFD_ALLOW_SEALING` alone still hands back an fd) — the `EINVAL`/`EOPNOTSUPP` lines counterfactual. Gated `t-memfd-shm === PASS`.

- **`getrandom` flag-word gate.** `handle_getrandom` discarded `flags` entirely (`_flags`) and filled the buffer for any flag word — an unknown `GRND_*` bit got random bytes and a success where Linux answers `EINVAL` (§4.2 accept-and-drop). The three valid flags are genuinely satisfied by this kernel's one always-ready, non-blocking generator (`GRND_NONBLOCK`/`GRND_RANDOM`/`GRND_AUTOCONF` — measured accepted on the host, with `0x100` rejected), so they stay accepted; an unknown bit now `EINVAL`, checked before the `len == 0` short-circuit (measured: a bad flag with `len == 0` still errors). Pinned by the host `getrandom` test (extended: `0x100` → `EINVAL` at `len 8` and at `len 0`, `GRND_AUTOCONF` accepted) — the `EINVAL` lines counterfactual against the old discard. Gated at baseline (`pass=134 fail=2`; boot unaffected — getrandom is on the stack-canary path and every program still runs).

- **`statx` flag word: unknown bits refused and `AT_SYMLINK_NOFOLLOW` honoured.** `statx` bound `flags` to `_flags` — it honoured `AT_EMPTY_PATH` but not the vocabulary or `AT_SYMLINK_NOFOLLOW`. Two faults: an unknown flag bit (e.g. `0x10000`) was accepted (Linux `EINVAL`; measured accepted set exactly `{0x100, 0x800, 0x1000, 0x2000, 0x4000}`), and every statx followed the last symlink, so an `lstat`-shaped statx (which uutils `ls -l`/`stat` reach for) reported the *target's* metadata while claiming success — a wrong-node answer, not merely less-than. The path branch now splits on `AT_SYMLINK_NOFOLLOW` exactly as `fstatat` does (`resolve_no_follow` vs `resolve_opt`), and a `statx_flags_are_known` predicate rejects bits outside the set. The sync bits (`AT_STATX_FORCE_SYNC`/`_DONT_SYNC`) and `AT_NO_AUTOMOUNT` stay accepted: one non-networked filesystem has nothing to sync or automount, so they cannot be violated (a legal no-op, like `mlock`'s). Pinned by host `statx_flag_gate_matches_linux_known_set` and `t-statx` (unknown `0x10000`→`EINVAL`; a symlink to a regular file reports `S_IFLNK` with `NOFOLLOW`, `S_IFREG` without) — the `EINVAL` and `S_IFLNK` lines counterfactual. Gated `t-statx === PASS` (`pass=134 fail=2`).

- **`open` honours `O_NOFOLLOW`.** The bit was not defined anywhere in the tree, so `openat(path, O_NOFOLLOW)` silently dropped it and `open_opt` followed the final-component symlink — the caller's "do not traverse a link here" guarantee (the classic world-writable-`/tmp` defence) was accepted with success and not kept: a §4.2 accept-and-drop with security meaning. Measured on the host: `O_NOFOLLOW = 0o400000`, `open(symlink, O_NOFOLLOW)` → `ELOOP`, `open(regular, O_NOFOLLOW)` succeeds, intermediate components still resolve. `openat` now defines the bit (`abi::O_NOFOLLOW`) and, when set, walks the parent without following (`resolve_no_follow`) and answers `ELOOP` if the final name is a symlink; the check is gated solely on `flags & O_NOFOLLOW`, so every normal open is untouched. Pinned by `t-openat` (a symlink made to the test file: O_NOFOLLOW → `ELOOP`, the same open without it still follows, O_NOFOLLOW on the regular file opens) — the `ELOOP` line counterfactual. Gated `t-openat === PASS` (the one extra run-red was `t-stdresolve`, the documented intermittent DNS flake, with no causal path to an open-flag check normal opens never reach).
  A **dangling** final component is the case that decides whether the no-follow walk is honest —
  `lstat` of `slink -> /does-not-exist` must answer the *link* (`S_IFLNK`), not `ENOENT` for its
  missing target, and `O_NOFOLLOW` on it must be `ELOOP`, not a followed open. Measured on a current
  host (`stat(path, AT_SYMLINK_NOFOLLOW)` and `os.stat(follow_symlinks=False)` both succeed). It
  holds here because `resolve_no_follow` looks the last name up with `Inode::lookup`, which never
  reads a target, and ramfs stores the link node itself — pinned by the host
  `resolving_a_name_without_following_it_still_handles_dot`, now extended with the dangling link so
  the property is asserted rather than assumed.

- **`ftruncate` enforces the descriptor's access mode.** The B1 access-mode enforcement covered `read`/`write`, but `ftruncate` resized any regular-file descriptor regardless of how it was opened — an `O_RDONLY` fd could truncate. Measured on the host: Linux refuses `ftruncate` on a read-only descriptor with `EINVAL` and leaves the size unchanged (note the errno differs from `write`'s `EBADF`). The handler now checks the recorded `O_ACCMODE` and refuses `ACC_RDONLY` with `EINVAL`; a non-path descriptor (pipe/socket/anon) defaults permissive and is caught by the regular-file test. Pinned by `t-truncate` (`ftruncate` on an `O_RDONLY` fd → `EINVAL` with the size unchanged at 13; the same fd `O_WRONLY` → `0`) — the `O_RDONLY` line counterfactual (the old handler resized it). Gated `t-truncate === PASS` (pipe `EOPNOTSUPP` divergence still holds). The same write-mode rule now covers `fallocate`, which resized a regular file through an `O_RDONLY` descriptor: it returns `EBADF` (measured host: `fallocate(O_RDONLY)` is `EBADF`, nothing grows), pinned by `t-fallocate` (`fallocate` on an `O_RDONLY` fd → `EBADF`; the O_RDWR file still allocates; pipe `ENODEV`/closed-`EBADF` preserved).



- **A6 rseq — refused with `ENOSYS`, after all (the earlier "NOT refused" is reversed).**
  A prior pass kept the accept and downgraded the gap to "write `cpu_id` later," reasoning that
  `t-rseq` pinned a `0` and rule 1 protects the test. That reasoning was wrong on two counts, and
  the correction is this pass. First, `0`-with-no-fields-written is the §4.2 violation the batch
  exists to remove, not a scope boundary — `t-rseq` had *encoded the false-success*, and rule 1's
  own carve-out (a test whose expectation is wrong vs Linux changes *with a measurement*) applies.
  Second, a measurement against a current host (Linux 7.2, `AT_RSEQ_FEATURE_SIZE` 33,
  `AT_RSEQ_ALIGN` 64) showed real glibc registers at length **33**, which the old `len != 32` arm
  rejected with `EINVAL` — so the accept served neither the musl binaries this machine actually runs
  (which never call `rseq`) nor a modern glibc's real startup; it satisfied only a synthetic length-32
  probe. The honest answer for a kernel that carries no per-thread registration is `ENOSYS`, the
  documented "rseq unavailable" fallback a probing libc is written to take. The syscall *number* stays
  in `nr.rs` (append-only ABI); only the hollow dispatch arm is gone. Doing `rseq` for real — writing
  and maintaining `cpu_id`/`node_id`/`mm_cid` across preemption, plus `RSEQ_FLAG_UNREGISTER` — is the
  named gap, a sched feature in its own commit.

- **A1 prctl — the name pair landed.** `PR_SET_NAME` (15) and `PR_GET_NAME` (16) are now real, not
  accepted-and-dropped: `set_current_comm` stores the caller's bytes in the group's `comm` slot (`Mm`,
  the same slot `comm_of` and `/proc`'s `comm` read) and `PR_GET_NAME` hands them back, NUL-padded to
  sixteen; with no name set it reports the derived image basename, as Linux does. A null buffer is
  `EFAULT` in either direction.
  - **The truncation half was first written backwards and has been corrected by measurement.** The
    landing note claimed a name with no NUL inside the sixteen-byte `TASK_COMM_LEN` slot is refused
    (`strnlen_user`→`EFAULT`), not truncated. Measured on this host with a buffer whose terminator
    cannot be an accident — sixteen non-NUL bytes with a non-NUL seventeenth — `PR_SET_NAME` answers
    **`0`** and `PR_GET_NAME` reads back **fifteen** characters: Linux copies at most the slot and
    forces its last byte to NUL. So the arm refused a case Linux accepts, which is the §4.2
    wrong-shape answer in the direction that hurts: a program that named itself sixteen bytes long
    would take its "the kernel would not name me" fallback here and not on Linux. The handler now
    truncates. Pinned twice — the host `prctl_stores_and_reports_the_process_name` (accepts, reads
    back fifteen, slot ends in NUL) and `t-prctl`'s new line for the same shape in the booted
    machine, the counterfactual for the `EFAULT`.
  - What Linux *does* refuse is the copy reaching memory the caller does not own: a name placed eight
    bytes from a page whose successor is unmapped answers `EFAULT`, while the same start with its NUL
    before the crossing answers `0` (measured). The byte-at-a-time scan that stops at the terminator
    is exactly that shape, and it re-validates at every page boundary the scan crosses — which is why
    the scan is bounded to the slot and does not read ahead for a terminator it will not use.
  `t-prctl` `=== PASS`. Only the catch-all `_ => 0` half is deferred, below.

**Deferred with its prerequisite named (not attempted blindly):**

- **A1 prctl catch-all — LANDED + gated.** The catch-all `_ => 0` was a §4.2 false success (a `0`
  claiming an unmodelled option was handled). It now returns `EINVAL`, matching Linux's `prctl`
  `default:` arm in `kernel/sys.c`. §4.2 lists `EINVAL` as the sanctioned honest answer for an
  argument the kernel refuses, and §4.4 prefers a refusal to a quiet fake — so blanket-`EINVAL` for
  every option *not backed by real state* is the objective-correct posture ("unknown → `EINVAL`"),
  not a wrong-shape answer. The prior worry that this "needs a measured whitelist so a blunt refuse
  doesn't break a workload" was over-cautious: the gate that boots the whole initramfs runs the real
  programs and stays green (`pass=133 fail=2`, reds the unrelated in-flight net/audio), proving no
  running program depended on the old `0`.   `t-prctl`'s "unknown answers 0" pin was corrected to
  "unknown → `EINVAL`" (it had encoded mpos's false-success, not Linux).
  **Then the four options that had real Linux behaviour behind them were given that behaviour.**
  The prerequisite turned out to be exactly what the earlier note said it was, and it is now built:
  `src/cred.rs` is a per-process credential model - the four capability sets, the dumpable tri-state
  and `KEEPCAPS` - stored on the thread group beside `comm`, with the measured Linux rules inside it
  and *consumers* rather than a stored no-op. `capget` reports the stored sets instead of a constant
  and `capset` stores them (Linux's `cap_capset` asks `CAP_SETPCAP` first and, holding it, accepts
  anything, which is why root's own lowering/raising round-trips); a uid/gid change re-derives the
  sets - the effective one cleared on a drop and restored from permitted on a return to root, the
  permitted one kept while any of the six ids is root's and cleared by a full drop unless `KEEPCAPS`
  (all four measured on 7.2.7); `execve` gives an `euid` of 0 its bounding set back, so a dropped
  bounding capability is the way to take one from a root program for good; and `ptrace_may_access`
  is consulted by `PTRACE_ATTACH` **and** `process_vm_readv`/`writev`, so the dumpable flag now
  decides something. `PR_SET_DUMPABLE`/`PR_GET_DUMPABLE`, `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS` and
  `PR_CAPBSET_READ`/`PR_CAPBSET_DROP` were then moved out of the catch-all and answered with the
  measured contract (only 0/1 settable, 2 and 3 `EINVAL`; read 1/0 for `0..=40` and `EINVAL` past
  it; `DROP` asks the capability *before* the range, so an unprivileged `100` is `EPERM` where
  root's is `EINVAL`). Named divergences, both conservative: no file capabilities are read at
  `exec`, so a capability Linux would grant from a binary here is simply not held; and the dumpable
  reset uses `SUID_DUMP_DISABLE`, the sysctl's default, where the machine these were measured on had
  it set to 2. Pinned by thirteen host tests in `cred.rs`, `t-prctl`'s option table and `t-posix`'s
  `capset`/`capget` round-trip.
- **A2/A3 kd + the remaining evdev writes** — paired with Phase-11 Step B2 (move each device's ioctl
  accept/refuse down to the driver). Editing the `syscall/linux/fs.rs` ioctl switchboard for them now
  would add calls along the V-2 edge the next refactor removes, against checklist rules 5 and 7 (a
  refactor and a behaviour change in one diff). Land them with Step B, one device at a time.
  **`EVIOCREVOKE` is the exception that landed now**: it had one unambiguous Linux shape (a per-ofd
  revoke the driver reads as `ENODEV`), its state belongs device-side in `crates/evdev` where B2 wants
  it anyway (so this is relocation-aligned, not switchboard-widening), and it is the security
  false-success §4 ranks worst — so removing it did not wait for the refactor. The `EVIOCSFF`/`REP`/
  `KEYCODE` writes and the whole `kd` family do need the device's own view and stay with B2.

### 12.2 Group B — accepted, but the full contract is not implemented

These need a real behaviour, not just a refusal, so each carries more risk: a narrowing change
sends the caller down a path it would not otherwise take. Confirm on Linux first; land with a
conformance program.

| # | Where | Linux's answer | What this kernel does | Fix / decision |
|---|---|---|---|---|
| **B1** | `fs.rs:2760`/`2793` `handle_read`/`handle_write` (+ `readv`/`writev`); `opened_for_write` in `vfs/file.rs:195` was read only by `inotify.rs` | writing an `O_RDONLY` fd → `EBADF`; reading a `W`-only fd → `EBADF` | was: access mode enforced at `open` but never afterwards — `open(O_RDONLY)` then `write()` succeeded | **LANDED + gated.** `File` now carries the open's `O_ACCMODE` in a dedicated `access` field (default `RDWR`, so sockets/pipes/anon stay permissive); `File::read`/`File::write` - the single chokepoint every user read/write/readv/writev/pwrite/splice/copy reaches the inode through - refuse a mismatch with `EBADF` (`VfsError::BadFd`), before even a zero-length access. `F_SETFL` cannot reopen a refused direction (separate field). Pinned by a host test (`vfs/file.rs` `access_mode_is_enforced_after_open`) and confirmed by the guest: `t-fill-ramfs` had been reading an `O_WRONLY` fd back (Linux answers that `EBADF` too) - a test expectation that was wrong for Linux, corrected to open `O_RDWR`; it and the rest of the fd-using suite pass. |
| **B2** | IPv4 tails at `socket.rs` `recvfrom`/`recvmsg` (compute `nonblock` from `O_NONBLOCK` only) | `MSG_PEEK` does not consume; `MSG_DONTWAIT` earns `EAGAIN` | **was: `MSG_PEEK` on TCP/UDP consumed the datagram and returned success** (silent data corruption); `MSG_DONTWAIT` ignored | **LANDED + gated.** All four IP recv/send tails now route the wait through `dontwait(fd, flags)` (honours `MSG_DONTWAIT`), and the two recv tails return `EOPNOTSUPP` on `MSG_PEEK` - the same honest refusal `AF_UNIX`/`NETLINK`/`PACKET` already give. Real `MSG_PEEK` over IP remains a named sub-gap. Pinned by the new `t-sock-flags` (peek `EOPNOTSUPP`, `DONTWAIT`→`EAGAIN`; hangs without the fix). `pass=131 fail=2`, no net regression. |
| B3 | `abi.rs` `get_robust_list` `ENOSYS` while `set_robust_list` was accepted | x86_64 has the call and returns the registered head | was: asymmetric — `set` accepted-and-discarded, `get` refused | **LANDED + host-gated.** `set_robust_list` now files the head on the process's `Mm` (a `robust_head` next to the membarrier registration); `get_robust_list` returns it with `sizeof(robust_list_head)` = 24, `ESRCH` for a foreign pid, `EFAULT` on null outputs. Pinned by the host round-trip in `abi.rs` `startup_registrations_check_their_abi_sizes`. The remaining gap is *walking* the list to release held futexes at thread exit (named below), not the registration pair. **Do not add head-pointer validation here**: measured on a current host, `set_robust_list` answers `0` for `NULL`, for a low non-`NULL` value, for an unmapped in-range address and even for a kernel-space address, because the kernel deliberately never touches the head it is handed — it files the pointer, and the dereference belongs to exit, where a bad one is handled as the user's own fault. Storing any value is therefore Linux's answer, not an unchecked input. The one thing Linux does check, `len != sizeof(struct robust_list_head)` → `EINVAL`, this kernel already refuses. |
| B4 | `posix.rs` `vhangup` → constant 0 | hangs up `current->signal->tty`, answers `0`; **answers `ENOTTY` when the caller has no controlling terminal** (`drivers/tty/tty_io.c` `SYSCALL_DEFINE0(vhangup)`) | was: `0` for every caller — a false success for the (common) no-terminal caller, not just the privileged one | **Landed and gated — both cases honest.** `vhangup` queries the controlling terminal: none → `ENOTTY` (Linux's answer, what the self-test programs reach — `t-posix` corrected from `0` to `ENOTTY`, rule 1's measurement exception); present → `EOPNOTSUPP`, because the kernel has no tty-layer hang-up (re-init + `EIO` on open fds) and §4.2 forbids faking the success. A logout/getty does its own fd teardown next regardless, so the honest refusal costs nothing it had. The full Linux behaviour (actually hanging up) remains the open feature; the prior "Linux answers `0` for root, don't refuse" reasoning conflated the two cases — measured here: the no-terminal caller does not get `0`. The `EOPNOTSUPP` branch is reached only with an acquired terminal (login path), which no headless self-test does, so it is gate-unexercised — the gate verifies the change is regression-free, not that branch's value. |
| B5 | `posix.rs` `setpriority` discarded, `getpriority` constant 20 | round-trips the nice value; `PRIO_PROCESS`/`PGRP`/`USER` sets over the resolved set, `20 - min(nice)` get, `ESRCH` on empty/gone, `EINVAL` on range/which | was: no round-trip; board wrongly said `Done` | **LANDED + gated.** `nice` is now a per-task field (mirroring `oom_score_adj`: stored honestly, reported truthfully, no scheduling-weight effect — that stays a named sub-gap), inherited across `fork`. The three `which` modes resolve via `current`/`pgid_of`/`group_members`/`uid_members`. Board corrected (the row split back to `Done`, leaving `vhangup` as the only `Partial`). Pinned by `t-priority` (round-trip −5→25 / +3→17, group set seen by process get, range `EINVAL`, gone pid `ESRCH`, bad which `EINVAL`). |
| B5b | `posix.rs` `sched_setattr` dropped the nice, `sched_getattr` reported a hardcoded 0 | the modern door onto the same per-task nice: `sched_setattr` writes it, `sched_getattr` reads it, both `SCHED_OTHER`-only, nice −20..19, `ESRCH`/`EINVAL`/`EOPNOTSUPP` on the shapes Linux names | was: `sched_setattr` returned `0` while discarding `sched_nice`; `sched_getattr` wrote size/policy and hardcoded nice 0 | **LANDED + gated (same pass as B5's slot).** These are the extensible-attributes door onto the *very* per-task `nice` B5 made real — `sched_setattr`'s `SCHED_OTHER` arm now calls `set_nice(target, nice)` (the value round-trips through `getpriority`), and `sched_getattr` reports `nice_of(target)`, not a constant. Hardcoding 0 in `getattr` was the identical wrong-as-a-lie `getpriority` had before B5 (§4.2). The `attr`+16 `sched_nice` field is the only one this policy carries; deadline/rt policies 1..6 stay `EOPNOTSUPP`, unknown policy or out-of-range nice `EINVAL`, flags ≠ 0 `EINVAL`, `pid` naming no task `ESRCH`, struct `< 48` bytes `EINVAL`. Pinned by `t-priority` (cross-door: `setpriority(-5)`→`sched_getattr` reads −5; `sched_setattr(-7)`→`getpriority` 27 and `sched_getattr` −7; nice 25 and 40-byte struct `EINVAL`) — every line counterfactual against the drop/hardcode. No new board `Partial`: this row was inside §1.3's scheduling `Done` lump, corrected by naming the gap here. |
| B5c | `posix.rs` `sched_setscheduler` accepted a nonzero `SCHED_OTHER` priority and returned `0` | the `SCHED_OTHER` arm validates the passed `struct sched_param` priority the way `sched_setparam` does: zero → the no-op success, nonzero → `EINVAL`, null/unbacked → `EFAULT` | was: `SCHED_OTHER => 0` ignored `args[2]` entirely | **LANDED + gated.** `sched_setparam` already refused a nonzero priority; the `setscheduler` door took the same `sched_param` and did not, so `sched_setscheduler(0, SCHED_OTHER, &{prio=5})` answered success where Linux answers `EINVAL`. Measured on a real host (gcc probe of glibc `sched_setscheduler`): `prio=5`→`EINVAL`, `prio=0`→`0`, matching this kernel's `setparam`. The `SCHED_OTHER` arm now mirrors `setparam`'s `buffer_ok`→`EFAULT` and priority→`EINVAL` checks before its success; the refused RT policies keep validating the param before `EOPNOTSUPP`. Pinned by `t-posix` (`setscheduler(SCHED_OTHER, prio 0)`→0, `prio 5`→EINVAL, `setparam(0)`→0, `setparam(7)`→EINVAL, then `getparam` still 0 after the refused sets) — the `prio 5`/`prio 7` lines are counterfactual (the old `SCHED_OTHER` arm returned 0 for both). Inside §1.3's scheduling `Done` lump. |
| B6 | `proc.rs` `times()`/`getrusage()` `tms`/rusage | advances with CPU time, split user/system, with reaped-child totals | **LANDED + gated.** The timer interrupt charges the running process's `Mm` user or system by the ring it interrupted (`frame.code_segment == user CS`); `times()` reports `tms_utime`/`tms_stime`/`tms_cutime`/`tms_cstime` in `USER_HZ` ticks (the 250 Hz internal clock scaled `×100/TICK_HZ`, matching the `CLK_TCK` musl hardcodes) and `getrusage(RUSAGE_SELF)`/`(RUSAGE_CHILDREN)` report the same as real `timeval`s. A reaped child's CPU folds into the parent's child totals at `reap()` (`absorb_child`: parent's `cutime += child.utime + child.cutime`), Linux's accumulate-on-`wait`. Pinned by `t-times`: a user spin raises `utime` not `stime` (the split fails if attribution is wrong), monotonic, sane `tv_usec`, and fork→burn→wait makes `RUSAGE_CHILDREN`/`tms_cutime` grow. The fourteen byte/page counters stay zero (not measured — an honest gap, not an invented number). |

### 12.3 Group C — rationale comments that no longer describe the code

Cosmetic to a caller, but they mislead the next reviewer, so fix in the same commits as the
behaviour they describe:

**Landed (all three corrected):**

- `mod.rs` `sigaction` doc said "a signal handler is never called" — corrected to say a caught
  signal builds an `rt_sigframe` and runs the handler, with the one honest caveat kept: `SA_RESTART`
  is still recorded-but-not-honoured (§2.5).
- `fs.rs` `unlinkat` doc said `AT_REMOVEDIR` was "ignored" — corrected to say it selects `rmdir`
  vs `unlink`, which the body already does.
- The `getpriority`/`setpriority` and `vhangup` "Done" lump in §1.3 — split out of the `Done` row
  into a `Partial` row that names B4/B5 as the open gaps (the behaviour itself is still a
  silent-accept; only the board claim was fixed here).

### 12.4 Order

1. **Restore the green gate.** Fix `t-tty-eintr`'s two test-side expectation bugs — `AGENTS.md`
   §3 already states the kernel's answers (`EINTR`, 3 bytes) are the measured Linux ones and the
   test compares against `-lin::EINTR` (`+4`) and expects two bytes for a three-byte `"hi\n"` —
   and reconcile `t-tty-vtime` with the in-flight `VMIN`/`VTIME`/`EINTR` read-path rewrite. Treat
   a lone red `t-netlat` as load. **This blocks everything below.**
2. **Group A** — the honest-refusal batch. Each is small, host-testable, and purely narrowing.
   Pair A2/A3 with Phase-11 Step B2 where they touch the same device-`ioctl` code.
3. **Group B** — the contract-completion batch, one behaviour at a time, each pinned by a `t-*`
   program and gated on the full run. B1 first (largest blast radius, most important), then B2.
4. **Group C** — fold the comment fixes into the commit that changes the behaviour they describe.

### 12.4a Root cause of the step-1 block — `t-tty-vtime` (diagnosed, fix designed)

Measured on the current tree: the guest suite reaches 115 verdicts and stops inside
`t-tty-vtime`, right after `ok: VMIN=2/VTIME=5 is accepted` — it never reaches a `=== SUMMARY`.
`t-tty-eintr` now PASSes (the two expectation bugs `AGENTS.md` §3 recorded are already corrected
in the tree: it compares against `lin::EINTR` (`-4`) and expects 3 bytes for `"hi\n"`). So the
single remaining red is this one hang, and it is a *wiring* bug, not a policy bug: `read_wait`
(`tty.rs:1163`) returns the correct `VMIN>0`/`VTIME` decision and its host tests pass.

The bug is an agreement mismatch between `read_blocking` and `block_until_deadline`:

- `block_until_deadline(cond, deadline)` (`runtime.rs:2868`) is a **condition loop**: it returns
  only when `cond()` is true, a signal is deliverable, or `timer::ticks() >= deadline`; on any
  other wake its schedule closure re-parks it.
- For `VMIN>0` with an empty queue, `read_wait` returns `UntilInput`, which `read_blocking`
  maps to `deadline = u64::MAX` with `cond = has_ready()` (tty.rs:1741,1755). `has_ready()` in
  non-canonical mode needs `out.len() >= VMIN`, so one partial byte leaves it false.
- `receive_byte` (tty.rs:1550-1556) *does* wake the reader for a partial byte
  (`byte_restarts_clock()` → clone the waiters → `sched::wake(id)`), but `block_until_deadline`
  wakes, re-tests `has_ready()` (still false), sees `deadline == MAX`, and **re-parks** — so the
  clock-restart wake is swallowed and the read never re-derives the real `UntilTick(last_byte +
  span)` deadline. It sleeps forever.

The `tty.rs` comment "a wake from anything else is simply another look" (read_blocking:1757)
assumes every wake surfaces to the caller; `block_until_deadline` does not honour that.

**Fix (additive, no change to the shared scheduler primitive):** give `LineDiscipline` an
`input_count: u64` bumped in `note_input_tick`, and for a pass whose wait is clock-restarting
(`byte_restarts_clock()`), snapshot `base = input_count` before parking and pass the predicate
`has_ready() || input_count != base`. A byte arrival then changes `input_count`, ends the park,
and `read_blocking` re-derives `UntilTick(last_byte + span)` — which arms the real deadline and
delivers the partial bytes one `VTIME` after the **last** byte, matching the measurement. This
cannot busy-spin (the predicate only flips on a real arrival) and leaves the canonical, `VTIME==0`
and `VMIN==0` parks on their current `has_ready()` predicate untouched.

**Landed and verified.** The fix is in `tty.rs` (an `input_count: u64` on `LineDiscipline`
bumped in `note_input_tick`, and a `byte_restarts_clock()` read parks on
`has_ready() || input_moved(base)`). After it, `cargo xtask test` runs to completion:
`t-tty-vtime`, `t-tty-eintr` and `t-tty-query` all report `=== PASS`, the run reaches
`=== SUMMARY pass=130 fail=2`, and QEMU exits 33. The two remaining reds are **not** tty and
**not** Phase 12 — `t-dsp` and `t-dhcp` are other in-flight new programs: `t-dsp` fails wholesale
because the self-test machine boots with `-audiodev none` (no `/dev/dsp`), and `t-dhcp` is the
not-yet-wired DHCP boot path (§4.5 board). `t-netlat` passed this run. So the green-gate work
*in Phase 12's scope* is done; the gate reaches a summary, and `fail={t-dsp, t-dhcp}` is the
known non-Phase-12 baseline the Group-A regression check compares against.

**Both of those diagnoses were wrong, and both reds are now closed.** The `-audiodev none`
explanation is the mistake worth writing down: it predicted a machine with no sound card, and a
machine with no card is a legitimate thing to be red about. Measured against the very QEMU this
gate boots, `-audiodev none,id=snd0 -device virtio-sound-pci` puts the card on the bus regardless
(`info pci` reports `1af4:1059`) — that backend silences the *audio*, never the device. The real
cause was this kernel searching for the wrong PCI ID, so the honest statement is "the driver never
found a card that was there": a bug, not a machine's absence. See §12.4a.

### 12.4a The last two reds closed: `/dev/dsp` and the DHCP answer — **DONE, gate green `pass=136 fail=0`**

The pair `fail={t-dsp, t-dhcp}` was the gate's standing red baseline since it was first quoted.
Neither red was a missing pin: each program *was* the pin, and each was pinning a real defect —
one in the kernel, one in the test's own construction. `cargo xtask test` now reaches
**`=== SUMMARY pass=136 fail=0 (136 built)`** with QEMU exit 33.

**`t-dsp` — two kernel bugs, found in that order, the second hidden by the first.**

1. **The sound card's PCI ID was wrong.** `DEVICE_SOUND` read `0x1055`. Measured two ways against
   the machine this kernel boots on: `/usr/include/linux/virtio_ids.h` gives `VIRTIO_ID_SOUND = 25`,
   and the modern virtio PCI ID is `0x1040 + that` = `0x1059`; QEMU's own `info pci` for
   `-device virtio-sound-pci,audiodev=snd0` reports `Audio controller: PCI device 1af4:1059`. At
   `0x1055` the lookup never matched, `init` took its `Probe::Absent` branch — which prints
   nothing, correctly, because a machine with no card is a quieter boot — and `/dev/dsp` was never
   registered. The whole program then cascaded: `open` `ENOENT`, every ioctl on the closed
   descriptor `EBADF`, `chmod` `ENOENT`.
   **The trap this exposes is worth generalising**: an ID looked up on the *absent* branch is
   invisible by construction — the only trace of a wrong ID is the device that never appears. The
   driver behind it was complete and correct the whole time, which is why §7.2b could truthfully
   say the family was implemented while the node it serves did not exist. Audited the other five
   virtio IDs the same way against `info pci`: `rng 0x1005`, `console 0x1003`, `net 0x1000`,
   `9p 0x1009`, `gpu 0x1050`, `input 0x1052` are all exactly what QEMU exposes. Left as they are.
2. **The first real write to `/dev/dsp` panicked the kernel.** With the card found, the boot reached
   `virtio-sound: 44100 Hz, 2 channels at /dev/dsp` and `t-dsp`'s frame-buffer write died on
   `virt_to_phys(0x3fa25c60): not an address from the direct map`, with the syscall ring showing
   `write(fd 3, 0x3fa25c60)` — a *user* address. Cause: `handle_write` builds its `&[u8]` straight
   over the caller's buffer, `DspDev::write` passes it to `VirtioSnd::play`, and
   `drivers::virtio::hal` maps the DMA buffer with `mm::virt_to_phys`, which is only defined on the
   direct map. Every other consumer of that slice only *reads* the bytes, so nothing else had ever
   needed the address to be the kernel's — sound was the first driver to hand one to the device.
   Fixed by copying into a per-device one-period staging buffer allocated at probe from the kernel
   heap, so the address handed to the crate is always the kernel's. That is also Linux's shape:
   ALSA's dsp write path `copy_from_user`s into the DMA ring instead of mapping the caller's pages.
   The copy is one period (4 KiB) at a time, so its size is a constant of this driver and not a
   number the caller steers (§4 rule 4). A refused piece stays a short write, so the
   `CharDevice` byte-count contract is unchanged.
   Pinned by `t-dsp`'s `a frame buffer writes through` (512 bytes, the counterfactual: before the
   staging buffer the same write ends the machine) and by the rest of the program, which was red
   wholesale and is green now. **No new host unit test was possible for either half** — the ID
   table lives in a `cfg(target_os = "none")` module and the panic needs the machine's page tables,
   so the guest program is the pin, which is what §5 prescribes for a behaviour only the machine
   can show.

**`t-dhcp` — the defect was in the test, not the kernel.** `sendto` returned all 292 bytes ("the
DISCOVER goes out whole") and slirp never answered. The kernel sends exactly the bytes it is handed,
so the bytes were the suspect, and they were wrong in two places: the IPv4 destination was written
at offset 20 (the UDP port pair) instead of 16, so it was then overwritten by the port numbers and
the packet left with destination `0.0.0.0`; and the magic cookie and options were written at their
*BOOTP-relative* offsets applied to the absolute buffer, so the cookie landed 28 bytes early, inside
the BOOTP `file` field, and no options followed it. A BOOTP request with no cookie is not a BOOTP
request, and slirp was right to ignore it. Fixed by naming the two bases (`IP_DST = 16`,
`BOOTP = 28`) and deriving every field from them. The expectations were **not** touched — "an answer
arrives within five seconds", "the reply is a BOOTREPLY", "and it carries our transaction id" are the
host's promise and the kernel's job, and they now pass on the machine.
This is the §1 case where the test was genuinely wrong: the *construction* was defective, so it
never asked the question it meant to ask. Rule 1's carve-out covers exactly this, and the line is
drawn in the other direction from "weaken the assertion to make it pass" — no assertion was
relaxed, the input was corrected so the assertion finally means something.
What it pins: `AF_PACKET`'s receive path delivers a frame the IP layer never claimed (`net/mod.rs`
calls `packet::deliver` before the stack decides the frame is any of its business), and the reply's
`op`/`xid` survive that path byte-honest.

**Gate, quoted from its own summary**: host `570 passed`; `mpos-xtask 29 passed`; guest
`=== SUMMARY pass=136 fail=0 (136 built)`, QEMU exit 33, boot line
`virtio-sound: 44100 Hz, 2 channels at /dev/dsp`, `t-dsp === PASS`, `t-dhcp === PASS`.
The gate has no known red baseline any more; a new red is a regression, full stop. (The
`29 passed` and the `136 built` are that run's numbers; `tests.toml` has since taken nine
programs out of the *default* sweep — every one of them is still built — so a re-run prints
`=== SUMMARY pass=127 fail=0 (127 built)` with `9 of 136 user tests left out by tests.toml`,
and xtask's own suite is at `36 passed`. See the Phase 12 board entry for what the split is and
why.)

**Re-run with this pass's two commits on top** - the machine's `gpu` key and the §7.4
measurement, neither of which touches the kernel or the image: xtask's own suite `38 passed`,
guest `=== SUMMARY pass=137 fail=0 (137 built)` with QEMU exit `33` and `10 of 147 user tests
left out by tests.toml`. Worth quoting for one reason beyond the green: the test boot's own
QEMU line still reads `-device virtio-gpu-pci`, which is the new key's *default*, so the sweep
boots exactly the machine it booted before - and that is why a machine key written for an
interactive `run` cannot silently move the gate.

**A named host-suite flake — `rand::tests::a_reseed_re_keys_the_generator`.** The suite is not
parallel-safe over a module whose tests share a global, and this is the house pattern's one known
cost (`static X: Mutex<RefCell<…>>`). The test asserts that two `fingerprint()`s with no
`reseed()` between them are equal — the counterfactual for the re-key — but `fingerprint`, `fill`
and `reseed` all act on the module's *one* global generator, so a sibling host test that draws
from it in that window makes the second fingerprint differ. Reachable from `abi`'s `getrandom(2)`
test as well as from `rand`'s own, which is why a lock inside that module is not enough.
Measured on this tree: 2 of 3 `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` runs
fail that single assertion with `511 passed; 1 failed`, and 5 of 5 runs with
`-- --test-threads=1` pass — 0.28 s against 0.14 s. So a stage-1 red in exactly that test is a
flake, not the change: re-run before believing it, and quote the run that was green. The fix,
when someone takes it, is either serialising the suite or making the generator a value with the
global as one instance.

### 12.4b The machine's own tools, and the inode flag word — **DONE, gate green `pass=139 fail=0`**

Reported from the shell of the running image, four commands that could not run:

```
mpos:~# lsattr          lsattr: reading ./README: Not a tty
mpos:~# lsblk           lsblk: can't read '/proc/self/mountinfo': No such file or directory
mpos:~# lsusb           lsusb: /sys/bus/usb/devices: No such file or directory
mpos:~# lsmod           lsmod: /proc/modules: No such file or directory
```

Four different causes, and none of them was the command. The identity of the
commands came first, because everything else depends on it: **all four are
busybox 1.38.0-r7 applets** — `/bin/lsblk`, `/bin/lsattr`, `/usr/bin/lsusb` and
`/sbin/lsmod` are symlinks to `/bin/busybox`, created by the `busybox --install
-s` that apk's post-install trigger runs inside the build's chroot. That is not
what Alpine's package index says (`/bin/lsblk` there is util-linux's, `/usr/bin/lsusb`
is usbutils'), and it is why every interface below was *measured against the
binary in this image* rather than reasoned from a man page - the image was mounted
read-only and each applet run under `chroot`, against the host's `/sys`, against
`/proc` files written by hand, and against synthetic `/sys` trees built to this
kernel's shape.

Three of the four causes were files that did not exist, and the fourth was worse:
a *number* that was wrong. `lsattr`'s `Not a tty` is musl's `strerror(ENOTTY)` -
the `'f'` ioctl family had no handler. `lsblk` died on `/proc/self/mountinfo`,
which this kernel did not have at all. `lsusb` and `lsmod` asked for a directory
and a file that were not there. And `lsblk` would have died a second time on
`/sys/block`, which was also absent - measured: without it, `can't open
'/sys/block'` and exit 1, whatever else the tree holds.

**The one that was not a missing file: `st_dev`.** `lsblk` builds its MOUNTPOINTS
column by matching mountinfo's third field against the `dev` numbers
`/sys/class/block/<name>/dev` publishes. This kernel reported the mount table's
own attachment counter there - `/` was `0`, the first mount `1` - so even a
faithfully-shaped mountinfo would have left that column blank, and the same field
is what `df`'s `Filesystem` column and `findmnt` key on. Measured with a mockup
before the change: the device table came out right and the mount points column
came out empty. Linux answers with the *superblock's device*, and `mount(2)`
resolves the source to one (`lookup_bdev`); the same resolution now happens at
the same place here, and the number travels with the mount. A pseudo-filesystem
gets an anonymous number - major 0, from its own counter, as Linux gives one -
because `0` is not a candidate: a `dev_t` of zero means "no device" and two of
those compare equal whatever serves them. The mount's attachment identity is kept
as well, and the two are used for what each is for: the identity keys locks, unix
sockets and the `EXDEV` decision, the device number is what `stat` reports. The
same change found `/dev/vda2` reporting `st_rdev` of 0 - `DevBlockNode` had never
implemented `rdev` - so `ls -l /dev/vda2` had no `254, 2` to print either.

**The flag word, and what makes it worth having.** `FS_IOC_SETFLAGS` that stored
what it was handed would have closed this report and been worthless: `chattr +i`
would have protected nothing. So the two bits that mean something are *enforced* -
`IMMUTABLE` refuses the write, the write-open, the truncate, the removal, the
rename, the second link and every metadata change; `APPEND` refuses everything a
truncation would do and every write that is not going to the end - in the helpers
that already exist for refusals, which is how `unlink`, `rmdir` and the
copy-and-unlink `rename` is emulated with all refuse together. The four remaining
flags are accepted because something reads them: `NOATIME` where the access time
would move, `NODUMP` because its whole meaning is a promise *to userland* that
reads it back, `SYNC`/`DIRSYNC` because a write-through filesystem already keeps
that promise. Everything else is `EOPNOTSUPP`, including every bit Linux's own
`FS_*` vocabulary names and this kernel cannot keep - the list is in "Remaining
ABI support" above, and `AGENTS.md` §3's second governing rule - "answer
truthfully, or refuse" - is why it is a refusal rather than a stored bit.

**Two corrections that only the pins could produce.** Writing the enforcement test
first caught the kernel: `may_create_in` had been given an append-only check, and
Linux's `may_create` has none - an append-only directory "can only be added to",
so a name *may* be created in it and only removal is refused. And the gate caught
a lie older than this work: both `/proc/mounts` and `/proc/self/mountinfo` left
the root filesystem out entirely, because the root's record required a *source*
and only a promoted mount ever supplied one - so `/` was reported as not mounted
at all, to every `df`, every `findmnt`, and `switch_root`'s own grep. Linux prints
the filesystem's type there (`rootfs / rootfs rw 0 0`), and now so does this.

**Measured, on the real image.** `cargo xtask run --mode terminal`, then at the
login:

```
mpos:~# lsattr /root/README /root/start-weston.sh
--------------e----- /root/README
--------------e----- /root/start-weston.sh
mpos:~# lsblk
NAME            MAJ:MIN  SIZE TYPE MOUNTPOINTS
vda             254:0   2048M disk
vda1            254:1    128M part
vda2            254:2   1918M part /
mpos:~# lsusb; echo $?
0
mpos:~# lsmod; echo $?
Module                  Size  Used by    Not tainted
0
mpos:~# df -h /
Filesystem                Size      Used Available Use% Mounted on
/dev/vda2                 1.9G    916.1M   1001.9M  48% /
mpos:~# chattr +i /root/README; lsattr /root/README
----i---------e----- /root/README
mpos:~# echo x >> /root/README
-sh: can't create /root/README: Operation not permitted
mpos:~# rm /root/README
rm: can't remove '/root/README': Operation not permitted
mpos:~# chattr -i /root/README; echo ok >> /root/README
```

`lsattr` prints what it prints on this host's ext4; `lsblk` fills the column that
was blank; `lsusb` and `lsmod` are the honest-empty answers; and `chattr +i` is a
switch that really closes and really opens.

**Gate, quoted from its own summary**: host `665 passed`; xtask `36 passed`; guest
`=== SUMMARY pass=139 fail=0 (139 built)`, QEMU exit `33`, with `t-fsflags` and
`t-mountinfo` among the 139 and `10 of 149 user tests left out by tests.toml`.
One thing the gate does *not* cover, named rather than assumed: the block tree's
shape. The test machine boots with no disk at all, so `/sys/block` has nothing in
it to walk; the shape of a disk's entry, its partitions and their
`size`/`partition` attributes is pinned by host tests that build the tree from a
numbering they write down, and the real thing is what the run above shows.

### 12.5 Deliberately not in this phase

- Anything already honestly refused. `fanotify`/`keyctl`/`userfaultfd`/namespaces and the
  `_ => ENOSYS` dispatch arm are correct answers, not gaps to close here.
- `SO_LINGER`/buffer sizes (`record-only`) and `/proc/<pid>/smaps` (its sibling
  `maps` is answered now) — already tracked in §3.3 / the ABI board; this phase
  does not duplicate them.
  (`SO_REUSEADDR` was on this list and is not any more: it is applied at
  `bind`/`listen`, §3.3.3.)
- Real `MSG_PEEK` over TCP/UDP, real `rseq` state, real per-task CPU accounting: named here as
  sub-gaps, planned where the prerequisite (peek-preserving delivery, the clock, the
  scheduler's own timekeeping) is scheduled.

---

## Phase 13: Machine-seam purity and aarch64 parity — **13.1 AND 13.3–13.17 LANDED, 13.19–13.29 LANDED, 13.18 OPEN, 13.2 IN PROGRESS**

Phase 9.1 leaves the second machine short of the first in three named places, and Phase 11.4
closed the `hal` *seam* without closing the `hal` *file*. `crate::arch` is named by no module
outside `arch`/`hal`/`main`/`lib` — measured 2026-10-04: **0** code references, the depgraph's
allow-list holds — but the machine is still named by `#[cfg(target_arch = …)]` in **250** places
outside `src/arch/`, **14** of them inside `src/hal/mod.rs` itself. The shape to aim for is that
above the machine layer nothing knows *which* machine it is, only what capabilities it has; and
that the second machine reaches the same runtime surface as the first. Two threads, one plan,
because the parity work is where the leaks are measured.

**Measured starting state** (this tree, 2026-10-04):

| What | Where | Count |
|---|---|---|
| `crate::arch` named in code, outside `arch`/`hal`/`main`/`lib` | — | **0** (the depgraph rule holds) |
| `#[cfg(target_arch = …)]` outside `src/arch/` | all `src/**` | **250** (17 are `lib.rs`'s module gates) |
| … inside `src/hal/` | `src/hal/mod.rs` | **14 at audit time → 0** (13.1) |

Top holders outside `src/arch/`: `sched/runtime.rs` 30 · `syscall/linux/mod.rs` 29 ·
`syscall/linux/fs/ioctl.rs` 26 · `syscall/linux/proc.rs` 23 · `lib.rs` 17 ·
`syscall/linux/fs/file.rs` 15 · `hal/mod.rs` 14 · `drivers/virtio/mod.rs` 13 ·
`drivers/devices.rs` 12 · `console.rs` 10.

**aarch64's gap to x86_64** (§9.1) was three items beyond the disk root: `net`'s live path,
`cargo xtask test --arch aarch64`, and the fuller userspace the spawn tests run from (the x86
harness gets `ash`/busybox in its `/bin`). All four have landed — the disk root in 13.3, the stack
in 13.4, the gate in 13.5, the `/bin` in 13.6. What the phase found on the way is that the machine
already ran a scheduler-driven EL0 program, `execve`d, `clone`d and `wait4`d a child, and ran an
unmodified static-musl binary to completion, while virtio over MMIO already drove rng, blk (read
*and* write) and net's TX probe — so what was actually missing was the layer above those probes (a
socket stack to run on the transport, whose seam is now in place) and a gate that boots the second
machine instead of silently running the first, since `xtask/src/main.rs` branched on `--arch` only
for `Commands::Run` and `test --arch aarch64` ran x86_64 and reported its result.

**Stages** (13.1–13.2 are the purity thread, 13.3–13.5 the parity thread; each its own commit
and green alone — AGENTS §3 rule 7):

| Stage | Contents | Pin | Status |
|---|---|---|---|
| 13.1 | `hal` becomes arch-neutral: the optional-capability forwards (`Msi`, `SerialRing`, `FpuTrap`, `Segments`) and the aarch64 `ConsoleInput` forward stop being `#[cfg(target_arch)]`-gated in `src/hal/mod.rs`. The gate is a capability flag the build declares per target (`mpos_msi`/`mpos_serial_ring`/`mpos_fpu_trap`/`mpos_segments`/`mpos_console_input`, in `.cargo/config.toml`'s `[target.…]` blocks beside each machine's linker script, listed for `check-cfg` in `Cargo.toml`), so the file names no architecture and a machine without a capability still does not get a faked method (rule 4) | **landed** — `src/hal` `target_arch` 14 → 0; both bare-metal clippy targets clean; host lib `511 passed`; depgraph `11/11` | *landed* |
| 13.2 | The callers stop spelling the architecture: `console.rs`, `sched/runtime.rs`, `logging.rs`, `rand.rs`, `mm/{elf,heap}.rs` and the `syscall/**` gates express the machine difference through `hal`/capability flags or the ABI layer instead of `target_arch`. The per-arch register/struct ABI facts (`user_regs_struct`, `struct stat`, the signal frame) move to their `arch/<arch>/` home — the §11.4 boundary, kept, not erased | host tests + full QEMU gate (trap/fault/context paths) | *in progress* — landed `console.rs` (10), `uevent.rs` (4), `mm/mod.rs` (4), the `sched` FPU-trap protocol (8), `syscall`'s socket legs (22) and `rand.rs` (2, via a new baseline `hal::Entropy`), on `mpos_framebuffer_console`/`mpos_serial_ring`, `mpos_net`, `mpos_dynamic_mmio_window` and `mpos_fpu_trap`, plus six more deleted rather than flagged - the `ifreq` link ioctls and the unix-socket nonblocking push, gated when `net` was the first machine's alone and stale the moment §13.4 gave the second machine the stack (`fs/ioctl.rs` 22 → 18, `fs/mod.rs` 4 → 2); the `169 sites outside src/arch` figure is the measurement *before* those six, and before `hal::Gate` took the exec return and the user ip out of the ABI layer (`proc.rs` 23 → 20); and `sched`'s whole ptrace/stop block stopped being architecture-gated (`sched/runtime.rs` 22 → 0, `sched/mod.rs` 1 → 0, so **`src/sched/` names no architecture at all**), because that gate was never about the machine: the block carries a register file *by value* and read its width from the x86 `user_regs_struct` (`abi::uregs::COUNT` = 27) and its protocol words from `syscall::linux::abi`. The width is now the machine's (`hal::Contexts::ABI_REG_COUNT`: 27 on x86_64, 34 on aarch64, each defined beside the layout it describes) and `PTRACE_O_*`/`PTRACE_EVENT_*`/`SYSCALL_TRAP` moved to the `abi` leaf beside the clone flags — so `sched -> syscall` is **0** and the `sched <-> syscall` mutual pair went with it (10 → 9, the ratchet tightened to match). Nothing is faked: a machine whose syscall layer has no `ptrace` handler never sets a tracer, so the un-gated block answers "no" honestly, and the `PTRACE_SETOPTIONS` mask moved into `abi.rs` so the rule is host-tested and the eight option names have a reader on both machines. Measured: x86 gate `pass=143 fail=0`, aarch64 sweep `pass=13 fail=0`, `boot-aarch64` green, `target_arch` outside `src/arch/` 171 → 148, aarch64 warnings 108 → 94; and the survey for that stage turned up two more **stale** gates of the `ifreq` kind - `process_vm_readv`/`writev` were `not(aarch64)` along with `ptrace`, although nothing in them is the first machine's: they walk the two iovec arrays (`transfer_iovs`, always ungated and therefore dead code on aarch64 until now), ask `sched::space_root_of` and `sched::may_inspect`, and reach the target through `read_user_of`/`write_user_of`. The aarch64 number table had named 270/271 for them all along, so both gates and both dispatch arms are gone and the second machine now serves the calls (`proc.rs` 20 → 18, `syscall/linux/mod.rs` 11 → 9). **Nothing drove them on either machine, so the cut adds the pin**: `user/tests/src/bin/t-process-vm-libc.rs` - the `-libc` suffix follows `t-unix-rustix`, since `rustix` binds neither call and they go through `libc::syscall` with the number from `libc::SYS_*`, a maintained per-arch table, which is exactly what that crate's `rustix` rule is for. It runs on **both** machines and outputs byte-for-byte identical lines, including a forked child whose memory the parent reads and overwrites while it is parked (the write that proves the two address spaces really are two). Its own assertions were checked against real Linux on the host first - 31/31 - because a pin's assertions are the first thing that can be wrong. Measured: x86 gate `pass=144 fail=0`, aarch64 sweep `pass=14 fail=0`, `boot-aarch64` green, `target_arch` outside `src/arch/` 148 → 144, aarch64 warnings 94 → 91. **The third stale-gate finding was `rt_sigtimedwait`**, gated with `ptrace` although the `siginfo_t` it fills is the generic 128-byte `kernel_siginfo` (`si_signo`/`si_errno`/`si_code` at 0/4/8, `si_pid`/`si_uid` at 16/20, the `union sigval` at 24, the same on both machines) and the aarch64 table had named 137 all along (`proc.rs` 18 → 17, `syscall/linux/mod.rs` 9 → 8). Writing its pin found two **real** divergences, not only the stale gate, and both are fixed: a NULL set pointer was read as "the empty set" and answered `EAGAIN`, where Linux copies the set in and answers `EFAULT`; and `sched::exit` queued a child's `SIGCHLD` only when the parent had a *handler*, so a parent that blocked `SIGCHLD` and waited for it with `sigtimedwait`/`sigwait`/`signalfd` slept for ever - Linux's `sig_ignored()` discards a generated signal only if it is unblocked and ignored, and says a blocked signal is never ignored (measured on Linux 7.2.8: a handler-less process that blocks `SIGCHLD`, forks and calls `sigtimedwait` gets `rc=17`, `si_code=CLD_EXITED`, `si_pid` the child). The pin `user/tests/src/bin/t-rt-sigtimedwait.rs` runs on both machines, std-side, through `libc::syscall` (16/16 on real Linux first). Measured: x86 gate `pass=145 fail=0`, aarch64 sweep `pass=15 fail=0`, `boot-aarch64` green, `target_arch` outside `src/arch/` 144 → 142. **The syscall half's register-ABI Stage 1 landed** (§13.2's own next step): `hal::Gate` grew four ops - `user_context` (the ring-3 frame a fork/clone child resumes with, one op for both machines), `capture_user_regs`/`restore_user_regs` (the gate frame read out as, and written back from, the ABI's register file, with the thread pointer passed in and any new value handed back because installing it is the scheduler's), and `syscall_args` (the six argument registers in ABI order, re-read *after* a tracer's stop so a rewritten argument reaches the call) - plus `hal::ABI_REG_COUNT`/`hal::AbiRegs` at the crate level, so `sched` and the syscall layer share one definition of the file's width. The x86 bodies moved into `arch/x86_64/syscall.rs` over the entry stub's own `Regs` (`arch/x86_64/context.rs`'s `uregs` gained the `r11`/`rcx`/`orig_rax` slots the frame needs), the aarch64 `user_context` twin and the identity register-file ops into `arch/aarch64/mod.rs`, and `proc.rs`'s `capture_frame`/`apply_frame` became forwards. The now-neutral sites un-gated: `syscall_entry_stop`, `syscall_exit_stop`, `tell_tracer_of_child`, the exec-trap block, `handle_fork` (and the `nr::FORK|VFORK` arm), and `handle_clone_impl`'s announcement. `struct RegsFrame` is **kept** for the signal family (`build_frame`/`handle_rt_sigreturn`/`deliver_signal`), which is Stage 2's subject. `hal::Segments` is now superseded - the gate ops read the selectors from `gdt` - and is removed as its own follow-up (**landed** immediately after Stage 2, at the end of this cell). Measured: x86 gate `pass=145 fail=0`, aarch64 sweep `pass=15 fail=0`, `boot-aarch64` green, clippy 0 on `x86_64-unknown-none`/`aarch64-unknown-none`/host lib/aarch64 loader, fmt clean, depgraph 9/9 within ratchet and **no `arch -> sched` edge**, `target_arch` outside `src/arch/` 142 → 129. **The syscall half's signal frame followed, and it is Stage 2**: `hal::SignalFrame` is the capability a machine lays a handler's frame out behind - `enter` (the frame bytes *and* the register file that enters the handler) and `exit` (the frame read back, and the mask it restores) over a `SignalDelivery`/`SignalInfo` value shape, so `hal` still names no layer above it. The x86 `rt_sigframe`/`sigcontext` layout - the `sc` offsets, the three part offsets, the put/get words and the register shuffling - moved out of `proc.rs` into `src/arch/x86_64/signal.rs`, the `SIGFRAME_*`/`UCONTEXT_*` constants left `sched::signal` for good, and `proc.rs`'s `build_frame`/`handle_rt_sigreturn` became policy plus `hal` plus the two scheduler calls the machine cannot make (`sched::write_user`/`read_user`, `set_current_blocked`) - so `struct RegsFrame` is deleted, and with it the last place the ABI layer spelled the x86 gate frame. The gate is the build's (`mpos_signal_frame`, declared by the x86_64 machine alone), so the four signal-family gates and their three call sites are `#[cfg(mpos_signal_frame)]` and name no architecture; a port without the capability is *not delivered a handler* rather than handed a frame it cannot lay out (rule 4). `handle_rt_sigreturn` now reads the whole frame back (one `read_user` instead of a mandatory `sc` read plus a best-effort mask read), and the `ioctl` restart verdict's request word comes from `hal::syscall_args`. Measured: x86 gate `pass=145 fail=0`, aarch64 sweep `pass=15 fail=0`, `boot-aarch64` green, host unit tests 511, clippy 0 errors on `x86_64-unknown-none`/`aarch64-unknown-none`/host lib/aarch64 loader, fmt clean, depgraph 9/9 within ratchet and **no `arch -> sched` edge**, `target_arch` outside `src/arch/` 129 → 122 (`proc.rs` 7 → 3, `syscall/linux/mod.rs` 7 → 5, `syscall/mod.rs` 1 → 0). **Then `hal::Segments` went**, as that follow-up: the trait, its `user_segments` forward and the x86_64 impl read the GDT for a `cs`/`ss` pair that `hal::Gate`'s `capture_user_regs`/`user_context` already fill from the same place, so the capability was superseded rather than merely unused, and only its plumbing left to delete (`mpos_segments` from `.cargo/config.toml`'s x86_64 block and `Cargo.toml`'s `check-cfg` list, plus the docs that listed it among the current capability set). Measured: x86 gate `pass=145 fail=0`, aarch64 sweep `pass=15 fail=0`, `boot-aarch64` green, clippy 0 errors on the same four targets, fmt clean, depgraph 9/9 - and `target_arch` outside `src/arch/` stays **122**, because a capability flag was never one of its sites. **The register-ABI relocation's Stage 3 began with a measurement that is not a frame at all — this machine's EL0 trap did not give a program its registers back.** `user/tests/src/bin/t-regs-across-syscall.rs` (std, both machines) stamps the callee-saved integer registers and every SIMD/FP register with a pattern, does one `getpid` in the same `asm!` block, and compares: 41 registers on aarch64, 20 on x86_64 (the reserved ones - `x19`, `rbx`, the frame pointers - cannot be inline-asm operands at all, which the file says). **Red on aarch64 before the fix, and the log is one line: `FAIL: the trap did not give back 1 of 41 registers: v0 came back 0x0000000000000000`.** The reason is the one the context switch's header already states: this kernel is built for `aarch64-unknown-none`, whose baseline includes NEON, so LLVM keeps its own values in `q0..q31` — and unlike x86_64's `+soft-float` build there is nothing to remove them with. Building *this crate* with `-C target-feature=-neon` is not enough, and that was measured rather than assumed: the binary still carried **420** references to `q`/`v` registers, and the reachable ones are `core`'s (`Formatter::pad`, `pad_integral`, `do_count_chars` — every `println!` — plus `memset`); only `-Zbuild-std` with the flag left the 32 in `mpos_context_switch` and nothing else. So the fix is the trap stubs, where the machine owns the registers: `__vec_el0_sync` and a new `__vec_el0_irq` (the EL0 IRQ entry used to share the kernel's stub) each carry the interrupted 528 bytes - `q0..q31`, `FPCR`, `FPSR` - in their own frame. On the trap's own stack rather than in the task's record, and that is not a style: a task parked in `read` is switched out and back while the trap is open, and the context switch writes the kernel's live scratch into the record as it goes past. The pin's own first version had a real bug worth recording, because the counterfactual is the point: restoring the FP *after* the register file handed the program an `FPCR` in `x9`/`x10`, and **6 of the 16 sweep programs** took a wild pointer (`user fault at 0x19`, an `EFAULT` out of `recvfrom`) because a `rustix` block had kept a live value there across the `svc`. Cost, stated rather than hidden: 20 instructions to save and 20 to restore on every EL0 trap, which is the price of a kernel that uses the vector registers at all. Measured after: x86 gate `pass=146 fail=0`, aarch64 sweep `pass=16 fail=0`, `boot-aarch64` green through `M6_INIT_OK`/`INIT_EXITED`, host unit tests green. **Stage 3b is the frame itself, and the second machine now delivers handlers.** `src/arch/aarch64/signal.rs` lays out arm64's `rt_sigframe` behind the same `hal::SignalFrame` the x86 twin implements, and `mpos_signal_frame` is declared for the target - so the four signal-family gates in `proc.rs` and their call sites, which were the *only* reason the family was off, turn on with no change of their own (the aarch64 number table had named `rt_sigaction`/`rt_sigprocmask`/`rt_sigreturn` 134/135/139 all along). Three facts about the layout came off `arch/arm64/kernel/signal.c` and the two uapi headers rather than out of the plan, and two of them correct it: **the restorer is in `x30`, not a word on the stack** (`setup_return` puts `sa_restorer` there), so `rt_sigreturn` is handed a stack pointer that *is* the frame - `SignalFrame::frame_at` is the identity here and `sp - 8` on x86_64, which is the one *shape* the two machines disagree about; **there is a 16-byte `struct frame_record` above the frame** holding the interrupted `x29`/`x30` with the handler's `x29` aimed at it, so a backtrace started inside a handler walks into the code the signal interrupted; and **`uc_mcontext` is at 176, not the 168 the field widths add up to**, because `struct sigcontext` contains a 16-byte-aligned member and the compiler pads `ucontext` up to it (the libc pads the same way, which is what makes the handler's `ucontext` and the kernel's agree). The floating-point half is the machine's own trap frame rather than its task record - the record is what the context switch writes the kernel's scratch into, so the interrupted `q0..q31`/`FPCR`/`FPSR` are copied into the frame's `fpsimd_context` record from where Stage 3a put them, and `rt_sigreturn` writes them back there for the trap's own exit to load. **Three real defects came out of writing the pin, and each is fixed here:** `apply_frame` seeded the thread pointer with a zero it expected the machine to overwrite - x86_64's register file has `fs_base` and does, aarch64's `user_pt_regs` does not, so the first handler delivered on the second machine left its task's `TPIDR_EL0` at 0 and the very next `println!` faulted reading musl's `errno` at `0x58` (NULL plus the libc's offset); the aarch64 gate wrote the dispatch's return value into `x0` *after* the delivery, so every handler ran with `x0 = 0` where the signal number belongs - `x0` is the result slot *and* the first argument, so the write moved into `hal::Gate::set_result` where it happens before a handler is entered (x86_64's `rax` is a plain scratch register and the stub still writes the same value after, so its behaviour is unchanged); and `SA_RESTART`'s rewind needs the interrupted call's *first argument*, which the result slot no longer holds by the time a signal is delivered - the trap stub keeps it beside the frame as `orig_x0`, exactly the word arm64's `pt_regs` keeps after `user_pt_regs`. `hal::SignalFrame`'s two operations became `unsafe fn` (clippy's `not_unsafe_ptr_arg_deref`, the same finding Stage 1 made for `linux_dispatch`): they take the machine's own gate frame. The pin is `user/tests/src/bin/t-signal-frame.rs`, and it runs on **both** machines: a sibling process raises `SIGUSR1` while the program is parked in a `read` it made from its own inline assembly, and it holds `x0`'s signal number, the `siginfo_t`'s `si_code`/`si_pid`/`si_uid`, `uc_mcontext.pc` naming the trapped instruction (`svc`+4, or the `svc` itself when the call is rewound), every callee-saved register and all thirty-two vector registers coming back, `SA_RESTART` really re-running the `read` with its own arguments (the second execution reads the bytes the sibling writes afterwards), and a child that dies of `SIGABRT`/`SIGSEGV` reported to its parent as `WIFSIGNALED` with that `WTERMSIG`. Its aarch64-only half reads the frame record the handler's `x29` points at and the `fpsimd_context` record, both *inside* the handler - the only moment those words are the frame's, since the frame lives below `sp` and the program is entitled to that stack again the moment the handler returns (measured: the record is right in the handler and holds something else by the time `main` looks). **That half is aarch64-only for a named reason**: x86_64's `uc_mcontext` has a null `fpstate` pointer and no record chain, so its `rt_sigreturn` does not restore the vector registers a handler clobbered - a pre-existing gap this pin now measures, and its own piece of work. Measured: x86 gate `pass=147 fail=0 (147 built)` with QEMU exit 33, aarch64 sweep `pass=17 fail=0`, `boot-aarch64` green through `M6_INIT_OK`/`INIT_EXITED`, host unit tests green, clippy 0 errors on `x86_64-unknown-none`/`aarch64-unknown-none`, fmt clean, `cargo xtask depgraph` `9/9` within the ratchet with no new `arch -> sched` edge. |
| 13.3 | aarch64 disk root: the MMIO virtio-blk already read/write-verified (`M7_BLK_OK`) and the ext4 it mounts (`M7_FS_OK`) become the machine's root; `xtask` assembles the aarch64 disk from `rootfs.toml`'s geometry and the `rootfs/` tree | a boot marker that a rootfs *file* was read, not merely that an ext4 mounted | **landed** — the ext4 is registered as `vda`; `xtask` lays the `rootfs/` overlay and the init images on it; the machine pivots its root onto the disk (`/` = ext4, ramfs at `/oldroot`), re-mounts `/dev` and runs the disk's `/init` as pid 1 (`M7_DISK_OK`, `M7_ROOT_OK`), with the init's file lifecycle now running on ext4 |
| 13.4 | aarch64 `net`: smoltcp over the `MmioTransport` whose MAC and TX are already proven (`M8_NET_OK`); RX via GIC line routing, or an honest polled note | a transmit/receive round trip on the second machine | *landed* — flipping `net`'s gate had cost **24** errors (16 the missing `smoltcp` dependency, 8 the PCI NIC, the PCI vsock device and the four `/proc/net` rows), which is why the cut was the seam first: `drivers::virtio` now carries the transport-neutral contracts `NetDevice`/`VsockDevice`, `net`/`net::vsock` hold `Arc<dyn …>` and name no transport, `VSOCK_PER_CONNECTION_BUFFER` moved beside the contract, and the `/proc/net` collector took `procfs`'s own gate rather than a new capability (with the gate opened by hand that was **0 errors**, was 8). The live path then landed with it: `mmio::MmioNet` is the MMIO adapter as a persistent `NetDevice` (the ring it posts stays posted, unlike the probe helpers it replaces), `check_net` opens it once and publishes it (`mmio::init_net_global`), `board/aarch64.rs`'s `init_net` hands that handle to `net::init` + `net::spawn_service` - the same pair x86_64 boots with - and prints `M8_NET_STACK_OK`, `lib.rs`'s gate is `target_os = "none"`, and both machines declare `mpos_net`. **Measured: the aarch64 sweep passes 10/10 staged programs**, including a TCP connect/accept/read-write round trip, UDP over the loopback and an `AF_UNIX` name - the round trip pinned by real EL0 programs (this job's pin), not a kernel hook. The flip also found three stale "`net` is x86_64's" gates in shared code (`close(2)`'s `close_last_socket` - a measured port leak: `t-socket`'s 34567 still held when `t-tcp` bound it - `unlink`'s `unix::unbind_node`, and `ioctl`'s two `ifreq_*` helpers, whose aarch64 arms answered `None` and made every interface ioctl `ENODEV`), and the harness's `/tmp` had to become a tmpfs like x86_64's, because an ext4 directory cannot hold a socket node (`t-unix-rustix`'s `bind` answered `EOPNOTSUPP` until it was). RX is polled, honestly: the service task's tick drives the device and nothing routes its GIC line yet |
| 13.5 | `cargo xtask test --arch aarch64`: the sweep boots the second machine and reads its own `=== RUN/PASS/FAIL/SUMMARY`/`END` and `M*_OK` chain with x86's verdict (summary + QEMU exit). An unsupported `--arch` is an explicit error, never a silent run of the other machine | `cargo xtask test --arch aarch64` prints a green summary; x86 unchanged; `xtask` unit tests green | *landed, with its gaps named* — the silent wrong-run is closed: `test --arch aarch64` now refuses in `test_sweep_machine` (pinned by an `mpos-xtask` unit test) instead of falling through to `selftest::run_tests` and printing an x86_64 verdict under an aarch64 banner. The aarch64 sweep itself — the user programs built for aarch64, packed with a generated `/init` that runs `/bin/t-*`, booted under AAVMF and parsed by the same `=== RUN/PASS/FAIL/SUMMARY`/`END` reader — is the next cut, and its first prerequisite has landed: the musl user programs build for aarch64 under cargo at all now (`.cargo/config.toml` names `rust-lld` for `aarch64-unknown-linux-musl`; without it the host `ld` rejects the `--fix-cortex-a53-843419` rustc passes for the target, measured — 28 programs, static `ET_EXEC`, ~5.6 MB). **And the sweep itself is wired**: `test --arch aarch64` builds the second machine, packs a musl runner (`aarch64_init::build_sweep_init`) as `/init` beside those 28 programs, boots under AAVMF and reads the same `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol. Its first run is **red, with the reason located**: the runner's `posix_spawn` path takes an EL0 **write** fault (ESR `0x9200004f`, level-3 permission; address `0x3fadafe0`) at `pc 0x272d90`, which is inside the runner's own R+X segment — so the fault is in musl's spawn, not in a test — and the machine then loses its init (`INIT_EXITED`), so no summary is printed. **That fault is fixed**: the handler read `WnR` from `ESR_EL1` bit 7, but a Data Abort's `WnR` is bit 6 — so every fault looked like a *read*, and `resolve_cow_fault` (guarded by `is_write`) was never reached: an EL0 write to a copy-on-write page always ended in SIGSEGV. The evidence is the run's own fault line, `ESR 0x9200004f` (`DFSC 0xF`, level-3 permission) on a page the tables call `r--u` and `is_cow_page` calls copy-on-write, 16 bytes below `sp` — a `clone` child's first write. It survived this long because the machine's own fork check has its child only *read* its marker string and let the kernel do the writing, so no child ever wrote to a shared page. With bit 6 the run passes that point and reaches the next gap: two faults in the runner — a write to `0xffffffffffffff5c` (translation fault, level 1) and a read of `0x58` (level 2) — which is where the aarch64 ABI is still short. **Both were fixed, and the run now completes.** The second was the same class of omission: `prepare` reinstalled `task.tls` on every dispatch but never captured the outgoing task's *live* value — and aarch64's musl installs its TLS base with a direct `msr tpidr_el0`, with no syscall for the kernel to see, so the record went stale and the base was reset to 0 by the first context switch after a spawn (the runner faulted at `0xffffffffffffff5c`, NULL minus musl's `errno`/canary slot). Capturing on switch-out is what arm64 Linux does; x86_64 is unaffected because its base is an MSR only the kernel writes. **Measured now: all 14 programs run, `=== SUMMARY pass=0 fail=14`, `=== END`** — the *instrument* is whole, and what is left is the ABI itself. (A harness bug came with it: cargo's `.d` dep-info files start with `t-` and were staged as programs, reporting `exit=127`; the payload now requires the ELF magic, which is why the count is 14 and not 28.) **The same class struck again in the copy**: `spawn_copy`/`spawn_thread` inherited the *record* (`Task::tls`) instead of the base the parent was running with, so the sweep's first `posix_spawn` child — its parent having set its own base after its last dispatch, the record still saying 0 — ran with `TPIDR_EL0 = 0` and faulted at the same NULL-minus-`errno` address. arm64 Linux's `copy_thread` hands the child the parent's live `tp_value` when `CLONE_SETTLS` is absent, and both sites now ask the register instead (`running_tls`); measured, `t-rename` and `t-stdspawn` — the two that faulted — reach their own code and fail on the harness's missing `/tmp` rather than on the kernel. **The classification is done and the arm is green on what this machine can serve**: 3 of the 14 std-side programs run and pass (`t-rename`, `t-rmdir`, `t-stdargs` — `=== SUMMARY pass=3 fail=0`, exit 0) and the other 11 are named in `tests.toml`'s new `aarch64` group with their reasons. The file gained the rule that makes that honest rather than a global exclusion wearing a machine's name: **a group named after a machine belongs to that machine**, invisible to the other's sweep, so all 11 still run in x86_64's default sweep (pinned by a `mpos-xtask` unit test). Ten are the socket family (`socket`/`socketpair` answer ENOSYS here — measured: syscall 198/199, §13.4's flip is what closes them) and `t-stdspawn` is a harness gap (`/bin/true|echo|cat|false|sh`; the x86 harness has them from busybox, this machine's disk root has no `/bin`, while the kernel's spawn path is exercised by the runner on every program). Reaching three needed the machine setup the x86 `/init` does with busybox — the runner makes `/tmp` and passes `t-stdargs` the argv the x86 PATH lookup passes it — and a `=== SETUP_FAIL` line now refuses the verdict instead of letting an unprepared machine read as failing tests. `--groups`/`--all-groups` remain refused for this arm: the file's vocabulary also names the `no_std` raw-syscall suite, which has no aarch64 port |
| 13.6 | aarch64 `/bin`: the programs `t-stdspawn`/`t-socketpair` start, built **in-tree** as five applets of one multi-call binary (`xtask/src/aarch64_bin.rs`) rather than installed from a distribution | both names leave `tests.toml`'s `aarch64` group and `cargo xtask test --arch aarch64` stays green; x86 unchanged | *landed* — the gap was the harness's, not the ABI's: `t-stdspawn` runs `/bin/{true,false,echo,cat,sh}` and `t-socketpair` runs `/bin/cat`, and this machine's ext4 root carried no `/bin` (the payload's own programs sit flat at `/`). **The choice that matters is build-or-fetch**: installing busybox — the x86 harness's answer, from Alpine — would put a network fetch *inside* the aarch64 gate, so a mirror outage would read as a broken kernel; the `rustc`/`aarch64-unknown-linux-musl` path the sweep's runner and musl helper already require builds these with no new prerequisite. `sh` is a small shell whose own comment states its subset (`;`/newline separation, `'`/`"`/`\` quoting, `<`/`>`/`>>`/`>&N`, the `echo`/`exit`/`cd`/`pwd` builtins, external programs by path or `PATH`) — honestly *not* busybox — and it moves **real descriptors** rather than modelling them, because `t-stdspawn` asserts the child put `captured-err` on stderr and reads both pipes. Two staging facts came out of it: a payload name may carry a directory (the kernel's `vfs::tar::unpack_into` creates missing parents on demand, so the archive needs no directory member) and every file the staging step lays down is mode **0755**, which `mkfs.ext4 -d` preserves — without it `/bin` lands 0644 and `execve` answers `EACCES`, a failure that reads as a broken program. **Measured: `=== SUMMARY pass=12 fail=0`** (12 of the 14 aarch64-built programs staged; the two left out, `t-stdresolve` and `t-udp`, are `external` for the internet's reason rather than this machine's), `t-stdspawn`'s `output() captured both of the child's streams` and `t-socketpair`'s `read back what the pair carried` among the passes, with x86 unchanged at `pass=142 fail=0`. The `aarch64` group is **empty** and stays as an empty group: the mechanism is what names a gap, and the day a call cannot be served is the day a name goes back in |

| 13.7 | aarch64 `SerialRing`: the PL011's line (SPI 33, the tree's `interrupts = <0 1 4>`) configured group 1 / priority 0x80 in `gic::init`, armed by the wiring once a tty exists, and served by the same visit that drains the receiver; the machine declares `mpos_serial_ring` and `crate::console` takes its queue-and-park path with **no edit above `arch`** | `M5_SERIAL_TX_OK` in `boot-aarch64`: bytes *queued* by a writer and *moved* by an **interrupt**, counted separately, with more than one line's worth so a queue that only ever held a line could not pass; x86_64's sweep unchanged | **landed** — the queue both machines drain was lifted out of `drivers/serial.rs` first (`drivers::tx_ring`, behind a three-method `Transmitter` trait), because the alternative was a second copy of "who sleeps for room, and when to stop asking the transmitter" written against the PL011's one shared `IMSC` register - the exact duplication the machine seam exists to prevent. What the pin measures is the difference a print cannot: `stats()` counts what writers queued against what the *handler* handed over, so a polled write (or a line that never fired) reports a queue nothing drains instead of passing. Three facts fell out of it. (1) The register carries *both* directions, so every mask write is a read-modify-write. (2) Writing `ICR` with every bit - the obvious "clear anything stale before arming" - **hung about half of all boots**, silently, at the first queued line: `TXIC` clears the transmitter's *latched* event, and a level-triggered transmitter that has lost its latch asserts nothing until the next byte leaves it, which is the thing the interrupt was going to do. Whether a given boot hung depended on whether the previous poll-written line's bytes had finished leaving when `ICR` was written, which is why a loop of runs found it and one run did not; `enable_interrupts` now clears everything except `TXIC`, and the fix is the difference between 5/5 hung and 10/10 green. (3) It is a race rather than a state, so the check had to be able to speak: its failure marker goes out through the machine's *polled* writer, because a report filed through a queue that is not draining is a report nobody reads. The polled `ConsoleInput` receiver stayed (the self-test that has no terminal to type on), and with the RX interrupt live the tick-rate input pump task left the boot entirely |
| 13.8 | aarch64 interactive shell: `run --arch aarch64` stops booting a hand-encoded read/echo loop and hands pid 1 to the machine's real shell — an in-tree `/init` (`aarch64_init::build_shell_init`) that makes `/tmp` and `execve`s `/bin/sh`, with the `sh` applet reading a line at a time (a prompt on a terminal, silence on a pipe), `ls` added to the `/bin` image and `cat` taught its file arguments; the superseded `looping_init_elf` is deleted rather than left as dead code | a scripted `run --arch aarch64` drops to a prompt and runs commands; `t-stdspawn` pins `/bin/ls`, `sh` fed a script on a pipe, and `cat FILE` on **both** machines | **landed** — the payload is the same multi-call `/bin` the sweep stages, so the program a person types at is the program the spawn tests run. Measured: `cargo xtask run --arch aarch64` with `echo hello-from-guest`, `ls /etc` and `cat /etc/inittab` fed to it prints `# `, `hello-from-guest`, `conf.d doas.conf fish inittab local.d network profile.d resolv.conf xdg`, and then the file. Two things fell out of doing it: `cat` had no file arguments at all (it copies standard input), which on a terminal is not an error but a *hang* — the shell's first real command blocked until the next keystroke and then printed that — and the shell's very first `read(0)` was the `EFAULT` §13.9 is about |
| 13.9 | aarch64 `uaccess`: the range check accepts a page the tables do not describe yet — an absent page is the live VMA table's to answer, a copy-on-write page counts as writable (its write is the fault the handler splits), and `len == 0` is accessible, which are the three rules `arch/x86_64/paging.rs` has always had; and the **EL1** synchronous entry resolves the two data aborts that fallback makes reachable, because the kernel reaches user memory through the caller's own tables | a program that fills memory nothing has touched: `t-lazy-buffer` (a 1 MiB `mmap` read into whole, and a `fork` child reading into a page its parent wrote) passes on **both** machines, and fails on the old kernel with `EFAULT`; `cargo xtask run --arch aarch64` reaches a working shell | **landed** — the second machine's `user_accessible` required every touched page to be *present*, so a `BufReader`'s 8 KiB heap buffer — allocated and unread, its second page with no table entry yet — was refused as `EFAULT`, and that is the shell's very first `read(0)`. Measured, and it is how the bug was found: `sh: debug err Bad address (os error 14)` was the whole of the interactive boot's failure. The same check refused a `fork` child's buffer, because a shared page is copy-on-write and therefore read-only; `arch/x86_64/paging.rs` reads `is_cow_page` in exactly that place and has since the fork path landed. The boot's own `M6_PAGING` check then went **red** on the corrected rule — it asserted that marking a page copy-on-write stops it being writable to uaccess, which is the divergence written down as a contract — so the check was corrected with the evidence rather than the fix backed out: a COW page's write is answered by the handler on both machines. Fixing the check alone is not enough and the second half is the interesting one: with the VMA fallback the kernel now *copies into* an absent page, and on aarch64 that is an EL1 data abort, which the vector table sent straight to the fatal dump. `__vec_sync` now calls `sync`, which resolves a data abort at a user address through the same `resolve_cow_fault`/`resolve_anon_fault` the EL0 entry uses and `eret`s back to retry the instruction; a fault at a kernel address, an instruction abort, or a user address no VMA backs is still the machine stopping with a dump. The stub saves x0–x18 and x30 around the call (`bl` clobbers them and the retried instruction belongs to a function that may hold its return address in x30); the condition flags need no saving because `eret` reloads PSTATE from `SPSR_EL1`. Measured: aarch64 sweep `=== SUMMARY pass=18 fail=0`, x86_64 gate green, `cargo xtask boot-aarch64` green, host lib 511. **Named gap found on the way, not fixed:** a *non-empty* `std::fs::remove_dir_all` faults the calling process at `pc 0` (ESR `0x82000006`, a level-2 translation fault at address 0) on this machine — reproduced three times, always with the check's own `ok:` line as the log's last and a marker written immediately before the call never reached. Nothing else in either suite removes a tree recursively, so this had never run there; the same removal by hand (`read_dir` + `remove_file` + `remove_dir`) works, which is what `t-stdspawn` now does, so the suspicion was the empty-name `unlinkat(fd, "", AT_REMOVEDIR)` that ends Rust's implementation. It is a pre-existing gap in this machine's ABI surface, orthogonal to §13.8/§13.9, and it is named here rather than left to be rediscovered — **§13.10 refuted the suspicion and fixed the cause**, which was not in the tree walk at all: a `struct stat` sixteen bytes too long, whose overflow lands in whichever frame the libc's own stat buffer sits in |

| 13.10 | aarch64 `struct stat` is 128 bytes and the kernel filled 144: `write_stat`'s length becomes the ABI's own (`STAT_BYTES`, 128 on aarch64 and 144 on x86_64) for the bounds check and the zero-fill as well as for the field writes, so a caller's — or a libc's — 128-byte struct is not written past | `t-stat-buffer` hands the kernel a guarded `struct stat` through `SYS_fstat`/`SYS_newfstatat` and reports the canaries intact on **both** machines (against the old length it reports "2 guard words written"); `t-rmtree` removes a flat and a nested non-empty tree with `std::fs::remove_dir_all` (against the old length it dies at `pc 0`) | **landed** — found by §13.9's named gap, and the fault was not in the tree walk the name suggested: `pc 0` with `lr 0` and `ESR 0x82000006` is a return through a zeroed link register, and musl's `fdopendir` stats the descriptor *before* it reads anything. Whose memory the sixteenth byte past the struct lands on is the interesting part: musl's `stat`/`lstat`/`fstat` never hand the kernel the caller's struct — `__fstatat` makes the syscall into a `struct stat` of its own on the stack and copies the fields out, which is how it stays correct where the two layouts differ — so the extra bytes went into the frame past *musl's* buffer. That is why every program's own `stat` call had survived them, and what finally caught them was code that keeps a return address in the frame beside it: `fdopendir`'s. Measured both ways: with the old length a non-empty `std::fs::remove_dir_all` faults the process at `pc 0` and `t-stat-buffer` reports two guard words written, while the ABI's length passes both and leaves `MPOS_STAT_OK` and the x86_64 gate untouched. The std-side suite had no `stat` test at all, which is why nothing had ever asked the writer how long it was |

| 13.11 | `struct statx` is one 256-byte layout on every machine: `write_statx` moves to Linux's offsets — `stx_atime`/`btime`/`ctime`/`mtime` at 64/80/96/112, `stx_rdev_major`/`minor` at 128/132, and the file's device split into `stx_dev_major`/`minor` at 136/140, which is the number `struct stat` carries packed as `st_dev` — and fills all 256 bytes rather than stopping at `stx_mnt_id` | one file, read through `statx` **and** through the `fstatat` family, must report the same device, inode, mode, size, link count and three clocks: `t-statx-agree` on **both** machines (its clocks set with `utimensat`, so a zero cannot pass for a time), plus the bare-metal `t-statx`, which now compares its clocks against `newfstatat`'s and checks that the whole struct was written | **landed** — the sibling §13.10 named and did not fold in, because 152 bytes into a caller's 256 is an under-write rather than the write-past that fix was about. Measured on the host before anything changed, against a file whose three clocks differ: atime 64, btime 80, ctime 96, mtime 112, rdev 128/132, dev 136/140, mnt_id 144, and every byte past 152 zero — where this kernel wrote its clocks at 96/112/128, its rdev pair at 72 (Linux's `stx_atime.tv_nsec`) and stopped the write at 152. Nothing had read those fields, and why is worth keeping: Rust's `std` does not call `statx` on this libc (its support is glibc-only, so `fs::metadata` is `fstatat`), and `rustix`'s `stat` family only reaches for `statx` on 32-bit targets — so the readers here are the programs that call it explicitly, like uutils' probes and musl's own wrapper. The device pair is information rather than a moved field: `statx` had none, so every file read through it reported `0:0`, which a tool grouping files by device reads as "all one filesystem". The same pin then failed for a second reason, which is the better argument for having written it: **x86_64's `struct stat` had its clocks in the wrong places too** — `st_mtime` at 80 and `st_ctime` at 88, where Linux has `st_atime_nsec` and `st_mtime`, so a program reading the mtime read the ctime and one reading `st_ctime` (104) read zero, which is what the pin said first (`1700000321 vs 0`). Both ABIs put the three `timespec` seconds at 72/88/104 with the nanosecond slots at 80/96/112, so the clocks are no longer inside the per-machine branches at all: they are written once at offsets the two shapes share, and the host `write_stat` test asserts each clock against the `_nsec` slot beside it. That divergence is older than this phase and was invisible for the same reason as the others — nothing read those two fields back, and the host test asserted the wrong offsets as if they were Linux's |

| 13.12 | aarch64's **real Alpine userland**: `xtask` installs a genuine `aarch64` Alpine tree (`apk --arch aarch64` under `fakeroot`) into the machine's bare ext4, the new `rootfs-aarch64/` overlay carries the `/init` that mounts `/proc`, `/sys`, `/tmp`, `/run` and `devpts`, `enable_services` turns the same runlevels on, and `cargo xtask alpine-aarch64` boots it headless and reads the verdict off the PL011 | the boot reaches Alpine's own `getty` prompt (`login:`) — busybox init on `/etc/inittab`, openrc running its runlevels, both at EL0 on this kernel | **landed** — the machine boots a distribution, not a payload: `Welcome to Alpine Linux 3.25.0_alpha…` and `localhost login:` on the wire, `cargo xtask alpine-aarch64` exit 0. `apk --arch aarch64` installs the real tree (measured: `alpine-base`+`openrc`, 27 packages, 13.2 MiB — the host *can* unpack a foreign tree, it just cannot run the packages' post-install scripts, which is why `--no-scripts` and why busybox's applet farm is made at first boot by `rootfs-aarch64/init`). **Three real defects came out of it.** (1) **`USER_LIMIT` was x86_64's `1<<47` on a machine whose user span is `1<<48`** — the follow-on §9.1 had *named* and not taken: `MMAP_BASE` here is `0xff00_0000_0000`, which is **above** `1<<47`, so the `mmap` cursor hands out addresses `handle_mmap` then called kernel territory and answered every `MAP_FIXED` mapping at one with `EINVAL`. That is exactly the mapping a dynamic loader makes for a shared library's segments, so `openrc` died the moment it loaded `librc.so.1`: `Error loading shared library librc.so.1: Invalid argument` — while every *static* binary, whose mappings are never fixed, had run fine, which is why nothing had found it. The bound is now the machine's (`mm::elf::USER_LIMIT`, declared beside `MMAP_BASE` with both values and the reason). (2) **`/proc` and `/sys` were behind a module-level `target_arch` gate** (`vfs/mod.rs`) whose deferral was over — a real init mounts and reads both, and `mount -t proc` answered `ENODEV`; un-gating them cost **9** errors, all of them the two leaves that genuinely are x86_64's, now behind `mpos_pci_config` and `mpos_input_registry` capability flags (with `net::collect_net_sockets` moving to the same "both machines" condition its `procfs` caller now has). (3) **`boot::set_cmdline` was never called on this machine** — `/proc/cmdline` was empty, where a userspace init reads `root=`; and the handoff's console *injection* (the `echo-test` line its read/echo pin needs) was unconditional, which a real getty would read as a username. The command line is now recorded, and the injection is gated on a `mpos.selftest` word the test boots carry and the Alpine boot does not. Also: the console tty's node is the **machine's** serial name (`ttyAMA0` here, `ttyS0` on x86_64 — one node either way, named the way the psABI names the port; `/dev/ttyS0` on a machine whose serial port is a PL011 was the wrong spelling). And the Alpine boot carries **no initramfs** at all — the disk root is the whole userland — so the handoff learned that a missing archive is a disk-root boot (it had halted: "nothing to hand off to") while still refusing a machine with no root. Two build facts, both measured: `fakeroot` must wrap **`mkfs.ext4` as well as `apk`** (`mke2fs -d` stats each file, so an unwrapped `mkfs` writes a root filesystem owned by the build uid — the first image's `/etc/shadow` was `mp mp`, and a login that cannot read its own password file is what that becomes), and a stale `qemu-system-aarch64` holds a **write lock** on the raw images (the aarch64 wiring did not pass the `file.locking=off` the x86_64 one does, so an interrupted run made the *next* boot die at once with an empty serial log — a machine that never started, read as a machine that hung). Measured final: `cargo xtask alpine-aarch64` exit 0 on `localhost login:`; x86 gate `pass=151 fail=0 (151 built)` exit 0; aarch64 sweep `pass=21 fail=0` exit 0; `boot-aarch64` green through `INIT_EXITED`; host 511; depgraph 9/9 within ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none`/`aarch64-unknown-none`/`mpos-xtask`/the aarch64 loader. **Named, not fixed:** `tty1`..`tty4` cannot reach the wire — this machine has no console multiplexer, so the x86_64 `/etc/inittab`'s virtual-console gettys start and print nowhere, and exact parity there is the `vcmux` port (`vfs/vcmux.rs` names `drivers::{video::render,drm}`, so it waits on a display path too); `hwclock` and `modules` are x86-only services the shared `SERVICES` table still enables here (no `/dev/rtc` on this machine, no `/lib/modules`); the install writes no `/etc/hostname` from `rootfs-aarch64.toml`, so the prompt says `localhost`; and the machine's `net` is up but its `/proc/net` rows are the same table the first machine's is. **The third gap this boot named — `syslog` failing — turned out not to be a filesystem fact at all**, and it took three separate fixes to reach a running logger: the kernel's `open(2)` flag numbers being x86_64's (§13.13), and then a boot check leaving the wall clock at the year 36812 (§13.14) |

| 13.13 | `open(2)`'s flag **numbers** are the machine's: `O_DIRECTORY` and `O_NOFOLLOW` are declared per architecture (and `O_DIRECT`/`O_LARGEFILE`, the other two arm64 overrides of `asm-generic/fcntl.h`, are documented as the divergence), because arm64 gives them different bits from x86_64 | `user/tests/src/bin/t-open-flags.rs` on **both** machines: it drives `libc`'s own constants through this kernel - musl's `open()` wrapper (which adds `O_LARGEFILE`), `O_NOFOLLOW`'s refusal, `O_DIRECTORY`'s `ENOTDIR`, and `O_TMPFILE`'s pair being read as a pair - and fails on three of those checks against the old table | **landed** - the class of bug §13.12's boot exposed, and the reason the machine's *first dynamic library* could not load: arm64's `O_LARGEFILE` is `0x20000`, which is exactly x86_64's `O_NOFOLLOW`, so a kernel carrying one table answered `ELOOP` to every `open()` of a symbolic link on aarch64 - a "loop" whose target is a `stat`-visible regular file. Measured, not reasoned: the dispatcher was instrumented to print the number and arguments of any syscall answering `ELOOP` (one producer exists in the kernel, the `O_NOFOLLOW` branch of `open`), and the serial log gave `DIAG ELOOP: syscall 56 args [AT_FDCWD, ptr, 655360, 0, 0, 0]` - `openat` with `0xA0000` = `O_CLOEXEC|0x20000` - while the aarch64 `musl-dev` header named `0x20000` as `O_LARGEFILE` and the host's `bits/fcntl-linux.h` named it `__O_NOFOLLOW` for x86_64. Both tables were then read from the two packages' headers rather than from memory, and the values differ in exactly four flags. Why it survived until a distribution: musl's `open()` adds `O_LARGEFILE` to *every* open, so `open()` of a symlink was the whole of the symptom, and nothing on either machine had ever opened one through the syscall - `busybox` reaches `/bin/sh` through the kernel's own exec path, `readlink` is not `open`, and no earlier test opened a link. Why it is not aarch64-only in *shape*: the flag vocabulary is the ABI on both machines, so the pin runs on both and the x86_64 half is a regression pin for the table that was already right. Measured after: the Alpine boot's `ELOOP` is gone and the service it blocked now gets *past* its loader — `crond`, which declares `need syslog`, starts for the first time (`crond[7]: crond (busybox 1.38.0) started, log level 8`). `syslog` itself still does not come up, but for a *new* and named reason: its daemon takes an EL0 **write fault at address 0** (`user fault at 0x0 (ESR 0x92000046, pc 0xfe24dec3d914)`) — data abort from a lower EL, translation fault level 2, `WnR` set, `FAR` zero, at a pc inside the randomized interpreter/program band, three respawns with the same fault at three different pcs. That is a *different* gap and was named here rather than folded in — **§13.14 takes it**, and it was not in the loader either: the clock was wrong. Also measured: `cargo xtask alpine-aarch64` still reaches `localhost login:`, x86 gate `pass=152 fail=0 (152 built)` exit 0, aarch64 sweep `pass=22 fail=0` exit 0, host 512 |
| 13.14 | a boot check must leave the machine as it found it: `check_wallclock` (`M6_WALL_OK`, which runs on **every** aarch64 boot because this machine's stage checks *are* the boot's verdict - in front of the sweep, or of a distribution's `init`) proved `set_boot_epoch` moves the anchor by setting it to `1 << 40` and never restored it, so `clock_gettime(CLOCK_REALTIME)` answered `1099511627776` seconds for the rest of the boot | `user/tests/src/bin/t-localtime.rs` on **both** machines: it reads the anchor back as `CLOCK_REALTIME - CLOCK_MONOTONIC`, ranges every `struct tm` field `localtime_r` derives, measures the length of the timestamp through `asctime`'s own format, and only then makes the real `asctime_r` call - and fails on the old kernel at `the year is a live one (got 36812)` before dying in `asctime_r` | **landed** - the last of §13.12/§13.13's chain, and the third distinct cause behind one symptom (`syslog` not starting). Where §13.13 ended: `syslogd` now got past its loader but took an EL0 **write fault at address 0** (`ESR 0x92000046`, `WnR` set, translation fault level 2, `FAR` 0), three respawns at three different pcs. Instrumenting the dispatcher (a `comm`-filtered per-syscall trace, reverted after the run) showed syslogd matching real Linux call for call up to `openat("/etc/localtime")` = `ENOENT`, and then faulting; the harness fault dump gave the pc, and the process's mapping ranges placed it in `ld-musl-aarch64.so.1`, where the disassembly named the instruction: `strb wzr, [x0]` with `x0 = 0`, the `a_crash()` musl's `asctime_r` reaches when the formatted timestamp is longer than its 26-byte buffer. So the fault was not in the tz code at all: the *time* was wrong. `clock_gettime` was answering `1099511627776` seconds (`1 << 40`) - a year of `36812` - and `1900 + (1 << 40)` is exactly `asctime_r`'s `%d` overflowing 25 characters. The value was searchable to one line: `check_wallclock` saves the real anchor in `anchored`, sets the software anchor to `0x0000_0100_0000_0000` to prove the setter moves it, asserts it moved, and then printed `M6_WALL_OK` and returned - leaving the machine's clock at that value for everything that ran afterwards. Fixed by restoring the anchor and checking the restore, with the rule written beside it. Why nothing had caught it: no test in either suite had ever called `localtime`/`asctime`/`gmtime` (the clock was pinned, the calendar was not), and the check's own log line reports the *saved* epoch, so the boot log reads correct while the machine's clock is wrong - the worst shape of false evidence. Why it is a *class*: the aarch64 boot runs its milestone checks as the boot's verdict rather than under `mpos.selftest`, so they run in front of real userspace - a check that damages shared state (this anchor, a TLS pointer, a mapped page) damages the machine it is checking. Measured after: aarch64 sweep `pass=23 fail=0` exit 0; x86 gate `pass=153 fail=0 (153 built)` exit 0; `cargo xtask alpine-aarch64` exit 0 with **`syslog` running**: `/var/log/messages` in the image carries `Oct  5 07:26:46 localhost syslog.info syslogd started: BusyBox v1.38.0` followed by every `init: starting pid …` and `crond … started` line - written *through* the daemon, which is the strongest form the evidence can take; `boot-aarch64` green through `INIT_EXITED`; host 512; xtask 55; depgraph 9/9 within ratchet; fmt clean; clippy 0 errors on both bare-metal targets, the host lib, `mpos-xtask` and the aarch64 loader |
| 13.15 | the second machine's `hwclock` and `modules` - §13.12's last two named services - were x86_64's twice over: the RTC device node and its ioctls were behind a `target_arch = "x86_64"` gate while the thing behind the node was already `crate::hal`'s, and the PL031 the second machine's node would rest on was recorded as a read-only counter, so `hal::write_epoch` answered `false` and `RTC_SET_TIME` had nothing to write. The node registers on both machines, the ioctls are served on both, `write_epoch` loads the counter and verifies it, and the aarch64 install writes the module index the `modules` service asks `modprobe` about | `user/tests/src/bin/t-rtc.rs` on **both** machines: `/dev/rtc0` and `/dev/rtc` open, `RTC_RD_TIME`'s nine fields convert back to the second `clock_gettime(CLOCK_REALTIME)` reports and match `gmtime_r`'s breakdown of that second field for field, `RTC_SET_TIME` is served (exercised with the value just read, so the check cannot move the machine's clock), a month no calendar carries is `EINVAL`, and the family's unimplemented half and a request aimed at another descriptor are both `ENOTTY` | **landed** - two mechanisms, one gap: OpenRC's `hwclock` walks `/dev/rtc` and `/dev/rtc[0-9]*`, found neither on the second machine, could not set the system clock, and reported `[ !! ]` on every boot; the `modprobe rtc-cmos`/`rtc`/`genrtc` fallback that guarded is the same `rtc_exists` test, so the missing node was also the source of that noise. The gate was the only x86-shaped thing in the path - `drivers/devices.rs` registered the node on one machine, `drivers/mod.rs` compiled `rtc_dev` on one, and `syscall/linux/fs/ioctl.rs` served the `RTC_*` requests on one - and what the node reads and writes is `crate::hal::WallClock`, which both machines implement. The second half came from re-measuring rather than trusting the note: this module's own doc recorded that the counter was read-only ("a naive write left both `RTC_DR` and `RTC_LR` reading the lock key"), and the write that was tried was `RTC_DR` - the counter's *read* door. Loading `RTC_LR` (`0x08`) moves the offset `DR` is derived from: measured on the machine, writing `0x6ac360c7` left `DR` reading `0x6ac360c7`, and the boot check now writes a year no boot would read (1979), reads it back *from the device*, refuses a value past the counter's 32 bits, and puts the anchor back - the same leave-it-as-you-found-it rule §13.14 landed next door; the boot line says so (`the counter loads and reads back, so write_epoch persists`). The `modules` half is a one-line consequence: `write_modules` was already there for x86_64 and only needed calling from the aarch64 install, since `uname -r` is one number on both machines and the names in `/etc/modules` (`af_packet`, `ipv6`) are one kernel's code. Also corrected while measuring it: that same ioctl arm's comment claimed the kernel's header spells the RTC commands `'t'`; the shipped `linux/rtc.h` spells all twenty-four of them `'p'` and none with `'t'` (measured: `RTC_RD_TIME` = `_IOR('p', 0x09, struct rtc_time)` = `0x80247009`, `RTC_SET_TIME` = `_IOW('p', 0x0a, …)` = `0x4024700a`, struct 36 bytes - which is what the pin spells and what Alpine's `hwclock` sends). Measured after: aarch64 sweep `pass=24 fail=0` exit 0; x86 gate `pass=154 fail=0 (154 built)` exit 0; `cargo xtask alpine-aarch64` exit 0 with `Setting system clock using the hardware clock [UTC] ... [ ok ]` and `Loading modules ... [ ok ]`, **no `[ !! ]` line and no `modprobe` message anywhere in the boot**; `boot-aarch64` green through `INIT_EXITED`; host 512; xtask 55; depgraph 9/9 within ratchet; fmt clean; clippy 0 errors on both bare-metal targets, the host lib, `mpos-xtask` and the aarch64 loader |

**What the remaining 122 are, and why the count stops being the metric** (a
`target_arch`-line count of `src/` outside `src/arch/`, measured 2026-10-04 after §13.1
and §13.2's slices. The audit this table was built from was a *per-file* one taken at
**169**, so the two numbers are not the same measurement: what the table classifies is
the *kinds* of site, and the per-file drops recorded in §13.2 are what the count has
followed since - `sched`'s ptrace/stop block 23 → 0, `proc.rs` 23 → 18,
`syscall/linux/mod.rs` 11 → 9, then `proc.rs` 18 → 17 with `syscall/linux/mod.rs`
9 → 8 when `rt_sigtimedwait` left the same class, and then the register-ABI stage moved
the gate frame's register file into `arch/` behind `hal::Gate` (`proc.rs` 17 → 7,
`syscall/linux/mod.rs` 8 → 7, `syscall/mod.rs` 3 → 1), and then the signal frame's
layout went the same way behind `mpos_signal_frame` (`proc.rs` 7 → 3,
`syscall/linux/mod.rs` 7 → 5, `syscall/mod.rs` 1 → 0)):

| Kind | Sites | Where | What it actually needs |
|---|---|---|---|
| Per-arch Linux ABI and not-yet-ported handlers | ~41 | `syscall` (`ioctl` 18, `file` 9, `mod` 5, `proc` 3, `nr` 2, `fs/mod` 2, `abi` 2) — the `sched` half of this row (`runtime.rs` 22 + `mod.rs` 1) is **done** (§13.2), and so are the bulk-memory pair, `rt_sigtimedwait`, the **gate frame's register file** (Stage 1) and the **signal frame's layout** (Stage 2) | §11.4's S4 extended to the **syscall gate frame**: move `RegsFrame` and its conversions (`capture_frame`/`apply_frame`/`user_ip`) into `arch/x86_64/`, exposed through `hal` as the operations the ABI layer needs (`to_regs`/`from_regs`/`ip`/`sp`/`set_exec_return`). Not a flag: this *is* the ABI's shape, and the thread-pointer/TLS and ptrace arms go with it. The scheduler's half landed the other way and needed no `arch/<arch>/` move: it carries the register file *by value* and never reads a field, so its width became a machine fact (`hal::Contexts::ABI_REG_COUNT`) rather than a per-arch code path, and its protocol words went to the `abi` leaf. The signal frame's layout went the same way next (Stage 2): `hal::SignalFrame`'s `enter`/`exit` over a `SignalDelivery` value shape, the x86 `rt_sigframe`/`sigcontext` bytes in `src/arch/x86_64/signal.rs`, and `build_frame`/`handle_rt_sigreturn` reduced to policy plus the two scheduler calls the machine cannot make — so `RegsFrame` is gone and the class is closed. The gate frame's *register file* went the same way as the scheduler's, and needed no layout move of its own: `hal::Gate` grew `user_context`/`capture_user_regs`/`restore_user_regs`/`syscall_args`, `proc.rs`'s `capture_frame`/`apply_frame` became forwards to them, the now-neutral `syscall_entry_stop`/`syscall_exit_stop`/`tell_tracer_of_child`/exec-trap/fork/clone sites lost their gates, and the aarch64 twin of `user_context` moved into `arch/aarch64/` (Stage 1) |
| Port-progress ledger | ~43 | `drivers` 36 (the x86-only submodules, the PCI virtio transports) · `vfs` 7 (`p9`/`procfs`/`sysfs`/`vcmux`, `tty`'s KD/VT imports) | Nothing here: it disappears when aarch64 parity lands (§13.3–13.5). Renaming to `mpos_driver_*` would spell the same architecture knowledge differently and outlive the port |
| Machine identity | 4 | `mm/elf.rs` (the `e_machine` this kernel is) · `syscall`'s `uname` machine name | Cannot go through `hal`: the host-compiled `mm`/`syscall` need the answer too and `hal` is bare-metal-only, so `cfg!`/`#[cfg]` is the right tool for an identity, not a leak |
| Stale gates, unpinnable as they stand | 8 | `logging` 6 · `mm/heap.rs` 2 | Both reasons are void (aarch64 has a console and a clock since stages 4a/1c), but `src/` emits **no `tracing` event at all** (measured: no `info!`/`warn!`/`error!`/`debug!`/`trace!` outside `logging.rs`), so deleting the subscriber's gate produces code nothing exercises; the heap prints are on the OOM path no test reaches. A landed behaviour needs a pin (AGENTS §3 rule 6), so each is a finding with a missing prerequisite, not a rename. **The stale-gate class is wider than this row and splits in two.** Nine more have lived in `syscall`'s `ifreq` arms, `fs/mod.rs`'s nonblocking push, the `process_vm_readv`/`writev` pair and `rt_sigtimedwait` - gated as "the first machine's" although nothing in them named it (the first six when `net` was the first machine's alone, the last three merely grouped with `ptrace`, which *is* per-arch) - and all nine *were* drivable, so they were deleted and pinned instead (`user/tests/src/bin/t-ifreq.rs`, `t-process-vm-libc.rs`, `t-rt-sigtimedwait.rs`); what makes the eight here different is that nothing can drive them yet, which is the missing prerequisite, not the gate |
| Allow-listed by design | `lib.rs` 17 · `main` 9 · `boot/mod.rs` 1 | the target-gating ledger and the per-arch bin arms | — |

**Acceptance** (both threads together): the aarch64 gate is quotable with a summary line and exit
code; `src/hal` names no architecture; `src/arch/` outside `lib.rs`'s module gates names no
architecture; `cargo xtask depgraph` stays green with the `arch`-naming allow-list at zero. One
deliberate exception stays named and is not work: the Linux register ABI is per-arch by definition
and lives in `arch/<arch>/` (§11.4) — "no leak" means a port writes `arch/<arch>/` and edits no
shared file, not that a register frame becomes neutral.

**Where this lands**: status in §9.1 (parity) and §11.4 (purity); `ARCHITECTURE.md` §2/§3 in the
same commit as any new or moved edge (AGENTS §3 rule 5), *before* the edge, not after. **13.1 and
13.3–13.11 have landed**, and 13.2 is in progress — the parity thread of this phase is complete: the
second machine boots its own disk root, runs the same network stack and socket syscalls over its own
adapter, and carries the `/bin` its spawn tests need, so the sweep is green on **everything it is
supposed to serve** and `tests.toml`'s `aarch64` group is empty. What a red aarch64 sweep means is
now entirely the kernel's. The board's `9.1` line above is stale where it says "stages 3-8 remain" —
§9.1's own status list is the truth.

---

## Phase 14: `arch` becomes a crate — the machine seam, enforced by the compiler

**Goal** (one sentence): `--arch x86_64|aarch64` switches the machine; `arch` and the HAL are one
crate `mpos-arch` that depends on nothing in the kernel; every other module is written once.

**Why a crate and not the ratchet we already have.** `cargo xtask depgraph` (§11.4) already fails
on a module outside `arch`/`hal`/`main`/`lib` naming `crate::arch::`, and `src/hal` already carries
zero `target_arch` gates (§13.1). Both are *measured by a tool this repository wrote*. A crate
boundary is the same rule enforced by rustc, and it is the only version of it a port cannot
accidentally edit around. Everything below is the work of getting to that boundary; none of it
changes what the HAL abstracts.

### 14.0 What was already true, and the one gap that was not

| Claim | State |
|---|---|
| `--arch` selects the machine | **Was not true.** The switch reached `run`, `test` and `boot-aarch64` only; `cargo xtask build --arch aarch64` compiled the x86_64 kernel and reported success, because `utils::build_kernel` never passed `--target` and the loader's target came from `boot/.cargo/config.toml`'s `[build] target`. `boot-aarch64` carried its own copy of both triples for exactly that reason. **Fixed** (S0 below). |
| `hal` is architecture-neutral | True since §13.1: capability traits, `mpos_*` cfg flags, no `dyn` on the downward path. |
| A new port writes `arch/<arch>/` and edits no shared file | True in practice for both ports. |

### 14.1 The whole obstacle: 128 upward references out of `src/arch/**`

Measured (`crate::` in `src/arch/**`, excluding `crate::hal`/`crate::arch`, comments excluded), after
S1a. A crate boundary turns every one of these into a compile error, so each is either **moved down**
(a leaf crate both sides depend on), **moved out** (the caller belongs above the machine layer), or
**inverted** (the machine declares the service and the kernel registers it).

| Kind | Sites | Disposition |
|---|---|---|
| `sched` (fault resolvers, `exit`, `preempt`, `current`, signals) | ~30 | **invert** — hook table |
| `println!` / logging | 23 | **invert** — hook table (a log line), or move the checking code out |
| `mm` (`KernelSections`, `MapFlags`, `Mapping`, `PageTaken`, `PAGE_SIZE`, frame allocation, `map_mmio`) | ~25 | **move down** the four vocabulary types; **invert** frame allocation and the dynamic MMIO window |
| `drivers` (`serial` 9, `tx_ring` 8, `virtio` 2, `qemu` 4) | 23 | **invert** — the console sink and the transmit ring are kernel services; `qemu`'s debug exit is a hook |
| `boot` (`BootInfo` 5, `handoff` 2, `set_cmdline` 1) | 8 | **move down** the record (and it is the same record `boot/src/handoff.rs` duplicates — S3 closes that too); `set_cmdline` inverted |
| `err`, `date`, `fdt` | 5 | **already leaf crates** — `mpos-arch` depends on them directly, no work |
| `syscall::linux_dispatch`, `vfs::vcmux`, `net::irq_service`, `console::force_serial` | 5 | **invert** — hook table |

### 14.2 The inversion: a static hook table, upward only

`mpos-arch` declares the kernel services it needs as one trait (or an equivalent struct of
`fn` pointers) and the kernel registers its implementation once at boot. The *downward* direction —
generic code asking the machine — stays exactly as it is today: capability traits, one `impl`, no
`dyn`, monomorphised. That asymmetry is deliberate and is the reason this is a table and not a
generic parameter:

- every upward call is on a slow path (a fault, a tick, a log line, a page-table frame), so an
  indirect call costs nothing measurable;
- `Machine<K: Kernel>` would have to thread `K` through every arch type, and the exception entry
  points reached from `global_asm!` **cannot be generic** — each would need a `#[no_mangle]` shim
  the kernel defines and the arch crate's assembly branches to, i.e. a symbol the machine layer
  references but does not own.

### 14.3 Stages

- [x] **S0 — `--arch` is one switch.** `xtask/src/arch.rs`: `Arch` carries the target triples, and
      `Arch::parse` refuses a machine this front end does not build. `build_kernel(profile, arch)`
      passes both `--target`s; `boot-aarch64`'s private kernel/loader build (and its two target
      constants) are gone. The `--arch` word now also crosses the sudo boundary, since the
      privileged half re-parses it. `initramfs`/`disk`/`rootfs`/`all` refuse `--arch aarch64` by
      name — they assemble *the x86_64 disk*, and the second machine's disk is laid out by
      `boot-aarch64` / `alpine-aarch64` / `run --arch aarch64`; building an x86_64 root under an
      aarch64 banner is the silent-wrong-machine failure `AGENTS.md` §3 refuses.
      Measured: `cargo xtask build` -> `target/x86_64-unknown-none`, `--arch aarch64` ->
      `target/aarch64-unknown-none` + `aarch64-unknown-uefi`; `cargo xtask boot-aarch64` still
      green through the shared builder (full marker chain, `INIT_EXITED`).
- [x] **S1a — aarch64 device discovery moves to `main`.** The virtio-MMIO identity constants,
      `read_device`, `virtio_device_windows` and the six device probes (`check_virtio`, `check_rng`,
      `check_blk`, `check_fs`, `check_net`, `check_input`) — 566 lines — moved from
      `src/arch/aarch64/mod.rs` to `src/board/aarch64.rs`, reached through one more pointer `boot`
      is handed (`devices: fn(&BootInfo)`), exactly as `milestone: fn()` already is. Opening a
      window as a virtio device, mounting a filesystem over it and registering it with the block
      layer is driver/filesystem/block work; a machine crate cannot name any of them. The one
      machine question in the moved code — *where is this physical window* — became
      **`hal::map_mmio_window`**, which on this machine is `paging::device_virt` and on the other is
      a fresh kernel-half mapping: the probes are now architecture-neutral code.
      Measured: upward refs 140 -> 128, `boot-aarch64` green with the identical marker chain.
- [ ] **S1b — the remaining `check_*` milestone probes.** `check_context`, `check_paging`,
      `check_timer`, `check_wallclock`, `check_cpu`, `check_serial_rx`, `check_tls`,
      `check_user_memory`, `check_paging_ops` (~1900 lines) still call `sched`/`mm`/`println!` from
      inside `arch`. **They can stay**: they are "does this machine's HAL work" checks and after S3
      they reach the scheduler through the hook table like everything else in `arch`. Moving them
      is optional tidiness, not a prerequisite — `check_el0` and `el0_returned` in particular own
      the EL0 entry and the handoff to `init`, and extracting those from the machine layer is a
      design job of its own. The reason S1a *was* a prerequisite is the opposite one: those probes
      were not testing the machine, they were driving devices.
- [x] **S2 — move the shared vocabulary down.** Two new leaves, both pure data and
      host-tested, both re-exported by the kernel under the names they already had so
      no caller changes:
      - `crates/mpos-bootinfo`: `BootInfo`, `MemRegion`, `MemKind`, `MAX_MEM_REGIONS`, and the
        wire record `Handoff`/`AbiRegion`/`KIND_*`/`HANDOFF_MAGIC`. `boot::handoff::collect`
        — the translation, which is the part that decides what a region *means* — stays in
        the kernel. `src/boot/mod.rs` and `src/boot/handoff.rs` re-export.
      - `crates/mpos-vm`: `Mapping`, `MapFlags`, `PageTaken`, `KernelSections`, the page
        arithmetic (`PAGE_SIZE`/`PAGE_SHIFT`/`align_up`/`align_down`) and the
        machine-independent `kernel_mappings` plan. `src/mm/vm.rs` and `src/mm/pframe.rs`
        re-export; the five tests that pin the plan moved with it.

      `err`/`date`/`fdt` were already leaves, so `mpos-arch` depends on them directly with
      no work. The two-places-to-sync problem with `boot/src/handoff.rs` is *not* closed
      here — the loader still carries its own copy of the record, and making it depend on
      `mpos-bootinfo` is a separate, self-contained cut (§14.3 S2-follow).

      **A gate hole this surfaced**: `run_unit_tests` named every leaf crate by hand, and
      `mpos-fdt` had never been added — eight tests, never run by any gate. The list is a
      const now and `selftest::tests::every_leaf_crate_is_tested` pins it against
      `crates/`, so a leaf added without a line there fails the xtask tests instead of
      quietly shrinking the sweep. Measured: the host count went 515 -> 510 + 5 (`mpos-vm`)
      + 8 (`mpos-fdt`), i.e. the same tests, now all inside the gate.
- [x] **S3 — build the hook table.** `src/hal/kernel.rs` declares [`Kernel`] — the
      services a machine's trap entry needs: `started`, `current`, the three fault
      resolvers, `kernel_lazy_accessible`, `send_signal`/`disposition_is_default`,
      `exit`, `preempt`, `charge_cpu`, `device_not_available`, `dispatch_syscall` —
      with one forward per method, so a handler reads `hal::kernel::resolve_cow_fault(addr)`
      and never names `sched`. `main/<arch>.rs` implements it and calls
      `hal::kernel::register` before the machine's vectors/IDT go in.

      **Slice 1 landed**: every `crate::sched::*` and `crate::syscall::*` site in
      `src/arch/**` (37 of them) now goes through it, on both machines. Upward refs
      **128 -> 89**. Measured: `boot-aarch64` green with the identical marker chain,
      `test --arch aarch64` 25/25, and the x86_64 sweep unchanged at 154/155 — the
      `#PF`, timer and syscall paths it converted are the ones that sweep leans on
      hardest, so that number is the evidence the indirection costs nothing.

      `device_not_available` is gated on `mpos_fpu_trap` inside the trait, the way
      every other optional capability is: aarch64 has no `#NM` to fault on, so the
      method does not exist for it rather than being a stub it must fake.

      **Slice 2 landed — the machine layer now names no kernel module at all.**
      Measured: `crate::` references in `src/arch/**` *and* `src/hal/**` that are
      neither `crate::hal` nor `crate::arch` are **89 -> 0**. What that took, by
      kind, and what each answer actually was:

      - **48 sites that were never hooks.** `mm::phys_to_virt` and `mm::hhdm_offset`
        are the loader's *direct map*, not a kernel service: the loader chose the
        offset and the loader mapped physical memory at it. They are now
        `hal::phys_to_virt` / `hal::direct_map_offset` / `hal::virt_to_phys`, backed
        by one `hal` static that `mm::init` fills from the boot record. This is a
        *downward* call — no `dyn`, no indirect branch — so the forty-odd page-table
        walks pay nothing for the move. `mm`'s three helpers stayed as forwards.
      - **The hook table's second half.** `print_line` (+ a `kprintln!` macro),
        `force_serial`, `route_console_input`, `service_network`, `set_cmdline`,
        `boot_info`, `start_memory`, `alloc_frames`/`free_frames`/`frames_free`, and
        `wake`. The batch's one trap: the macro's newline delimiter cannot be a
        `concat!`, because an implicit named argument (`"{rsp:#x}"`, which dozens of
        the fault reports use) is only captured from a *string literal* and
        `concat!`'s expansion is a macro call. `format_args!("{}\n", format_args!(…))`
        is the nested form that works.
      - **Three relocations, because the call was not an inversion.** `tx_ring` (the
        console's transmit queue) is machine-*neutral* — parameterised by a
        `Transmitter` and knowing no register — so it moved to
        `src/hal/tx_ring.rs`, beside the `SerialRing` trait it implements. `serial`
        (the 16550) and `qemu` (the `isa-debug-exit` port) are one machine's own
        hardware and moved to `src/arch/x86_64/`. `drivers/` no longer declares any
        of the three.
      - **`qemu` became a capability, not a path.** `hal::TestExit` +
        `mpos_qemu_debug_exit` + `hal::exit_qemu(code)`, with `TEST_EXIT_OK` /
        `TEST_EXIT_PANIC` beside it because those codes are the *harness's*
        contract. A machine without the flag halts instead — which is what the
        second machine's `reboot(2)` stop path now gets, honestly, rather than a
        second body of code.
      - **The per-BAR MMIO window moved out of `mm`.** `mm::map_mmio` (the x86
        bump allocator behind `hal::map_mmio_window`) is `arch/x86_64/mmio.rs`; the
        `mpos_dynamic_mmio_window` flag is **deleted**, because it described a
        *mechanism* and a mechanism is exactly what a capability flag must not name.
        `ahci`, `virtio::msix` and the local APIC now call `hal::map_mmio_window`.
      - **`virtio-rng` left the machine layer.** Both machines' `Entropy::mix_into`
        folded in the rng device; the device is a driver the kernel enumerates, not
        an answer the machine gives, so `virtio::entropy()` names the transport and
        `crate::rand` folds it in. aarch64's `mix_into` is now *empty* and says so,
        which is the honest answer for a core with no `RDRAND`.
      - **One name for both machines' console arming.** `board/x86_64.rs` no longer
        names `drivers::serial`: `arch::enable_console_interrupts` is now the
        x86_64 twin of the aarch64 re-export of the same name.

      Measured: both kernels build warning-free (for what this change touched),
      `boot-aarch64` green with the identical marker chain, module graph within the
      ratchet (no edge appeared, no count grew, 9/9 mutual pairs).

      [`Kernel`]: ../src/hal/kernel.rs
- [x] **S4 — the move itself.** `arch/` is a workspace-root crate (`mpos-arch`, **not** under
      `crates/` - that directory means "host-testable leaf" and `xtask`'s
      `every_leaf_crate_is_tested` pins it against the host sweep, which a crate of `cli`/`sti`,
      MSRs and descriptor tables can never join). `src/hal/` and `src/arch/` moved to
      `arch/src/{hal,arch}/`, so the module paths *inside* are exactly what they were
      (`crate::hal::…`, `crate::arch::x86_64::…`) and the whole body moved unchanged.
      `src/arch` and `src/hal` are gone.

      **The kernel kept its names.** `src/lib.rs` has `pub use mpos_arch::{arch, hal};` - the same
      treatment the leaf crates got in S2. That is what makes the thousands of words of commentary
      above every `crate::hal::` call site still true rather than merely nearby, and it is why this
      step is a move and not a 139-site rename. What the crate boundary bought is not a different
      spelling in `mpos`; it is that `mpos-arch` has no `crate::sched`, `crate::mm` or
      `crate::println!` *in scope*, so the machine layer's independence is cargo's business now.

      Three things followed the code, and one did not:

      - `.cargo/config.toml`'s two `-C link-arg=--script=` paths point at
        `arch/src/arch/<arch>/linker.ld`. (They are passed to every crate built for the target and
        ignored by the ones with no link step; only the kernel binary links.)
      - `feature(abi_x86_interrupt)` went to `arch/src/lib.rs` with the `extern "x86-interrupt"`
        handlers it exists for, and the kernel's crate root dropped it - "declared but not used" was
        the warning that said the last machine body had left.
      - The `mpos_*` capability vocabulary became `[workspace.lints.rust]` with `[lints] workspace =
        true` in every member, because a cargo lint table is per-package and the second copy - the
        one `mpos-arch` needs - is the copy whose staleness would silently stop a warning.
        (`missing_lints_inheritance` then fires once per manifest that does not opt in, which is why
        every member opts in; two members had an empty `[lints.cargo]` placeholder that cargo
        rejects alongside `workspace = true` and that did nothing, so they are gone.)
      - The capability *flags* stayed global: `.cargo/config.toml`'s `[target.…]` blocks are
        per-target, not per-package, and the kernel reads `mpos_net`, `mpos_signal_frame`,
        `mpos_framebuffer_console` and `mpos_serial_ring` itself.
      - And the *dependencies* found their own places, because cargo reports `unused dependency`
        per target: `mpos-fdt` and `mpos-err` are aarch64's (the device tree, and the errno an
        unhandled `svc` answers with), `mpos-date` is x86_64's (the CMOS clock's calendar
        arithmetic), and only `mpos-vm`, `mpos-bootinfo` and `critical-section` are the seam's own.
        Written as one flat list first - which is what a move that just wants to compile produces -
        and then told apart by the tool.

      **The ratchet had to change two things, and one of them was a bug.** `ARCH_EXEMPT` drops to
      `["main", "lib"]`: `arch` no longer exists as a module of this crate, so the rule stopped
      being "the machine layer may name itself" and became "only the crate root and the wiring may
      reach for the machine at all". And the mutual-pair count went **9 → 8** - but not by the one
      that was expected:

      - `hal ↔ mm` collapsed with the move, as it should (the mapping vocabulary is a leaf path
        now, so `hal → mm` is zero while `mm → hal` remains, one-way).
      - `drivers ↔ rand` **arrived in the previous slice** (`NOTES.md` §14.3 S3, slice 2) and was
        *cancelling* that removal exactly. Folding the virtio-rng into `rand`'s seed is what let the
        machine layer stop naming a driver - but `rand → drivers` closed a cycle with `drivers →
        rand` (`/dev/urandom` reads the pool), so one pair left and another took its place while the
        count stood still. That is the plainest demonstration of what this ratchet cannot see, and
        the fix is the project's usual one: the source is now *injected*
        (`rand::set_device_entropy`, called from `main/<arch>.rs`), so `rand` names no driver.

      A fourth thing was found on the way and is filed here rather than fixed quietly:
      `rand::tests::a_reseed_re_keys_the_generator` was **flaky** - five tests over one global
      generator, run on several threads, so a neighbour's re-key made "a passive read must not
      advance the key" false. Measured 1 failure in 4 parallel runs, 0 in 4 serial ones; the tests
      now take a lock for the length of each body (0 in 6 after). It predates this step, and it is
      here because the gate runs it: a quarter of runs red is how a suite stops being believed.
- [x] **S5 — verify both machines.** Measured after S4: `cargo xtask boot-aarch64` green with the
      identical marker chain and `INIT_EXITED` (which is the evidence that `global_asm!`'s vector
      table and `critical-section`'s `set_impl!` survive being linked out of an rlib);
      `cargo xtask test --arch aarch64` **25/25**; `cargo xtask test` **154/155**, the one failure
      being `t-unix`, which was red before this work began and is not this work's; host unit tests
      **510**; `cargo xtask depgraph` within the ratchet with `mutual pairs 8/8`; xtask's own
      **65**.

### 14.4 What deliberately does not move

The Linux register ABI (`user_regs_struct`/`user_pt_regs`, the signal frame, the syscall gate) stays
in `arch/<arch>/`: it *is* the machine's register layout, and wrapping it in a neutral type would be
the private dialect `AGENTS.md` §4 rule 3 forbids. "No leakage" means a port writes `arch/<arch>/`
and edits no shared file — not that a register frame becomes architecture-neutral.

### 14.5 Reading the paths in this document

Every section above (and the stage ledgers in §9.1, and `ARCHITECTURE.md`) names the files as they
stood *when it was written*, and S4 moved two directories without rewriting them. The translation,
for every path that starts with one of these prefixes:

| Written as | Means, after S4 |
|---|---|
| `src/hal/…` | `arch/src/hal/…` |
| `src/arch/…` | `arch/src/arch/…` |
| `src/arch/x86_64/serial.rs`, `qemu.rs`, `mmio.rs` | the same under `arch/src/`, and the first two came from `src/drivers/` in S3 |

The history is left as history on purpose: a stage ledger that has been rewritten to match the tree
can no longer be used to tell what a stage actually did. This note is the single place the mapping
is stated, so a reader has one hop to make instead of a search.

### 14.6 The loader's half of the machine layer

`mpos-boot` was the last thing in the tree that named an architecture - 20 `target_arch` sites in
three groups, which its own file header listed:

| What | Was | Now |
|---|---|---|
| the progress lines' wire | `main.rs`'s `mod serial`, one `imp` per machine | `mpos_arch::early::{serial_init, serial_write}` |
| the page-table descriptor | `boot/src/vm/{x86_64,aarch64}.rs` | `mpos_arch::early::{PageTables, Frames}` |
| the last instruction | two `unsafe fn enter` bodies in `main.rs` | `mpos_arch::early::enter` |

**Criterion, measured**: `grep -r target_arch boot/src/` is **0**, and `boot → mpos` (the kernel)
is still absent from `cargo xtask depgraph`. The only machine-dependent thing left in the loader is
the target triple its files are compiled for.

**One deviation from §14.3's plan, and why.** The stage list said `arch/src/arch/<arch>/early.rs`.
It landed at `arch/src/early/{mod,x86_64,aarch64}.rs` instead, one level up, because
`arch/src/arch/**` is the *kernel's* machine layer and is gated `#[cfg(target_os = "none")]` while
the loader is `{x86_64,aarch64}-unknown-uefi`. Putting `early` inside that tree would have meant
gating most of `arch/mod.rs`'s re-exports on the target's operating system, which is a larger and
more fragile change than the one this stage is. So the loader's machine code is a sibling tree -
`arch/src/early/` beside `arch/src/{arch,hal}/` - and `arch/src/lib.rs` gains the crate's only
`#[cfg(target_os = "uefi")]`. When the kernel becomes a UEFI stub itself (item 5 of
`docs/TEST-AND-BOOT-REFACTOR.md`) that gate widens to `any(…)` and nothing else moves.

**`Frames`: who decides what.** The loader decides where frames come from - one contiguous
`LOADER_DATA` allocation, sized from RAM before the first frame is taken, never returned - and
`early` decides what is written into them. That seam is a one-method trait, `PageTables<F: Frames>`,
implemented by the loader's own `Pool`. Monomorphised, so the narrowing costs nothing at run time
and needs no `dyn`; and the builder *cannot* allocate anything the loader did not hand it, which is
a property the old arrangement could only state in prose.

**`set_log`: who owns the format.** The loader owns the text (`mpos-boot: ` and a newline) and
`early` owns the wire, so `early` registers a sink (`mpos_arch::early::set_log`) and its own two
lines - the device window it maps, and the configuration-table GUIDs when no tree is found - come
out through the loader's formatter. Same shape as `hal::kernel`'s registration, same reason: the
layer with the facts is not the layer that owns the presentation.

**Two things deliberately did *not* go down**, and the distinction is the point:

- `copy_device_tree` stays in `boot`. Reading the firmware's configuration table, taking the tree's
  self-declared length and copying the bytes into frames is UEFI work, not machine work - and it is
  the same code on both machines. The machine answers one boolean, `FIRMWARE_PUBLISHES_DEVICE_TREE`
  (false on x86_64, whose firmware describes it with ACPI), and the loader asks it by name instead
  of hiding the branch in a `#[cfg]`. That is also why `DTB_GUID`/`FDT_MAGIC`/`FDT_MAX_BYTES` are
  now unconditional constants: they were never per-architecture facts, they just happened to be
  compiled on one machine.
- `Pool`, the three ranges, `merge` and `pool_bytes_for` stay in `boot/src/vm.rs`, which now
  contains no `cfg` at all: the *plan* is architecture-free, and only the encoding is the machine's.

**A dependency that had to move with the code**: `mpos-arch`'s `critical-section` became a
`[target.'cfg(target_os = "none")'.dependencies]` entry. `hal::tx_ring` is its only user and `hal`
is not built for the loader, so leaving it unconditional made cargo report `unused dependency` for
both UEFI targets. An unused dependency is a dependency that will be wrong without anyone noticing,
which is exactly what that lint is for.

**Verification** (each of these is a command that was run):

| Gate | Result |
|---|---|
| `cargo build -p mpos-boot --target {x86_64,aarch64}-unknown-uefi` | both build, zero warnings |
| `cargo build -p mpos-arch --target {x86_64,aarch64}-unknown-uefi` | both build, zero warnings |
| `cargo xtask boot-aarch64` | full marker chain + `INIT_EXITED`, and the loader's own eleven progress lines byte-identical, including `device window 0x8000000..0xc000000 …` |
| `cargo xtask test` / `cargo xtask test --arch aarch64` | x86_64 155/155, aarch64 25/25 |
| `cargo xtask depgraph` | inside the ratchet, mutual pairs 8/8, no new edge |
| host unit tests | `mpos` 511 + every leaf crate |

`boot-aarch64` is the load-bearing one of those: it is the only gate that runs the loader's page
tables, its PL011 writer and its `enter()` as relocated code inside another crate, and the marker
chain it prints is unchanged character for character.

### 14.7 The loader becomes a crate, and the record one definition

Two smaller things landed before §14.8, and both are prerequisites of it.

`boot/` became a **library plus a thin `#[entry]` binary**: `boot/src/lib.rs` holds the whole
sequence as `mpos_boot::run()` and `boot/src/main.rs` is fifteen lines that call it. That is the
shape a second caller needs - and the kernel is the intended second caller (§14.8's successor is
item 5 of `docs/TEST-AND-BOOT-REFACTOR.md`, where the kernel takes the loader's place).

`boot/src/handoff.rs` was **deleted**. The loader now depends on the `mpos-bootinfo` leaf and reads
the same `Handoff` the kernel does, which closes risk **V-10** (`docs/ARCHITECTURE.md`): the two
copies kept honest by a magic number are one definition, read by both sides of the seam. Two type
aliases (`AbiRegion as Region`, `HANDOFF_MAGIC as MAGIC`) left every call site untouched.

| Gate | Result |
|---|---|
| `cargo build -p mpos-boot --target {x86_64,aarch64}-unknown-uefi` | both build, zero warnings |
| `cargo xtask boot-aarch64` | full marker chain, the loader's eleven progress lines byte-identical |
| `cargo test -p mpos-bootinfo --target x86_64-unknown-linux-gnu` | passes |

### 14.8 The kernel's entry is one file, and the machine's half goes down

**What was wrong.** `src/main.rs` was a 34-line dispatcher whose whole body was a `#[path]` choice
between two bodies, and each of those bodies re-stated the *machine's* half of the boot - the entry
register, SSE, the record, physical memory, the descriptor tables, the page tables, the wire -
before getting to the kernel's. Worse, the same seam already existed for one machine and not the
other: aarch64 had been given `mpos_arch::arch::aarch64::boot(handoff, milestone, devices, init)`,
and x86_64 inlined the equivalent. So the architecture-independent part of the entry was written
twice, and the machine layer's own entry point was written once.

**What changed.**

| File | What |
|---|---|
| `src/main.rs` | now the one entry and the one panic handler. `_start` takes the record in the platform's first argument register (`RDI` vs `x0` is the compiler's business, not this file's), registers the kernel services, and hands `board::start` the record; `panic` writes one line through `mpos_arch::arch::print_args`, calls the machine's context dump, and stops. **No `#[cfg]`, no `#[path]`** |
| `src/board/mod.rs` | **new**: picks the machine's body in one place - exactly the arrangement `arch/src/arch/mod.rs` uses for the library - and exports `start` and `panic_context` |
| `src/board/x86_64.rs` | was `src/main/x86_64.rs`. Keeps the acceptance checks, the bus enumeration (`devices`), the console and banner (`milestone`), the kernel's own boot (`init`) and the panic context dump; loses the entry, `init_arch` and the panic handler |
| `src/board/aarch64.rs` | was `src/main/aarch64.rs`. Keeps the device probes, the EL0 handoff and its checks; loses the entry and the panic handler |
| `arch/src/arch/x86_64/mod.rs` | gains `boot(handoff, milestone, devices, init)` and `print_args`: the x86_64 half of the seam, symmetric to the aarch64 one and built on the `hal::kernel` table §14.2 already put there |
| `arch/src/arch/mod.rs` | exports both under one name each, so `mpos_arch::arch::{boot, print_args}` resolve on either machine and the call site has no architecture in it |
| `arch/src/arch/aarch64/mod.rs` | `milestone` now takes the record: the command line in it decides *where* the kernel log goes (`console=`), which is why the first machine's counterpart reads it too |

**What moved down, and what could not.** The entry, SSE for user processes, the record, physical
memory, the descriptor tables, the kernel's page tables and the wire that reports them are now
`mpos-arch`'s on both machines - which is what "the machine layer is one crate" has to mean.
Everything else stayed in the kernel, and by construction rather than by taste: the milestone
checks, the device bring-up and the handoff all name `vfs`, `blk`, `net`, `drivers` or `sched`, and
those are names that do not exist in `mpos-arch` (§14's whole point). So the kernel-side per-machine
code is `src/board/<arch>.rs` - a body, not a second `main`.

**The one thing deliberately not unified, and why.** The two machines' boot *sequences* remain their
own, and so do their check suites. A shared sequence would have to reorder aarch64's milestones,
which are load-bearing and were each a bug once: `check_dev` must run after `vfs::tty::init`,
`check_console_rx` before the receiver is armed, `check_serial_tx` after it. Unifying the two suites
into one capability-driven one is a real change with a real payoff, but it changes what aarch64
*accepts*, so it belongs in its own stage rather than in the one that makes the entry one file.

**A log-order note.** The banner is now the kernel callback's first line (`milestone`), which runs
after the machine has read the record - so the three machine lines (`pmm:`, `image:`,
`page tables:`) precede it where they used to follow it. Nothing parses the order; the self-test
protocol reads `=== RUN/PASS/FAIL/SUMMARY`.

**Verification** (each of these is a command that was run):

| Gate | Result |
|---|---|
| `cargo build -p mpos --target x86_64-unknown-none` | builds; the two warnings are the pre-existing drift in `net/packet.rs` and `seccomp` |
| `cargo build -p mpos --target aarch64-unknown-none --release` | builds; only the pre-existing lib drift |
| `cargo build -p mpos-arch --target {x86_64,aarch64}-unknown-none` | both build |
| `cargo xtask test` | `=== SUMMARY pass=156 fail=0 (156 built)`, exit 33 |
| `cargo xtask boot-aarch64` | full marker chain, all twenty-two markers, `INIT_EXITED` |
| `cargo xtask depgraph` | inside the ratchet, mutual pairs 8/8, no new edge |
| host unit tests | `mpos` 511 passed |
| stale-path sweep | `grep -rn 'main/x86_64\.rs\|main/aarch64\.rs\|src/main/'` over `AGENTS.md docs/ src/ arch/` finds none |

`boot-aarch64` is the load-bearing one again: it is the only gate that runs the second machine's
`arch::boot` with three function pointers crossing the seam, and every marker it prints is
unchanged character for character.

### 14.9 The steps every machine's `init` runs are stated once

**What was wrong.** `board/x86_64.rs` and `board/aarch64.rs` each end in an `init` that hands the
machine to `/init`, and each one re-stated the same five steps. Some of the statements were
byte-identical - `vfs::file::init_standard(vfs::tty::init())`, the ramfs root over it, and
`assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1")` - and two more carried the
same *ordering contract* written twice: the console mux goes up before the receiver is armed, and
the first program must be pid 1. One of the two copies said so explicitly ("the order is the same
one and the same reason as x86_64's `vcmux::init(); serial::enable_rx()`"), which is the comment
that says the two should have been one function.

**What changed.**

| File | What |
|---|---|
| `src/board/steps.rs` | **new**: the five steps - `start_scheduler`, `mount_ramfs_root`, `unpack`, `arm_console`, `hand_to_init` - each carrying its contract in its own doc, and the module saying why the two `init`s are still two |
| `src/board/x86_64.rs` | `init` and `handoff_to_init` call the steps; the one-line `init_sched` wrapper is gone. `handoff_to_init` keeps what is its own: the boot-stack check at the deepest point, the exit-code discard, and the wait for the shell's child |
| `src/board/aarch64.rs` | `init` calls the steps in its own order; the read/`argv`/spawn trio it had inline is now one call |
| `src/board/mod.rs` | declares `mod steps;`; its doc says which half of a body is not the machine's |

**Why the steps are in the wiring crate and not the library.** `arm_console` is half machine
wiring - arming the receiver is `crate::arch::enable_console_interrupts` - and `ARCH_EXEMPT` in
`xtask/src/depgraph.rs` allows only `main`/`lib` to reach the machine layer, naming that very call
("arms its console interrupts") as wiring. The library's `boot` module was tried first and the
depgraph refused it for a second, independent reason: the steps need `crate::sched` and
`crate::vfs`, and `vfs` already names `boot` once (the command line behind `/proc/cmdline`), so
`boot` reaching back closed the graph's ninth mutual pair. The wiring crate sits above both, names
the machine by right, and adds no edge the ratchet can see.

**Why still not one function.** Only the five steps are shared; their *placement* is not. x86_64
arms the console after the spawn - nothing reads the wire until the shell is up - while aarch64 arms
it before `check_serial_tx`, which is a check *of* the armed path; x86_64 unpacks inside its
filesystem bring-up while aarch64 unpacks after its devices and switches root after that. A single
`run()` would have to take a list of hooks and reorder them, which is the two sequences wearing one
hat (§14.8's argument, still standing).

**Verification** (each of these was run, after the change):

| Gate | Result |
|---|---|
| `cargo xtask test` | `=== SUMMARY pass=156 fail=0 (156 built)`, exit 33 |
| `cargo xtask boot-aarch64` | all twenty-two markers, `INIT_EXITED` |
| `cargo xtask depgraph` | inside the ratchet, mutual pairs 8/8, no new edge |
| host unit tests | `mpos` 511, `mpos-xtask` 65 |
| both kernel targets | `x86_64-unknown-none` and `aarch64-unknown-none --release` build; warning counts unchanged from before the change |

The depgraph shaped this stage rather than merely checking it: the first placement of these steps
(`boot::init`) was rejected by it, and the rejection was right on both counts.

---

## Phase 15: One boot flow — aarch64 to the first machine's level

**Status: 15.1–15.6 landed, all four of 15.6's pins green. 15.1, 15.3, 15.4 and 15.5 and the init
seam unified - all four check families run on both machines, the reporting/exit-hook/net/cmdline/log-sink
copies are one each, and 15.2's six device classes have all landed (the input registry §13.24, the
host-driven console §13.25, the display adapter + framebuffer console §13.26, the sound card §13.27,
the vsock device §13.28, and the host share §13.29). 15.6, the initramfs flow, is landed:
`boot-aarch64` full chain + `INIT_EXITED`, `test --arch aarch64` `pass=27 fail=0`,
`cargo xtask test` `pass=158 fail=0 (158 built)` exit 33, `alpine-aarch64` reaching
`mpos login:` in 16 s, and `depgraph` within the ratchet. `### 15.6` below records what it cost.
The last named difference this machine carried from the first - `init_net` +
`virtio::spawn_service` behind `mpos.selftest` - is also closed, measured the same day.
Measured 2026-10-07.**

Phase 13 closed the *ABI* gap (13.3–13.16) and §13.19/§13.20 closed two of its consequences. What
is left is the *boot*: the second machine reaches the same userspace surface as the first, but it
gets there along its own path, and the parts of that path which are not about a bus, a UART or a
page-table format are written twice - once in `src/board/x86_64.rs`, once in `src/board/aarch64.rs`.
The goal of this phase is that the machine-independent half of the boot has **one** copy, so that
"which machine is this?" is answered by the machine layer and by nothing else.

### What is measured today

| What | x86_64 | aarch64 | Where it is answered |
|---|---|---|---|
| `check_mm` (allocator, frames, buddy merge, slab classes, heap/frame cross-check) | `board/checks/mm.rs` | **same** | machine-independent: `mm` is one module |
| `check_sched` + its five scenarios and helpers | `board/checks/sched.rs` | **same** | machine-independent: `sched` is one module |
| `check_vfs` (dispatch, ramfs, a mounted tree crossing, two tasks on one fd) | `board/checks/vfs.rs` | **same** | machine-independent |
| `check_ipc` (pipe, tty line discipline, signals) | `board/checks/ipc.rs` | **same** | machine-independent |
| `banner`, `park`, `check_boot_stack`, `report_stack_use`, `panic_context` | `board/report.rs` | **same** | machine-independent: `BootInfo`/`hal`/`sched`, with the stack pointer read through `hal::Cpu::stack_pointer` and the text range from `arch::kernel_sections` (§13.23, 15.4) |
| the `init` callback's signature | `fn(&BootInfo) -> !` | `fn(*const u8, usize) -> !` (the archive is stashed *before* the address-space switch) | a fork in `mpos_arch::arch::boot`'s own seam |
| the root switch | **userland**: the initramfs's `/init` mounts `root=` and `switch_root`s | **kernel**: `check_disk_root` mounts the disk at `/mnt`, `switch_root` pivots, `/init` runs from the disk | the largest duplication: the first machine's flow is a script the second machine has no copy of |
| `M0`–`M8` milestone checks | the boot's verdict | a different set, and they run on **every** boot (in front of a distribution's `init`) | §13.14's rule applies: a check that changes shared state damages the machine it checks |
| capabilities (`.cargo/config.toml`) | `mpos_msi`, `mpos_serial_ring`, `mpos_fpu_trap`, `mpos_signal_frame`, `mpos_framebuffer_console`, `mpos_net`, `mpos_qemu_debug_exit`, `mpos_pci_config`, `mpos_input_registry` | `mpos_console_input`, `mpos_serial_ring`, `mpos_net`, `mpos_signal_frame`, `mpos_input_registry`, `mpos_framebuffer_console` | the seam works; four of the first machine's nine are missing here (`mpos_msi`, `mpos_fpu_trap`, `mpos_qemu_debug_exit`, `mpos_pci_config`), and `mpos_input_registry` + `mpos_framebuffer_console` moved across in §13.24/§13.26 |
| device discovery | PCI enumeration | device tree + virtio-MMIO windows | genuinely the machine's; both answer `hal::map_mmio_window` |

Of the four missing capabilities, three are hardware absence on this port and one is port work:
`mpos_msi` and `mpos_fpu_trap` have no counterpart here (a `virt` machine with a GIC and no `#NM` -
both honest "absent", not stubs), and `mpos_qemu_debug_exit` is an emulator exit port AAVMF does not
answer. `mpos_pci_config` is *not*: the bus is there, but it is not the bus this port drives its
devices over - the device tree is. The three that *have* crossed over are the ones whose device was
already reachable over that tree: `mpos_input_registry` (§13.24), the console (§13.25) and
`mpos_framebuffer_console` (§13.26, on the `virtio-gpu` card the same MMIO transports carry) - each
by handing the same device to the shared driver instead of opening it, reading its identity and
dropping it.

### The target shape

1. **One home for the neutral checks.** `src/board/checks/{mm,sched,vfs,ipc,report}.rs`, under the
   wiring crate for the reasons `board/steps.rs` already records (the depgraph lets the wiring name
   the machine and both libraries; `boot` would close a mutual pair with `vfs`). A body keeps a
   private `devices()` and a private `arch_checks()`; everything else is called from one place.
2. **One `init` seam.** `mpos_arch::arch::boot`'s `init` callback becomes `fn(&BootInfo) -> !` for
   both machines. This is what makes "the same sequence, both machines" expressible at all: the
   aarch64 fork exists only because it stashes the archive before the switch, and *that* is a
   consequence of (3).
3. **One root-switch flow: the second machine gets an initramfs `/init`, like the first.** The
   kernel unpacks the archive and hands the machine to `/init` on both machines; the *script* mounts
   `/proc`, `/sys`, `/dev`, `root=` and pivots. That is Linux's own model, it is what
   `docs/ARCHITECTURE.md` §4 and `AGENTS.md` §4 rule 1 require ("a kernel that assembles them first
   leaves pieces the owner of the system cannot unmount"), and it deletes `check_disk_root`'s mount
   half and `switch_root` outright. The alternative - moving the pivot into the kernel for *both*
   machines - is the same line count with more risk: it re-implements the script's `root=`/
   `rootfstype=`/`rootflags=` loop in Rust, breaks `cargo xtask run`'s "no `root=` → drop to a shell
   in the initramfs" behaviour, and contradicts §4. Cost of the chosen direction: one
   `initramfs-aarch64/` tree plus its toml, a busybox in it (`mount`, `pivot_root`), and `M7_ROOT_OK`
   moving from a kernel marker to a script-printed one - which moves `xtask/src/aarch64.rs`'s
   `TERMINATORS` in the same commit (AGENTS §5's trap: a wrong terminator kills the boot seconds in,
   indistinguishable from a hang).
4. **Nothing machine-independent stays in a body.** After this, `board/<arch>.rs` holds: the entry's
   three callbacks, `devices()`, the machine's own checks (the paging/IDT proof, the UART's TX/RX
   proof, the MMIO transport probes), and the ordering its own milestones need - and every one of
   those is machine work by definition.

### Stages

Each is one commit and green alone (AGENTS §3 rule 7). Only stage 0 is measurement.

| Stage | Contents | Pin | Risk |
|---|---|---|---|
| 15.0 | the inventory above becomes a §13.16-adjacent measurement, with the numbers `grep`-able | — | none |
| 15.1 | extract `check_mm`, `check_sched` (+ helpers), `check_vfs`, `check_ipc` into `board/checks/**`, x86_64 calling them at the same points | `cargo xtask test` prints the same `pass=157 fail=0 (157 built)` + exit 33; `depgraph` unchanged (the wiring is exempt); **both** bare-metal targets build (the modules are cfg-free, so aarch64 compiles them while not yet calling them) | **landed (§13.21)** — identical x86_64 numbers before and after the move |
| 15.2 | aarch64 device parity, one class per commit: the input registry (`mpos_input_registry` + `input::close_at_exit`), virtio-console, virtio-gpu + framebuffer console (`mpos_framebuffer_console`), virtio-sound, vsock, 9p | per class: `boot-aarch64` with the new `M*_OK` **and** its failure marker added to `TERMINATORS`; `test --arch aarch64` stays green; `machine.toml` gains the `-device` | **all six classes landed: the input-registry (§13.24), console (§13.25), virtio-gpu + framebuffer-console (§13.26), virtio-sound (§13.27), vsock (§13.28) and 9p (§13.29)** (§13.4's rule: measure the flip before designing the seam). The input registry's flip was re-measured for this cut and costs **1 error** - `E0432: unresolved import super::hal::transport_all` at `drivers/virtio/input.rs:40`, where the item is gated in `drivers/virtio/hal.rs` - against the **4** recorded here before, because three of those were in `src/vfs/sysfs.rs`'s `build_input` and §13.23's capability flip made them resolve. The design cost the number did not show was real and is now paid: `EvdevNode` was monomorphic over `VirtIOInput<HalImpl, PciTransport>` through `read_caps`, `service`'s harvest and `init`'s enumeration, so the class became a transport-neutral contract (`drivers::virtio::InputDevice`, with the crate-backed `VirtioInputDev<T>` as the one implementation of its six methods), the MMIO side opens its own window and hands the live device to `input::register`, and the board carries `check_input` + `check_input_registry`. The marker moved with the pin: `M8_INPUT_OK` is printed after `check_dev`, where the registry can be read back through a mounted `/dev` - `input::nodes()` non-empty *and* `/dev/input/event0` resolving at evdev's `13:64`, because event flow needs a keypress and the harness is headless (the `/sys/class/input` half is the same capability's, and appears when sysfs is mounted). The console class (§13.25) then cost the same **1** error (`E0432: unresolved import super::hal::transport`) for the same reason and paid the same kind of design debt: a `ConsolePort` contract, one `VirtioHvc<T>` over the crate's driver, `mmio::console_device` opening the window, the devfs `#[cfg(target_arch = "x86_64")]` on `/dev/hvc0` deleted, and the marker resolved through the mounted `/dev`. The virtio-gpu class (§13.26) was measured the same way and cost the same **3** errors - but they hid a *type-erasure* refactor rather than a one-helper seam, which is the one place this phase's "measure the flip first" rule under-reported: the crate's `Transport` is not object-safe, so `Gpu` became `Gpu<T: Transport>` and the render task had to hold a `Box<dyn GpuDevice>` (a 16-method trait) to keep one copy of it. The rest of the stack needed nothing (one `pci::` reference in 7.6 kLoC). The sound class (§13.27) then cost the same **1** error and was the console class's shape exactly - a three-method `SoundCard` contract, one `VirtioSnd<T>` over the crate's driver, `mmio::sound_device` opening the window, the devfs gate deleted, and a marker that resolves the node and moves a silent burst through the data path (the OSS *parameter* face is `abitest`'s `t-dsp`, which this machine's sweep does not carry - a named gap). The vsock class (§13.28) was the console class's shape again - the same **1** error (`E0432: unresolved import super::hal::transport`) and the same one-contract/one-generic-driver design (`VirtioVsock<T: Transport>`, `mmio::vsock_device`, the module's `#[cfg(target_arch = "x86_64")]` deleted, `spawn_service`'s vsock half moved to the contract) - but it is the one class whose device this host cannot always provide: QEMU does not emulate vsock in userspace, so `vhost-vsock-device` needs `/dev/vhost-vsock`, and the aarch64 boot attaches it only when the host has one, requiring `M8_VSOCK_OK` only then and printing the skip otherwise (never silently, and with a one-line summary that then claims seven devices rather than eight). The 9p class (§13.29) cost the same **1** error and turned up two defects the machine had carried all along: the PCI probe (`fs9p::init_global`) had **no caller at all**, so `devices()` was always empty and `/mnt/share` was never mounted on *either* machine, and the client spoke plain `9P2000`, which QEMU 11 answers `"unknown"` to (measured: `.u` and `.L` are what it serves). The client now speaks `9P2000.u`, `mount_share` moved out of the first machine's body into the shared steps, and the class is pinned twice - the aarch64 boot's `M8_P9_OK` (the mounted share serves a 39-byte file back) and `abitest`'s `t-9p` on the first machine (the boot's mount, a program's own `mount -t 9p` over the dispatch, an unexported tag's `ENOENT`) - with `parse_stat` newly driven by a `fuzz/` target |
| 15.3 | aarch64 adopts the shared checks, in an order its own markers permit | `boot-aarch64` full chain + `INIT_EXITED`; `test --arch aarch64` green; `alpine-aarch64` still reaches `localhost login:` | **landed** (§13.21 + §13.22): all four families run on both machines, and adopting them found three defects the machine had carried since its scheduler landed - the clock never preempted, the EL0 interrupt stub never saved the interrupted program's resume state, and the EL1 one never saved the kernel's (the abort that held `checks::vfs`/`checks::ipc` out until it was fixed). They run **under `mpos.selftest` on both machines** (§13.14's rule: the Alpine boot must not pay for or be damaged by them) |
| 15.4 | shared reporting: `banner` on both, `check_boot_stack` on both (measure the loader's canary first), `park` instead of `halt`, a real `panic_context` | `boot-aarch64`, both sweeps, host tests | **landed** (§13.23): `src/board/report.rs`, one copy for both bodies, with `hal::Cpu::stack_pointer` as the one machine question and the boot-stack canary moved into `crates/bootinfo` so the loader and both kernels read one definition |
| 15.5 | one exit-hook registry instead of two bodies (`register_exit_hooks`: five hooks + two cloexec on x86_64, one on aarch64) | `t-tcp`/`t-tcp-listen` (they measured the socket hook's absence as `EADDRINUSE`), both sweeps | **landed** (§13.23): `steps::register_exit_hooks`, one list in the one order, the two input hooks under `mpos_input_registry` - which also became the gate for the input modules and their `/dev` block, replacing four `target_arch` gates |
| 15.6 | the initramfs flow: `initramfs-aarch64/` + toml, `alpine-aarch64` stages it, the script pivots, `switch_root`/`check_disk_root`'s mount half deleted, the `init` seam unified to `fn(&BootInfo) -> !` | `alpine-aarch64` → `localhost login:`; `boot-aarch64`; both sweeps; `depgraph` | **landed, all four pins green.** `boot-aarch64` full chain + `INIT_EXITED`; `test --arch aarch64` `pass=27 fail=0`; `cargo xtask test` `pass=158 fail=0 (158 built)` exit 33; `alpine-aarch64` reaches `mpos login:` in 16 s; `depgraph` within the ratchet. Cost seven defects, all recorded below - and the risk was real: §13.20's bug lived on this exact path, and six more had hidden behind it |
| 15.7 | **the other half of "one flow": the archive *build* is one pipeline too.** `xtask/src/aarch64_initramfs.rs` (249) was the second machine's copy of `xtask/src/initramfs.rs` (156) - same staging tree, same apk, same farm, same ustar, same staleness question, written twice. It is one module now, parameterised by the machine: the toml, the overlay, the staging/archive names, the package source, the applet farm, and whether a stamp records the tree's mtime. The `aarch64-initramfs` subcommand is retired for `initramfs --arch aarch64` - which needed `main.rs`'s `--arch aarch64` refusal to stop covering `Initramfs`, because it never was the x86_64 disk's: it is a directory of files, and both machines have one. `--arch` then decides the *root requirement* too: the first machine's farm is `chroot … busybox --install -s` (root), the second's is a hand-laid symlink list under `apk --usermode` (not root), which is why the retired command needed no root either. **What is deliberately *not* unified** is the two `/init` scripts and the two tomls: the script differences (the `R:` selftest read, `MPOS_SWITCH_ROOT_OK`, and the `exec` target - the second machine's disk carries its own pid-1 chain) are the *disk root's* difference rather than duplication, and the toml pair is the same convention `rootfs.toml`/`rootfs-aarch64.toml` already follow | `cargo xtask initramfs --arch aarch64`'s archive is member-for-member identical to the retired command's (138 members, `/init` md5 `66e653b6…`, all eight farm symlinks and modes equal; the one mode delta, `var/empty` 555→755, is `--usermode`, i.e. *who ran the build*, not the pipeline - the pre-change archive was laid as root); `cargo xtask boot-aarch64` `EXIT=0` with the whole chain through `MPOS_INITTAB_OK bytes=1957` and `INIT_EXITED`; `cargo xtask test` `pass=159 fail=0 (159 built)`; `cargo test -p mpos-xtask` 69 passed; `depgraph` within the ratchet; `cargo fmt --check` clean | **low** - the pin is byte-identity, and it was measured before and after |
| 15.8 | **the sweep's machine-independent half is one copy.** `xtask/src/sweep.rs` holds what `selftest::run_tests` and `aarch64::run_sweep` had written twice: the `tests.toml` load and the undefined-group refusal, the staged/skipped selection, the `"user tests: {} staged, {} left out by tests.toml"` line and the per-group list under it, and the `=== RUN/PASS/FAIL/SUMMARY/END` parser. Each arm keeps what only it can answer - which target to build, how the programs travel (initramfs+FAT vs ESP+ext4), which firmware, when the run is over - **and its own verdict wording**, because the claims differ: the first quotes the debug exit (`status == Some(33)`), the second has no exit to read and asserts an exact count instead. The parser takes the stricter of the two rules each arm carried (the *last* `=== SUMMARY`, and a failed program's name as the first word after `=== FAIL `, since the runner appends `exit=N`). **One rule that looked shared is not**, and it was measured rather than argued: "every name in `tests.toml` must be answerable by a program this arm built" has two scopes, because the arms do not build the same file - x86_64 builds both suites (169 programs) and can require it of the whole file, while aarch64 builds only the std suite (30) so `t-sh`, `t-dns`, `t-rawicmp`, `t-tcp-close`, `t-fill-ramfs`, `t-forkcost`, `t-netlat` and `t-pipecost` legitimately have no program there. With the check shared, the aarch64 sweep refused on exactly those eight names; each arm keeps its own question and its own wording now | both sweeps' reportable lines are **byte-identical** to the pre-refactor runs (`user tests: 169 programs built` / `159 staged, 10 left out` with the three group lines, `pass=159 fail=0`, `self-test: all 159 …` on x86_64; `30 aarch64 programs built` / `28 staged, 2 left out`, `pass=28 fail=0` on the second machine), `EXIT=0` on both; `cargo test -p mpos-xtask` 75 passed (5 of them this module's); `cargo xtask test --arch aarch64 --groups slow` still refused by name with the same message; `depgraph` within the ratchet; `fmt --check` clean | **medium** - the only hard evidence is that the output did not move, so it was captured before and diffed after |
| 15.9 | **the two machines' QEMU wiring is in one file and reads one `machine.toml`.** `aarch64_qemu` (190 hardcoded lines, `-m 256`, `vsock_available()`, `-display none`) and `find_ovmf`/`firmware` were the last of the second machine's boot written twice. Now: `machine.toml` carries the second machine's own values under an `aarch64_` prefix (`xtask/src/config.rs::take_for` - the plain key is the first machine's, the prefixed one wins for the second, and a key with no override stays shared), `xtask/src/qemu.rs` holds both machines' command lines (`machine_args`, `aarch64_machine_args`) with one firmware lookup (`find_firmware`), and one capture loop (`capture`) replaces the two polls. **What stayed per-machine is the device list**: every aarch64 device is a `virtio-*-device` MMIO form because that is how that kernel finds anything at all, so it is a machine fact rather than a `machine.toml` key, and `aarch64_extra_args = []` is written explicitly because the key falls back to the shared `["-vga","none"]`, an x86-only option a `-machine virt` boot refuses. **And the loop's marker poll had never worked**: it read the log with `read_to_string`, the second machine's log carries openrc's escapes and one byte sequence that is not valid UTF-8, so the read failed and every `boot-aarch64` waited out its whole 60 s deadline before reading a marker that had been in the file for ten seconds. `log_contains` searches bytes; the same boot is now 7 s | `boot-aarch64` `EXIT=0` in **7 s** (was ~60 s), the whole chain through `INIT_EXITED`, no deadline line; `cargo xtask test` `pass=159 fail=0 (159 built)` exit 33; `alpine-aarch64` `EXIT=0` in 14 s, `mpos login:` with no `[ !! ]`; `cargo test -p mpos-xtask` 81 passed (10 qemu, incl. the second machine's line, its MMIO device set, the host-conditional vsock, the byte-wise marker search and the three capture stop reasons); `depgraph` within the ratchet; `fmt --check` clean | **medium** - the value is one wiring surface, and the measurement is the marker poll finally firing |
| 15.10 | **S4's premise was measured before a seam was designed, and the measurement says the seam is already there.** Ungating the four x86-only virtio modules (`blk`, `net`, `rng`, `msix`) for aarch64 costs **9 errors**, and every one of them is the *PCI helpers* (`crate::drivers::pci`, `hal::pci_root`, `hal::transport`, `hal::init_message_signalled`) - not a hidden second bring-up. The bring-up itself is already one copy per class, because Phase 15 landed it that way: each class's *install* is shared (`console::install`, `sound::install_probed`, `input::register`, `net::vsock::init`, `fs9p::install`, `VirtioGpuDev::install`, `steps::bring_up_net`) and only the *transport open* is the machine's - x86_64 enumerates PCI, aarch64 walks its device tree - which is what §15's target shape says it should be. So there is no second bring-up to unify, and forcing one would mean a `dyn` over `virtio_drivers::Transport`, which is not object-safe. **What the comparison did find is a defect**, and it is exactly this class: the disk's registration was written twice, and the second copy drifted - `board/aarch64.rs` registered the boot disk as `add_disk("vda", 254, "/dev/vda", dev)` while the first machine wrote `/devices/virtio/block`, so the second machine filed its disk at a tree position no device has and every event about it announced `DEVPATH=/dev/vda/vda`. The fix is one call (`drivers::virtio::register_block_disk`) with the facts in one place (`uevent::VIRTIO_BLK_DEVPATH` beside `VIRTIO_BLK_MAJOR`), so the second copy cannot come back | `cargo check` clean on both bare-metal targets; `boot-aarch64` `EXIT=0` with the whole chain through `INIT_EXITED`; `test --arch aarch64` `pass=28 fail=0`; `cargo xtask test` `pass=159 fail=0 (159 built)`; host tests **514** (one new: the virtio disk is numbered and filed where its driver says); `depgraph` within the ratchet - the edge `drivers->blk`/`drivers->uevent` already existed per-module, so no new edge and no ARCHITECTURE.md change | **low** - and the stage's own answer is that the rest of it is not work |
| 15.11 | **the ABI suite starts moving to the std side, so the second machine can run it.** The gap TEST-AND-BOOT-REFACTOR.md §1 measured is "139 programs only one architecture can run": `user/abitest` is `no_std` raw-syscall, built for `x86_64-unknown-none` with the kernel's own `nr.rs`, while `user/tests` is `std` + `rustix` and builds for both machines. The migration is mechanical per program (`#![no_std]`+`lin::raw_sys` -> `fn main()` + `rustix::`, `lin::check_true`/`done` -> `Report::check`/`finish`) and it moves a `[[bin]]` block from `user/abitest/Cargo.toml` to the std crate's flat `src/bin`. **Every batch so far, every program self-contained** - batch 1 `t-getuid`, `t-getpid`, `t-getrandom` (no filesystem, device or net at all); batch 2 `t-mkdirat` (its scratch directory is under `/tmp`, which is what the second machine's payload carries) and `t-fionbio` (its fixture is a `pipe`, and `FIONBIO` is served unconditionally on both machines - checked before the move rather than after); batch 3 `t-read` and `t-write`, whose fixture **had** to move: the no-std versions read `/etc/motd`, and the second machine's sweep disk has no `/etc/motd` at all (measured with `debugfs`: `/etc` carries `inittab`, `doas.conf`, `resolv.conf`, `local.d`, `network`) - so `t-read` writes its own file under `/tmp`, which changes the *fixture* and not the claim. Their `PROT_NONE` page is mapped at an address the kernel chooses rather than the no-std suite's fixed `0x5000_0000`: the claim is that the page cannot be read, not where the machine's address space happens to have room. `t-write`'s `EBADF` probe uses descriptor 999 rather than `-1`, because `rustix`'s `BorrowedFd::borrow_raw` refuses a negative raw fd by contract; the kernel's answer is the same branch. **Batch 4 (`t-dup`, `t-dup2`) is where the migration's rule had to change, and the measurement is the reason**: `rustix::io::dup2` is not a binding of `dup2` - its own source substitutes `dup3(fd, new, DupFlags::empty())` under `cfg(target_arch = "aarch64")`, because its `new` is `&mut OwnedFd` and `oldfd == newfd` is therefore unreachable through it. That case is the middle of `t-dup2`, so the wrapper would pin a different call on one machine than the other. `user/tests/Cargo.toml`'s rule now names this shape (a wrapper that is not the syscall the ABI promises, or that discards its result - `rustix::io::close` returns nothing, so "close answers 0" needs `libc::close`) beside the original one (a call rustix does not bind at all). And `t-dup2` pins a *different level* on each machine, which is honest rather than a defect: arm64 has no `dup2` syscall (`nr_aarch64.rs`: `DUP2 = 64006; // no arm64 number; unreachable on aarch64`), so on the second machine musl implements it over `dup3` and the program pins the answer a caller gets, while on the first it reaches the syscall itself. What had to exist before either: `user/tests/src/lib.rs`'s `Report`, the crate's own verdict helper (every one of the thirty wrote its own `struct Report`, which is also how `t-process-vm-libc` came to end on "all passed" where its neighbours end on `PASS`). **Batch 5 (`t-access`, `t-openat`, `t-dup3`) measured the fixture again and found the rule's third shape.** The first two had to move their "existing file" off `/etc/motd` for the reason batch 3 found, and `t-openat`'s one path it does *not* create itself - `/bin/sh` - was measured on the second machine's disk before the move (a 479176-byte static ELF, ELF magic), because "the image starts with ELF magic" is a claim about a real program rather than a fixture the test just made. `t-access` writes its own file after removing any leftover, so the mode its X_OK claim rests on is `0666 & !umask` rather than whatever the image happened to ship. `t-dup3` is where `user/tests/Cargo.toml`'s `libc` rule gained its third clause, and again the measurement is the reason: `rustix::io::dup3` *binds* the syscall but its shape cannot express the claims - it returns `()` rather than the new number and takes its target as a non-aliasing `&mut OwnedFd` (so `oldfd == newfd`, the one case dup3 differs from dup2 on, is unreachable through it) - so `libc::dup3` carries those two, and the third claim (a flag word with bits beyond `O_CLOEXEC`) rides along. The flags are *not* a reason, and finding that out took a measurement: `DupFlags` is a bitflags type carrying bitflags' unnamed flag (`const _ = !0`), so `from_bits_retain(!0)` holds an invalid word and rustix *can* express that claim. Unlike `t-dup2`, it pins the *same* syscall on both machines: `DUP3` has a real number on each (`24` / `292`) and both reach `handle_dup3`. **Batch 6 (`t-getcwd`, `t-chdir`) is the third clause a second time, and the fixture moved once more.** `rustix::process::getcwd` binds the call but not the contract: it takes a `Vec<u8>` and returns a `CString`, growing the vector on ERANGE until the path fits, so the *size the caller chose* (and its EINVAL for a buffer under two bytes and its ERANGE answer) and the *address the syscall returns* are both outside it - `libc::getcwd` has the shape the ABI promises, and both machines reach the same `handle_getcwd`. `chdir`/`fchdir` do come from `rustix`. `t-chdir`'s fixture had to move for batch 3's reason: the no-std version chdir'd to `/etc` and opened `motd` relatively, so it now owns `/tmp/t-chdir_file` and chdirs to `/tmp`, whose `..` is `/`, before the relative step into `/bin` - a directory both payloads carry. **Batch 7 (`t-faccessat`, `t-faccessat2`, `t-openat2`) measures the rule a second and a third time, and the numbers a raw call needs turn out not to be in `libc` at all.** `t-faccessat` is the three-way agreement `apk`'s bug was about, and the third spelling has to be the third syscall: `rustix::fs::accessat` *is* `faccessat` (its linux_raw backend issues `__NR_faccessat` when the flag word is empty) and reaches `faccessat2` only when a caller passes a flag, so the program calls `libc::syscall(libc::SYS_faccessat2)` for the no-flag form. Its privilege drop is `rustix::thread::set_thread_uid` rather than a `libc` call - the wrapper's own documentation says it is the kernel's per-thread `setuid` and not POSIX's, which is precisely the call a single-threaded program wants and precisely `__NR_setuid`. `t-faccessat2` makes the flag word its whole subject, so all twelve of its claims are the raw call; and `AT_EACCESS` turned out to have *no* `libc` binding on this target (`libc` 0.2.189 defines it for fuchsia, the BSDs, Hurd and AIX and in no `linux_like` file - measured), so its flag numbers come from `rustix::fs::AtFlags` while the call stays raw. `t-openat2` is the rule's third clause a third time: `rustix::fs::openat2` writes `size_of::<open_how>()` itself, so the size word - 23 `EINVAL`, 25 `E2BIG`, 24 opens, which *is* the program's subject - is unreachable through it, so the whole program is `libc::syscall(libc::SYS_openat2)` with a local `#[repr(C)]` `OpenHow` while every flag and mode word still comes from `rustix`'s tables. **The resolve bit is not a second reason, and that correction is the one batch 8 turns on**: `ResolveFlags` carries bitflags' unnamed flag (`const _ = !0`) exactly as `DupFlags` does, so an unnamed bit *is* expressible through `rustix`'s `from_bits_retain`. A rustix *plain enum* (`SeekFrom`, `Advice`) has no such escape, so an out-of-vocabulary *word* genuinely has nowhere to go - and that, rather than a bit, is what forces `libc`. Its readable file moved off `/etc/motd` for batch 3's reason. `chmod` has no arm64 number either (`nr_aarch64.rs`: `CHMOD = 64020; // no arm64 number`), and `rustix::fs::chmod` is written as `fchmodat(AT_FDCWD, ...)` on every machine, so batch 7's `0000` step is one call on both. **Batch 8 (`t-lseek`, `t-fadvise`, `t-fallocate`) is the first batch written under the corrected rule, and the correction pays off in one of the three.** `t-fallocate` is *pure `rustix`*: its unnamed mode bit travels as `FallocateFlags::from_bits_retain(0x8000)`, bitflags' own escape for "a word from a kernel newer than the flag list", where the pre-correction rule would have sent the whole program to `libc` for a reason that was not true. `t-lseek` and `t-fadvise` do need `libc`, and for exactly the shape the correction tells apart from a bit: `SeekFrom` and `Advice` are plain *enums*, so an out-of-vocabulary word - whence 5, advice 6, advice 99 - has nowhere to go in them at all. Both reach for the named wrapper rather than a raw number, `libc::lseek` and `libc::posix_fadvise`, and the second has a shape worth naming: POSIX's wrapper *returns the error number*, so those two claims compare against `libc::EINVAL` directly while the `rustix` ones compare against `Err(Errno::INVAL)`. `t-lseek`'s fixture moved off `/etc/motd` for batch 3's reason, and the size its claims are stated against is the one it drains out of the file rather than a constant it also wrote down. `t-fadvise` needs no fixture at all: its regular file is a `memfd` and its pipe comes from `pipe_with`. `t-fallocate` also had to drop the no-std `lin::Stat`, which is the x86_64 `struct stat`, so the size is read through `rustix::fs::fstat`, whose `Stat` is the machine's own. All three calls carry real numbers on both machines (`LSEEK` 62/8, `FADVISE64` 223/221, `FALLOCATE` 47/285) and both machines reach the same handlers. **Batch 9 (`t-link`, `t-readlink`, `t-unlinkat`) is a pure-`rustix` batch, and one claim turned out not to need the fixture everyone assumed it would.** All three calls are served on both machines (`LINKAT` 37/265, `READLINKAT` 78/267, `UNLINKAT` 35/263) - and two *spellings* are not: `READLINK` and `SYMLINK` have no arm64 number (`READLINK = 64015; // no arm64 number`, `SYMLINK = 64016`), and `rustix::fs::readlink`/`symlink` are written as `readlinkat`/`symlinkat(AT_FDCWD, ...)` on *every* machine, so the migrated program pins one handler on both where the no-std version reached `readlink`/`symlink` themselves on the first. `t-readlink` is the batch's instructive one: `rustix::fs::readlink` is clause (c) - a `Vec` in and a `CString` out, growing until the whole target fits, which hides exactly the truncation and zero-size EINVAL the program is about - but beside it `readlinkat_raw` takes *the caller's* buffer and returns how many bytes were initialised, so the rule is satisfied with no `libc` at all. And its `/proc/self/exe` claims, which looked like they would need a procfs the second machine's payload does not mount, need no mount: the handler matches that literal path string and answers with the path the task was execve'd from, which both machines confirm by printing the same three `ok:` lines. `t-link` drops the no-std `lin::Stat` for `rustix::fs::statat` and reads S_IFLNK through `FileType::from_raw_mode`; `t-unlinkat`'s unknown-flag claim goes through `AtFlags::from_bits_retain(0x1000)` - the bit-versus-enum distinction the rule correction bought, costing no `libc` where the old wording would have called for one. **Batch 10 (`t-umask`, `t-priority`, `t-prlimit64`) is the batch that found the two kernel bugs, and neither was a missing call.** `t-umask` is pure `rustix`, and it is the third time the bit-versus-enum distinction pays: `rustix::process::umask` takes and returns a `Mode`, and `Mode::from` masks with the low twelve bits on *this* side - so the claim that the *kernel* keeps only twelve needs the word to arrive whole, which is `Mode::from_bits_retain(0o17777)` (a bitflags type carries bitflags' unnamed flag; an enum would have nowhere to put it). `t-priority` needed `libc::syscall` for two separate reasons: the `20-nice` word is clause (c) because `rustix::process::getpriority_process` has already converted it (`Ok(20 - ret_c_int(..))` in its own backend, and the named `libc::getpriority` converts the same way), and `sched_setattr`/`sched_getattr` are clause (a) - `rustix` binds neither - so the 48-byte `struct sched_attr` goes through the raw call too, with the numbers from a table that has `GETPRIORITY`/`SETPRIORITY` *swapped* between the machines (140/141 and 141/140), which is what `libc::SYS_*` is for. `t-prlimit64` is clause (b) and (c) at once: `rustix::process::getrlimit` returns an `Rlimit` with **no `Result`**, so the success four of the seven claims read is discarded - and for exactly the reason the bug survived, since a closed `Resource` enum cannot be `99`. **What the two bugs were**: the resource table put `RLIMIT_NOFILE` at row 5, and `setpriority` answered `EINVAL` for a nice outside `-20..=19` where Linux clamps. Both are §15.13 and §15.14; both were invisible because the one program touching each had the wrong number written into it (`lin.rs`'s `RLIMIT_NOFILE = 5`, and `EINVAL` asserted as the contract), and the whole point of the migration is that a claim's number and its claim now have different sources. The clamp also had a second pin: `t-posix` (`user/abitest/`) asserted the same `EINVAL`, so it moved in this commit too. **Batch 11 (`t-fcntl`, `t-fd-list`, `t-filelock`) is the descriptor trio, and all three of the rule's clauses show up in one batch.** `t-fcntl` is clause (c) twice, and *not* for the word a reader would reach for first: `rustix` makes `fcntl` type-safe by splitting it into one function per *command*, so the command number `999` the kernel has no meaning for has no spelling in it, and the descriptor `-1` is the same clause one step down (its `AsFd` cannot name one) - while the word with bits beyond `FD_CLOEXEC` needs no `libc` at all, because `FdFlags` is a bitflags type and `from_bits_retain` hands the kernel the whole word. `t-fd-list` is the batch's *binding* choice: `rustix::fs::RawDir` - `getdents64` with **the caller's** buffer, the call the no-std version made by hand - rather than its high-level sibling `Dir::read_from`, which `fcntl(F_GETFL)`s the descriptor and then `openat(fd, ".")`s one of its own, two calls this program's subject never makes. `t-filelock` is all three clauses in one program: `flock` and `F_SETLK` stay on `rustix` (`flock`, `fcntl_lock`; `FlockOperation` is a plain enum, but every operation this program asks for is one of its named variants, so the missing escape hatch for an out-of-vocabulary word costs nothing here), while `F_DUPFD` is clause (a) for a measured reason (its only plain `fcntl_dupfd` is `#[cfg(target_os = "espidf")]`; the Linux spelling is `fcntl_dupfd_cloexec`), `F_GETLK` is clause (a) again (`fcntl_lock` is `F_SETLK`/`F_SETLKW` only - there is no query), and `close` is clause (b) a second time after `t-dup2`. Its `struct flock` is `libc::flock` rather than the no-std version's hand-rolled 32 bytes, and all twelve of its claims were run against real Linux *in the shape the program makes them*, because a pin's assertions are the first thing that can be wrong: `0, EWOULDBLOCK, 0, 0, 0, 0, 0, 0, 0, 0, F_UNLCK, 0`. The batch also **measured the harness and had to correct itself**: `t-fd-list` mounts a procfs of its own when the payload has none, and *neither* sweep takes that branch, because the aarch64 payload does have `/proc` - its disk chain (`rootfs-aarch64/init`) mounts one before the runner takes over, which is why `aarch64_init.rs`'s "this payload reaches neither /proc, /sys nor the network" is corrected in this commit. And it is the second batch in a row to find a kernel bug by reading a claim's answer against Linux: §15.15. Two further divergences its pins surfaced are named, not fixed, in §15.16. **Batch 12 (`t-fstat`, `t-newfstatat`) is the pair the board had flagged as still open, and it measures the harness from both ends.** Both carried the x86_64 `struct stat` in `lin::Stat`, so both claims became the *answers* through `rustix`'s machine-owned `Stat` - `fstat`/`statat`, `st_size`/`st_ino`/`st_nlink`, `FileType::from_raw_mode` - which is the pattern batches 8-10 used. The *length* stays `t-stat-buffer`'s subject (§13.10) and the comments say where it went, because a wrapper that owns its buffer cannot express "and it wrote exactly this far". Both fixtures moved off `/etc/motd` for batch 3's reason (`/tmp/t-fstat_file`, `/tmp/t-newfstatat_file`), while `/bin` stays: measured, the second machine's staging tree has a flat `/bin` of six regular files and no subdirectory, which is what its `nlink` 2 rests on, and `/bin/sh` there is a 479176-byte static ELF. `t-newfstatat` is pure `rustix`, `AT_EMPTY_PATH` and all - its claim is that two spellings of one description agree - and `t-fstat` needs the raw call for exactly one claim, which is a *new* measurement of what the raw door is for: the null-buffer `EFAULT` claim is outside `rustix` (it owns the buffer) **and** outside musl's `fstat`, which faults in userspace where the raw call answers `EFAULT` (NULL and `0x1` alike, SIGSEGV, measured in child processes so one crash could not hide the next). Run against the host first, as always: `t-newfstatat` 14/14, and `t-fstat` 12/13 - the difference being this host's `/bin`, which is `/usr/bin` behind a symlink, so its `nlink` is larger; that claim is about the payloads' flat `/bin` and not about any Linux. One of the batch's findings is a divergence of this kernel's, §15.17 **Batch 13 (`t-set-robust-list`, `t-set-tid-address`, `t-writev`, `t-kill`, `t-sched-getaffinity`, `t-uname`, `t-rseq`, `t-enosys`, `t-brk`, `t-nanosleep`, `t-clock-gettime`, `t-rt-sigprocmask`, `t-sigaltstack`) is the self-contained thirteen, and it measures the raw door's *answer shape* for the first time**: musl's `syscall` door returns `-1` and leaves the refusal in `errno`, so every raw claim compares through `std::io::Error::last_os_error()` (batch 12's `t-fstat` already did this) rather than against the negated constant the no-std harness could. Three programs turned out to need a *new* kind of honesty, named per machine: on real Linux `rseq(32)` answers EINVAL where mpos answers ENOSYS (measured with both a C probe and this crate's own binary), `sigaltstack` treats a null base as SS_DISABLE and answers 0 where mpos refuses ENOMEM, and `clock_nanosleep(TIMER_ABSTIME)` sleeps to the deadline where mpos refuses EINVAL - so those three claims accept their host's answer under `MPOS_ON_HOST` and pin mpos's refusal on the payload sweep, each with a printed note; `t-enosys`'s seven probes are served calls on a current host and take the same branch. Two more measurements came out of the fixture rule biting the runtime: `t-brk` can no longer grow *the program break* under musl - that is Rust's heap - so it parks its own span above the heap with a private map and states every claim against the break the kernel actually gave; `t-sigaltstack` starts inside a process whose sigaltstack state std's guard-stack setup may have touched, so it pins the lifecycle from a known point (disable, then install, read back, disable) and hands the original state back. `t-rt-sigprocmask`'s unreadable-set probe moved off the fixed `0x5000_0000` because under a dynamic musl that address names nobody's mapping, and is now a page mapped PROT_NONE and unmapped after. The libc/libc-table absences this batch measured: `libc::set_robust_list` does not exist on linux-musl (only glibc/uclibc declare it); `RSEQ_SIG` has no libc binding on this target (musl's own header spells `(0xAB << 24) | 0`); rustix binds neither `sched_getaffinity` nor `clock_nanosleep`'s four-word shape, and its `Signal`/`ClockId` enums cannot name 0/65/999 or clock 10/999 - clause (c) again. **Batch 14 (`t-mmap`, `t-munmap`, `t-mprotect`, `t-madvise`, `t-mremap`, `t-mmap-demand`, `t-mmap-reserve-commit`, `t-segv`, `t-arch-prctl`) is the mmap family, and it moved the fixed-address rule from a comment into a fixture**: the no-std versions mapped at `0x5000_*` because their static image could not be elsewhere; under a dynamic musl those addresses may belong to any mapping, so each program first reserves its own PROT_NONE arena wherever the kernel puts it and runs every MAP_FIXED / hint / NOREPLACE / mremap claim inside that owned span. Five claims turned out to differ between Linux and mpos *by design* and are named per machine rather than hidden (each accepts the host answer under `MPOS_ON_HOST` with its reason in the code): an unaligned `mprotect` inside nobody's region is silently skipped by Linux where mpos answers EINVAL; a grow-in-place `mremap` without MAYMOVE is ENOMEM on Linux (it only grows over already-mapped pages) where mpos keeps the mapping; `MAP_FIXED|MAP_FIXED_NOREPLACE` is EPERM-conflict/EEXIST-ordered on Linux before mpos's pair-refusal EINVAL; advice 5 is still Linux's MADV_REMOVE slot where mpos retired it; and Linux's mincore reads a faulted page's neighbourhood resident (readahead) where mpos faults exactly one. Two more measurements of the raw door: musl's `syscall` wrapper reads the stack canary through `fs:[0x28]` on entry, so t-arch-prctl runs every SET/read-back pair in a forked child (the parent's own TLS cannot survive a foreign FS base), and the same call has no arm64 number at all - the program builds on both machines, says "skipped" on the second, and joins tests.toml's `aarch64` group, which gains its first name. `t-segv` keeps libc::fork/wait4 (std names neither the death-by-signal nor the status word). The family's bindings: rustix binds neither mincore nor madvise nor mremap (clause (a)), and `GetRandomFlags`/`ProtFlags` spellings ride rustix's own enums where they carry the claim. **Batch 15 (the tmpfile/pipe thirteen: `t-copy-file-range`, `t-pipe2`, `t-readv`, `t-renameat2`, `t-eventfd`, `t-splice`, `t-preadv2`, `t-timerfd`, `t-xattr`, `t-fchmodat2`, `t-truncate`, `t-sync`, `t-inotify`, plus the device-shaped `t-dsp`) is where the migration met Linux's *ordering*, not just its answers.** The raw door's split-offset ABI came first: `preadv2`/`pwritev2` take their position as two signed halves (glibc's own convention, measured here), and the test's single-`i64` spelling read back EBADF on a live fd until the helper split it like the ABI does. Six claims turned out to differ between Linux and mpos by design and are named per machine rather than hidden: splice's unknown-flag-vs-zero-length ordering (Linux returns early at len 0; mpos validates flags first), splice's pipe-offset refusal arriving as EBADF/ESPIPE through Linux's fd check where mpos names EINVAL, `fchmodat2` on a symlink (EPERM on Linux, mpos's documented EOPNOTSUPP), ftruncate on a pipe (EINVAL there, EOPNOTSUPP here - the divergence t-truncate has always named), xattr on a tmpfs that *does* store attributes (every EOPNOTSUPP claim takes the host branch), and inotify's whole attribution model: Linux reports CLOSE_WRITE/DELETE_SELF/IGNORED only on the file's own watch, queues the unlink's DELETE bare on the parent, retires silently, interleaves OPEN/MODIFY/CLOSE_NOWAIT across watches, and delivers its queue a buffer at a time - so the life-sequence claims accept Linux's five-step subsequence under the host branch while the payload sweep pins mpos's merged six-record 123456. Two fixture rules from earlier batches paid off again: every `/etc/motd` reader now owns its `/tmp` file (batch 3), and `t-xattr`'s unreadable-value probe maps its own PROT_NONE page instead of naming the no-std suite's never-mapped `GUARD_PAGE` (batch 14's arena rule). Numbers measured against libc's tables where they exist (`SYS_pipe2` 293/59, `SYS_renameat2` 316/276, `SYS_copy_file_range` 326/287, `SYS_splice` 275/76) - and twice they did not: linux-musl aarch64 declares no `SYS_fchmodat2` at all, so that program carries the number from this kernel's uapi table (452 on both machines, which is Linux's), and rustix binds none of inotify/syncfs/timerfd-create's refused words, so those probes ride `libc::syscall`. `t-dsp` keeps the OSS `_IOC` words as soundcard.h computes them (neither library names them) and forgives a host with no card as the honest absence it is. |`t-dsp` keeps the OSS `_IOC` words as soundcard.h computes them (neither library names them) and forgives a host with no card as the honest absence it is. **Batch 16 (`t-stat-consistency`, `t-statfs`, `t-statx`, `t-mount`, `t-chroot`, `t-ns-uts`, `t-9p`) is the stat/mount/namespace seven, and its raw door is about *buffer ownership* three times over.** `rustix::fs::{statfs, statx}` own their buffers (`MaybeUninit`, never seeded), so the two claims that read the kernel's *write coverage* - statfs's poisoned-0xff struct coming back whole, statx's pattern-seeded 256-byte struct - have no spelling through any wrapper and ride `libc::syscall(SYS_statfs/SYS_statx)` into a caller-owned struct; the same door carries statx's out-of-vocabulary flag word (0x10000 EINVAL), because rustix's backend *masks the mask off* before sending it ("users passing flags unknown to rustix could end up writing outside the buffer", says its source) - a wrapper that edits the argument cannot pin what the kernel does with it. One measurement corrected this batch's own first draft: real Linux leaves `stx_mnt_id` at 144 nonzero under a BASIC_STATS request (host, kernel 7.2.9: the only seed survivor past 144 is exactly that word), where mpos zeroes the whole 256 - so the tail claim is named per machine rather than relaxed. The statx struct's *length* is pinned against the ABI's 256 (`STATX_SIZE`), not `size_of::<libc::statx>()` (320 on this glibc host): the declared type's tail is nobody's promise. Three programs met the host's *privilege* rather than its semantics: mount(2)'s bind answers EPERM for an unprivileged user (measured, errno 1), chroot(2) the same, and an unprivileged user namespace refuses unshare by policy - so those sections print a skip verdict under `MPOS_ON_HOST` instead of reporting the host's capabilities as this kernel's answer; the payload always runs as root. t-ns-uts keeps the no-std program's whole observation shape - forked child writes its three doors down a pipe, parent compares bytes - and learned twice the hard way that a forked std child must neither flush nor keep: musl's `exit_group` would run the inherited atexit list (the child leaves through raw `SYS_exit`), and a *leaked-but-open* pipe fd is as wrong as a dropped one (the parent's drain waits on every writer; measured EOF-with-nothing-on-it). Its capability gate still lowers only the effective id, which needs raw `SYS_setresuid` because libc's typed `setresuid` takes `uid_t` and cannot name `-1`. t-stat-consistency drops `lin::Stat` for rustix's machine-native pair (`stat`/`fstat`) and keeps both lock paths including niri's `/run/user/0` (its mkdir chain is best-effort, the open is the check). t-9p's subject exists only inside the sweep boot - no host has a virtio-9p export tagged `hostshare` - so it skips on the host with its reason printed and pins all four claims on the payload. x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `pass=159 fail=0`; aarch64 `61 aarch64 programs built` (was 59) / `59 staged, 2 left out` / `pass=59 fail=0`, with `=== RUN t-getpid` / `t-mkdirat` / `t-fionbio` / `t-access` / `t-openat` / `t-dup3` / `t-getcwd` / `t-chdir` / `t-faccessat` / `t-faccessat2` / `t-openat2` / `t-lseek` / `t-fadvise` / `t-fallocate` / `t-link` / `t-readlink` / `t-unlinkat` / `t-umask` / `t-priority` / `t-prlimit64` / `t-fcntl` / `t-fd-list` / `t-filelock` / `t-fstat` / `t-newfstatat` and their `=== PASS` lines in its log; **every migrated program's per-claim `ok:` lines are byte-identical on both machines** - `t-getuid` 4 lines, `t-getpid` 5, `t-getrandom` 7, `t-mkdirat` 5, `t-fionbio` 9, `t-read` 6, `t-write` 5, `t-dup` 10, `t-dup2` 8, `t-access` 6, `t-openat` 16, `t-dup3` 9, `t-getcwd` 11, `t-chdir` 17, `t-faccessat` 15, `t-faccessat2` 12, `t-openat2` 16, `t-lseek` 12, `t-fadvise` 9, `t-fallocate` 20, `t-link` 25, `t-readlink` 6, `t-unlinkat` 14, `t-umask` 7, `t-priority` 19, `t-prlimit64` 7, `t-fcntl` 10, `t-fd-list` 7, `t-filelock` 14, `t-fstat` 13, `t-newfstatat` 14, **Batch 17 (`t-epoll`, `t-poll`, `t-unix`) is the multiplexer trio, and it is the first batch whose raw door was chosen by *disassembling the maintained table* rather than by reading its API.** `epoll_create` is the measurement that forced it: the crate's own static `libc.a`, disassembled on both machines, spells it `size <= 0 ? -EINVAL : epoll_create1(0)` - musl's own check, then a tail call into `epoll_create1` - so the wrapper never reaches `__NR_epoll_create` at all. On x86_64 the claim therefore rides the raw `SYS_epoll_create` (213), which is the kernel's positive-size rule the no-std version pinned; on arm64 that call has no number (`EPOLL_CREATE = 64028; // no arm64 number; unreachable on aarch64`), so musl's own check is the only door a caller has - and both answer EINVAL, which is why the claim and its `ok:` line are the same on both machines. The wait and the poll are one step down the same shape: `EPOLL_WAIT`/`POLL` have no arm64 number (`64029`/`64003`) where `EPOLL_PWAIT = 22` and `PPOLL = 73` do, so `t-epoll`'s wait is `SYS_epoll_wait` here and `SYS_epoll_pwait` with a null mask there, and `t-poll`'s millisecond word is `SYS_poll` here and `SYS_ppoll` (a timespec, `-1` spelled as a null pointer) there - each number checked against the kernel's own `nr_aarch64.rs` and libc's table before it was written. The rest of the trio's raw door is clause (c) and the *caller's* buffer five times: `PollFd::new` zero-fills the slot (the sentinel seed that catches a kernel scribbling on `revents` has no spelling through `rustix`), `BorrowedFd::borrow_raw` refuses `-1` (so musl's resolver-shaped array - placeholders parked at `-1` beside the live socket - cannot exist inside a rustix array), `rustix::event::epoll` splits `epoll_ctl` into `add`/`modify`/`delete` (op `7` and a null event pointer have nowhere to go), the wait derives `maxevents` from the buffer's length ("maxevents 0 while handing over eight records" is inexpressible), and `getsockname`'s echoed `addrlen` is hidden behind `SocketAddrAny`'s parsed variants, where the number *is* the claim. `t-unix` keeps `rustix` for the other calls (`socket`, `bind`, `connect`, `listen`, `accept_with`, `shutdown`, `SocketAddrUnix::new_abstract_name` - so the abstract-namespace and empty-path probes reach the kernel *as* addresses), takes the raw door for three (a foreign family word under a unix body, the addrlen echo, and the empty path, which `SocketAddrUnix::new("")` builds and then refuses to send), and makes the listener nonblocking through `rustix`'s own `fcntl_getfl`/`fcntl_setfl` pair (the shape `t-pipe2` and `t-tcp-listen` use) beside the connector's `SOCK_NONBLOCK` type word - a first draft reached for `libc::fcntl` on a claim about `rustix` that reading its source refuted. One fixture changed for a measured reason: the no-std version re-bound a socket that had *already* been bound, and Linux refuses exactly that (EINVAL whether or not the name is free, measured on the host), so the re-bind is made from a socket that has never been bound - the claim unix(7) states rather than the one the old test happened to make. Three per-machine answers the no-std version had already named are re-measured and kept, each with its host branch beside its payload answer: an unregistered target answers ENOENT here and EBADF on a host (errno 9 - Linux refuses an unbound number at the door before the interest list is consulted), `getpeername` on a connected unix socket answers the anonymous triple here and the listener's bound path on a host (measured: `/tmp/t-unix.sock`, length 19) because this kernel keeps no peer name on a connect, and a connect to a bound-but-unlistened name is `ECONNREFUSED` here where Linux rendezvouses with the backlog-0 queue (the divergence NOTES.md's socket section already carries). x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `pass=159 fail=0` (exit 0); aarch64 `107 aarch64 programs built` (was 104) / `104 staged, 3 left out` / `pass=104 fail=0` (exit 0), with `=== RUN t-epoll` / `t-poll` / `t-unix` and their `=== PASS` lines in its log; the three programs' per-claim `ok:` lines are **byte-identical on both machines** - `t-epoll` 37 lines, `t-poll` 20, `t-unix` 58, **Batch 18 (`t-fork`, `t-exit-group`, `t-wait4`, `t-waitid`) is the process/reap four, and it moved the layout question from this repository into the machine.** `fork` is unbound in `rustix` on both machines, so creation is `libc::fork` (arm64 reaches `clone`; `nr_aarch64.rs`'s `FORK = 64007; // no arm64 number`), and every child leaves through the raw `SYS_exit_group` - the subject of `t-exit-group`, and the one door that cannot flush a forked std child's inherited copy of the parent's buffered stdout. That musl's `_exit` *is* that call was measured from the crate's own static `libc.a` (`__syscall(SYS_exit_group, status)`, then a `SYS_exit` loop for the return that cannot happen), which makes the raw spelling a choice rather than a shortcut. Reaping is `rustix::process::waitpid` - `wait4` with a null `rusage`, the call the no-std versions made by hand - and its `WaitStatus` is where `WIFEXITED`/`WEXITSTATUS` went (`exited()`, `exit_status()`, `as_raw()`); `ECHILD` becomes `Err(Errno::CHILD)` and `WNOHANG`'s "0, not an error" becomes `Ok(None)`, a *different variant*, which is the same claim at full strength. `t-fork`'s `static mut SHARED` became an `AtomicI32` (the 2024 edition refuses to reference a `static mut`) and states copy-on-write just as plainly. **`t-waitid` is where the raw door was kept and the *offsets* handed over.** The record the kernel writes *is* this program's claim and `rustix::process::waitid` owns its buffer, so the call is `libc::syscall(SYS_waitid, …)` (247 here, 95 on the second machine) into the caller's own record; the no-std version read `si_pid` at byte 16 and `si_status` at 24 through an offset written here, and the batch measured what those bytes are before replacing them - dumping a real record out of this host's kernel (musl, `waitid(P_PID, child, WEXITED)`) shows `si_signo` 17 at byte 0, `si_code` 1 at byte 8, the child's pid at 16, its uid at 20, the code at 24: asm-generic's `{signo, errno, code, union}` with the union pushed to 16 by its own 8-byte alignment, which is the shape of *both* machines here (x86_64 includes asm-generic's header, arm64 is asm-generic). The offsets were therefore right on both and are now unnecessary on both: the fields are read through the machine's own `libc::siginfo_t` (`si_signo`/`si_code` as fields, `si_pid()`/`si_status()` as libc's accessors) with the record's length pinned against the ABI's `SI_MAX_SIZE` (128) rather than a number written here. **Two fixtures moved and the claims did not**: `t-wait4`'s "still running" child was `/bin/sleep 2`, and the second machine's `/bin` has no `sleep` at all (measured - its farm is `initramfs.rs`'s fixed `AARCH64_APPLETS`, which names `true`/`false` for `t-stdspawn`'s sake and nothing else this test needs), so the sleeper became the test's own forked child, asleep 500 ms and leaving through `SYS_exit_group`; and `t-wait4`'s `execve` is `libc`'s, because the crate's one `execve` sits in `rustix::runtime`, a doc-hidden experimental module outside the enabled feature set - clause (a), recorded in the file. **Three divergences `t-waitid`'s pins surfaced, named rather than fixed** (`docs/UAPI.md` §5.2 carries them; each was measured on the host before its claim was written): the `id` argument is ignored, so `P_PID` hands back whichever child finishes first where `wait4` *does* honour its pid; `P_PGID` (2) and `P_PIDFD` (3) are answered `EINVAL` where Linux *serves* `P_PGID` (`idtype = 2`, `id = 0` is accepted and a childless caller gets `ECHILD`, errno 10); and a call asking for no event at all is answered `0` with the record cleared, where Linux clears the record and then refuses with `EINVAL` (errno 22) - the mild-direction §3.2 shape, a success where Linux refuses. None is fixed here: the first two need a *feature* (waiting on a process group, and using the id at all), the third one condition in `handle_waitid`, and that handler's file is held by another line of work in this tree; the migrated program pins this kernel's answers on both machines, with each host answer named beside it. x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `pass=159 fail=0` (exit 0); aarch64 `111 aarch64 programs built` (was 107) / `108 staged, 3 left out` / `pass=108 fail=0` (exit 0), with `=== RUN t-fork` / `t-exit-group` / `t-wait4` / `t-waitid` and their `=== PASS` lines in its log; the four programs' per-claim `ok:` lines are **byte-identical on both machines** - `t-fork` 5 lines, `t-exit-group` 4, `t-wait4` 12, `t-waitid` 20, **Batch 19 (`t-execve`, `t-mmap-shared-fork`) is the pair beside batch 18's family - replacing a process's image, and the memory a `fork` keeps shared - and it is the batch where the family's door split in two.** `t-execve` rides `libc` end to end: `libc::execve`, because the crate's one `execve` lives in `rustix::runtime`, a doc-hidden experimental module outside the enabled feature set (clause (a), the reason `t-wait4` gives for the same call), `libc::fork`, the raw `SYS_exit_group` for the code the child leaves with, and `rustix::process::waitpid` with `WaitStatus` for the reaping. Its one fixture moved: the no-std version's real-and-non-executable file was `/etc/motd`, which the second machine's payload does not carry at all (batch 3's measurement), so the program makes its own under `/tmp` at `0644 & !umask` - the mode the EACCES claim rests on is the test's own now, which is the shape batch 5's `t-access` gives the same kind of probe. `t-mmap-shared-fork` is the batch's other half and the first program in the suite that is **pure `rustix` for a reason**: `mmap_anonymous`, `munmap` and `madvise(.., Advice::DontNeed)` carry every call, and that is exactly the distinction the memory family's neighbours could not make - `t-mmap` and `t-madvise` ride `libc` because their claims *are* the six-word call and the advice vocabulary, while this program's claims are about what the pages are (whether two processes alias them), and `rustix`'s `Advice` carries Linux's own `MADV_*` numbers rather than a POSIX translation. The one wrapper edit is named in the file rather than hidden: `mmap_anonymous` ORs `MAP_ANONYMOUS` into the caller's flags and `MapFlags` has no name for that bit - harmless here precisely because the flag word is not this program's subject, and the same kind of edit is why `t-mmap` goes through the raw door. x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `pass=159 fail=0` (exit 0); aarch64 `113 aarch64 programs built` (was 111) / `110 staged, 3 left out` / `pass=110 fail=0` (exit 0), with `=== RUN t-execve` / `t-mmap-shared-fork` and their `=== PASS` lines in its log; the two programs' per-claim `ok:` lines are **byte-identical on both machines** - `t-execve` 6 lines, `t-mmap-shared-fork` 15, diffed clean - the pin the whole exercise exists for; **Batch 20 (`t-rt-sigaction`, `t-rt-signal-queue`, `t-signal-delivery`) is the signal family's first three, and it is the batch whose raw door had to be *declared* - a record the ABI owns, which no header either machine publishes to Rust declares.** The record `rt_sigaction` copies in and out is `{handler, flags, restorer, mask}`: 32 bytes, one 8-byte mask word, `sigsetsize` the ABI's 8. It was measured on *both* machines before a line of it was written - this host's kernel fills a query's record in exactly those four slots (`rt_sigaction(SIGSEGV, NULL, &rec, 8)` leaves handler 0, flags 0, restorer 0, mask 0), and arm64's own musl builds the same 32 bytes for its `__libc_sigaction`, which the crate's `libc.a` shows storing the handler at 0 and the flags at 8, putting the restorer at 16, `memcpy`ing 8 bytes of mask to 24, ORing `0x04000000` into the flags word and calling `SYS_rt_sigaction` (134) with `sigsetsize` 8. So the no-std `lin::SigAction` was right on both machines; what moves is where the numbers come from (`libc::SYS_rt_sigaction`, 13 and 134, and libc's signal names) and the one number no table names - `SA_RESTORER`, which this libc defines for redox alone, so it is written down here with the musl measurement beside it. `libc::sigaction` could not have carried the program: it is the machine's *user-space* struct - a 128-byte `sigset_t` in the middle of it, a different field order - which its wrapper converts, and that wrapper validates the signal number itself (0 and 65 in userspace, and on musl its own three real-time numbers 32, 33 and 34 before the syscall), so the refusal set, the seeded record and the `EFAULT` probe would all have measured the libc rather than the kernel: clause (c) on the record's shape beside clause (b) on the wrapper's answers, the pair batches 12 and 17 measured. **Two things moved under the test.** The *runtime*: std arms SIGSEGV and SIGBUS on its guard stack before `main`, so "the fresh query reports SIG_DFL" is no longer what a caller finds - the value claim moved to a number nothing has touched (`SIGUSR1`), the SIGSEGV query keeps the half that *is* about the kernel (it must overwrite every seeded word of the record), and the two guard dispositions are saved and handed back on the way out, which is the shape batch 13 gave `t-sigaltstack`. The *fixture*: the unreadable-record probe's fixed `0x5000_0000` - safe only in a static-PIE image - is now a page the process maps `PROT_NONE` and unmaps, batch 13's `t-rt-sigprocmask` shape. **One divergence this pin surfaced is named rather than fixed** (`docs/UAPI.md` §5.2): Linux refuses SIGKILL and SIGSTOP only when a disposition is being *set* (`act && sig_kernel_only(sig)` in `do_sigaction`), so a bare `rt_sigaction(SIGKILL, NULL, NULL, 8)` answers 0 - measured on this host - where this kernel's `catchable` check runs before it looks at `act` and answers EINVAL. The payload pins this kernel's answer on both machines and a hosted run prints the note that records Linux's, batch 13's shape for the same kind of claim. **The other two programs are named doors, because their subject is what the queue and the frame do rather than the shape of the calls that reach them.** `t-rt-signal-queue` rides `libc::sigqueue` (musl's build of it is the 128-byte `SI_QUEUE` record `rt_sigqueueinfo` takes - `si_signo` at 0, `si_code` -1 at 8, the sender at 16 and 20, the value at 24, number 138 - from the same archive), `libc::sigtimedwait` (the same build passes `sigsetsize` 8 and retries `EINTR`), `libc::sigprocmask`, `libc::sigaction` and `libc::kill`, and reads each copy's payload through the machine's own `libc::siginfo_t::si_value()` - the union `t-waitid` pinned on both machines. The one thing a libc *would* have hidden is the signal number, since musl's `sigaction` refuses its own three real-time numbers before the syscall: that is why the three copies under test live at 35, 36 and 37, and `t-rt-sigaction` is where the raw boundary itself is pinned. `t-signal-delivery` keeps exactly one raw door - `SYS_rt_sigpending` with the caller's own 8-byte word - because the claim *is* the wire's `sigset_t` and the `sigsetsize` the caller names (`pending & (1 << (SIGTERM - 1))`, the spelling `t-rt-sigprocmask` pins), while `libc::sigpending` fills the machine's own 128-byte set and blocks-and-restores around the call, so both the size the kernel is handed and the word it writes are outside it (clause (c), the same reason `t-rt-sigprocmask` is raw throughout). Its signal is `SIGTERM`, a fixture choice for the reason the SIGSEGV claim moved: the runtime has opinions about SIGPIPE, SIGSEGV and SIGBUS and none about SIGTERM - what `t-signal-frame` chose `SIGUSR1` for. x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `pass=159 fail=0` (exit 0); aarch64 `116 aarch64 programs built` (was 113) / `113 staged, 3 left out` / `pass=113 fail=0` (exit 0), with `=== RUN t-rt-sigaction` / `t-rt-signal-queue` / `t-signal-delivery` and their `=== PASS` lines in its log; the three programs' per-claim `ok:` lines are **byte-identical on both machines** and equal to their `MPOS_ON_HOST` runs - `t-rt-sigaction` 16 lines, `t-rt-signal-queue` 12, `t-signal-delivery` 16, diffed clean; the host run of `t-rt-sigaction` prints one extra `note:` line for each claim whose host answer differs, which is not an `ok:` line - and `user/abitest` is down to 53 programs (22 syscall plus 31 integration); **Batch 21 (`t-sa-restart`, `t-signalfd`) is the signal family's other two halves - the flag that rewinds an interrupted call, and the descriptor that hands signals over as data - and it is the batch that first moved a fixture because the *claim* was wrong about Linux, then found the kernel defect standing behind it.** `t-sa-restart` is `t-tty-eintr`'s counterfactual made observable: with `SA_RESTART` set, the parked terminal read is rewound and answers the line typed 400 ms after the signal, and with the flag clear the same read answers `EINTR` at the signal and the retry takes the byte - beside the never-restarted pair, `poll` and the sleep, which answer `EINTR` *with* the flag set, and the sleep's remainder pointer, the one thing that makes a retry cost the rest of the sleep rather than the whole of it. Its doors are the named ones: `libc::sigaction` (whose musl build fills the restorer), `rustix::io::read` (a plain `read`, with no `EINTR` retry in its backend, which is what makes the counterfactual a claim), `rustix::event::poll`, and `libc::nanosleep` *for its second pointer* - `rustix::thread::nanosleep` takes only the request, so the remainder has nowhere to go through it. Two wrapper differences are named rather than hidden: rustix's `poll` is `ppoll` on **both** machines (the call `t-poll` pins, `poll` itself, has no arm64 number), and musl's `nanosleep` is not a binding of `nanosleep` - the crate's own `libc.a` shows it calling `__clock_nanosleep(CLOCK_REALTIME, 0, req, rem)`, once, with no `EINTR` loop - so the wait this program pins on both machines is `clock_nanosleep`, which is the one call mpos serves both spellings from. Two numbers no maintained table carries: `ITIMER_REAL` (this libc defines it for fuchsia, hurd, aix and nto and in no `linux_like` file - measured) and `TIOCGPTN`/`TIOCSPTLCK`, which no libc names at all and which are computed the way `asm-generic/ioctls.h` computes them and checked against a compiled probe. The fixture is a pty pair per check, minted by opening `/dev/ptmx` and asking `TIOCGPTN` for its slave, both ends `O_NOCTTY` so a test cannot acquire a controlling terminal by accident - and it is the first pty this suite opens at all. **`t-signalfd` is where the batch's two findings came from, and the first is that the no-std claim was wrong about Linux.** It never blocked `SIGCHLD`, and on this kernel it did not have to; Linux discards a generated signal that is unblocked and whose disposition is the ignoring default, and `SIGCHLD`'s default action is exactly that - measured on this host with the pair one child's exit makes: unblocked, `-1`/`EAGAIN`; blocked, the 128-byte record. So the block became part of the fixture, which is also what the file's own contract for the *other* half already said. With it in place a second divergence showed, and it is a defect rather than a difference of style: this kernel keeps a child's exit in one table and *also* queues a generated `SIGCHLD` when the caller has it blocked, so a descriptor whose mask names `SIGCHLD` hands over **two** records for one death - the queued copy first, `ssi_code` `SI_USER`, then the exit record itself, `CLD_EXITED` with the child's pid (the payload log, verbatim: `note: 2 records for one death - the first carries ssi_code 0`) - where Linux's pending queue is the one table and one death is one record. The program therefore drains the descriptor and claims the *count* (1 under `MPOS_ON_HOST`, 2 on the payload, with the note that names both halves), while the claims about the *exit* record - `CLD_EXITED`, the child's pid, `wait(4)` still collecting - hold on both machines. Both are named in `docs/UAPI.md` §5.2 and not fixed here: the owner is `src/sched/runtime.rs`'s `queue_signal` call, and that file is held by another line of work in this tree. The record itself is the machine's own `libc::signalfd_siginfo` now, its length pinned against the ABI's 128 as a claim of its own (the `t-waitid` move a second time), and the door is `libc::signalfd` - one spelling rather than two, because this libc binds no `signalfd4` at all (measured) and its `signalfd` *is* `SYS_signalfd4` on both machines (crate `libc.a`: 74 here, 289 on x86_64, with the size word 8), where the no-std version's "plain spelling" reached the legacy `signalfd`, which has no arm64 number. x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `pass=159 fail=0` (exit 0); aarch64 `118 aarch64 programs built` (was 116) / `115 staged, 3 left out` / `pass=115 fail=0` (exit 0), with `=== RUN t-sa-restart` / `t-signalfd` and their `=== PASS` lines in its log; the two programs' per-claim `ok:` lines are **byte-identical on both machines** and equal to their `MPOS_ON_HOST` runs - `t-sa-restart` 26 lines, `t-signalfd` 26, diffed clean, with the payload's signalfd run adding the one `note:` line above, which is not an `ok:` line - and `user/abitest` is down to 51 programs (20 syscall plus 31 integration) after batch 21; host tests 516; xtask tests 89; `depgraph` within the ratchet. **The threading pair** (`t-clone`, `t-futex`) is the first batch to find a *kernel* bug rather than a test-side one, and it is an aarch64-only ABI break: Linux's raw `clone` is the one syscall whose last two arguments are not in the same order on every machine - the kernel calls the difference `CLONE_BACKWARDS`, x86-64 taking `(flags, stack, parent_tid, child_tid, tls)` and arm64 `(flags, stack, parent_tid, tls, child_tid)`, which glibc's arm64 `__clone` states in its own comment ("X0:flags, x1:newsp, x2:parenttidptr, x3:newtls, x4:childtid") - and this kernel's handler read the x86-64 order on both. Measured, in this order: `t-clone`'s `CLONE_CHILD_SETTID` claim was refused `EINVAL` on the second machine, because the tid pointer sat in the slot the handler does not look at; the musl thread the program then made faulted at `pc 0x3` on its way out, because `CLONE_SETTLS` had handed it the *tid word's* address as its thread pointer and its exit path walked a TLS block that was never one; and the sweep never reached its end, because the word `pthread_join` waits on was never woken. The fix is `syscall::linux::mod`'s `clone_arguments`, one `swap(3, 4)` under `cfg(target_arch = "aarch64")`, and it is the first thing on this machine that puts a musl thread's tid and thread pointer where the kernel looks for them. Both programs are `std`-side now: `rustix::thread::futex` carries four of the five futex calls (`wait`, `wake`, `wait_bitset`, `wake_bitset`, whose `NonZeroU32` *is* the man page's "an empty bitset is not a wake anyone could answer"), the fifth - the empty bitset's `EINVAL` - rides `libc::syscall(SYS_futex, ..)` because no value of that type is zero, `clone` rides `libc::syscall(SYS_clone, ..)` with the two argument orders reconciled in one helper, and the real thread is `std::thread::spawn` where the no-std version mapped its own stack and entered through a `global_asm` stub - so its "a stack was mapped for the thread" and "the tid was parked where the caller named" checks retired with the scaffold (what they were really reaching for, `CLONE_CHILD_SETTID` parking a tid, is `t-set-tid-address`'s subject). Two claims are named divergences, both measured on this host rather than assumed: an unknown flag bit is `EINVAL` here and reads as `CLONE_NEWNET` there (a new namespace to a privileged caller, `EPERM` to an unprivileged one), and a null tid pointer is refused here where Linux starts the child and skips the write. A third is *avoided* rather than gated, because a request that means different things on the two machines cannot be one claim: the no-std version cloned with a literal zero flag word, and an empty word means "no signal announces the child", which Linux answers by auto-reaping it (`wait4` is `ECHILD`; measured) - so every process-shaped clone here names `SIGCHLD`, the word `fork` is actually built from, and the reaping and exit-code claims are about the same request on both machines. The spawn shape (`CLONE_VM|CLONE_VFORK`) retires to `t-stdspawn`, whose header already names that flag word: under `CLONE_VM` a child shares the caller's address space and therefore the caller's stack, so a raw-syscall child has no frame of its own to return on - that entry-point shuffle is what a libc's `clone` wrapper exists to do, and rebuilding it here would be the scaffolding this migration removes. x86_64 `170 programs built` / `160 staged, 10 left out` / `pass=155 fail=5` (the five are other work in flight - `t-drm-sync` and the tty family), exit 33; aarch64 `123 staged` / `pass=119 fail=4` (the same tty four), reaching `=== END`, with `=== PASS t-clone` and `=== PASS t-futex` in both logs and their per-claim `ok:` lines **byte-identical on the two machines**; **Batch 23 (`t-unix-dgram`) is the AF_UNIX pair's first half - the names that receive whole messages instead of byte streams - and it is the first batch whose claims had to be *measured against Linux* before they could be pinned, because the no-std original had only ever run here (`unix(7)` was its source, not a measurement).** The sockets, names and byte doors are `rustix`'s; the two claims about `recvfrom`'s `addrlen` ride `libc::syscall(SYS_recvfrom, ..)` with a `libc::sockaddr_un`, because the crate parses the address into a `SocketAddrUnix` and does not hand the count back. Running it against a host Linux - the raw-syscall probe is `.workbuddy/probe_unix_dgram.c` - split the claims in three. One got *truer*: the `SEQPACKET` listen claim has always said "*named*", and Linux refuses `listen` on an unbound `SEQPACKET` with `EINVAL`, so the socket is bound first now and the claim holds on both machines where the no-std version only held here. Four answers are this kernel's own, each named in its claim and accepted under `MPOS_ON_HOST=1` with the host's own answer printed: a second `connect` is `EISCONN` here where Linux accepts a repeat connect to the same name; an addressed `sendto` on a connected datagram is `EISCONN` here where Linux takes the address; an unbound sender's name is three bytes here where Linux reports `addrlen` 0; and a datagram aimed at a stream listener's name answers `ECONNREFUSED` here where Linux answers `EPROTOTYPE` - these want `docs/UAPI.md` §5.2 rows, and read like kernel divergences rather than test-side ones. The queue's *depth* (64) is this kernel's number too: Linux bounds the queue by `SO_RCVBUF`, so its count is a byte budget and not a message count, and what holds on both is the shape - a full queue refuses the sender with `EAGAIN` instead of dropping what was sent, and a drained read makes room again. The four tty programs the previous batch left red reached `=== PASS` on both machines in this batch's gate, and the *kernel* reason is worth recording here beside it: `rustix::termios::tcgetattr` asks `TCGETS2` first, `ENOTTY` sent it to the `TCGETS` fallback, and `TCGETS`'s 60-byte structure overran the 44-byte `termios2` the caller had handed it - the last eight bytes written are the two 38400 speeds, so the caller's saved return address became `0x960000009600` on aarch64 (an instruction abort at its own garbage PC, `ESR 0x82000004`) and a quiet stack clobber on x86_64. `src/vfs/termios.rs` gained the `termios2` layout (`store_to2`/`from_ptr2`, with a host unit test pinning the offsets) and `src/syscall/linux/fs/ioctl.rs` the `TCGETS2`/`TCSETS2` families, leaving `TCGETS`'s existing 60-byte contract untouched. x86_64 `170 programs built` / `160 staged, 10 left out` / `pass=159 fail=1` (the one was `t-drm-sync`, other work in flight), exit 33; aarch64 `124 staged` / `pass=124 fail=0`, reaching `=== END`, with `=== PASS t-unix-dgram` in both logs and its per-claim `ok:` lines identical on the two machines; host tests 517; `user/abitest` was down to **43** programs (12 syscall plus 31 integration). **Batch 24 (`t-unix-scm`) is the AF_UNIX pair's other half - the descriptors and the identities a unix socket carries - and it is the batch that found a second `kernel` bug, this time in a flag's *value*, by spelling it from `linux/socket.h` instead of from this kernel's own table.** Every door that is not a control message is `rustix`'s (`socketpair`, `pipe`, `sockopt::{socket_peercred, socket_passcred, set_socket_passcred, socket_type}`, `fcntl_getfd`/`dup`, `event::epoll`, `io::{read, write}`); the control chain stays on the raw door, because half the claims *are* numbers on the wire - `msg_controllen == CMSG_SPACE(4)`, `cmsg_len == CMSG_LEN(12)`, `msg_controllen == 0`, `MSG_CTRUNC` at `sizeof(cmsghdr)` of room - and `rustix`'s `SendAncillaryBuffer`/`RecvAncillaryBuffer` compute those numbers internally and hand back only the parsed messages, so a claim about the number cannot be made through them. The chain's header is also read field by field rather than through `libc::cmsghdr`, because musl's `msghdr` carries a `socklen_t` control length where glibc's carries a `size_t` while the kernel writes one layout either way. (One helper turns libc's `-1`/`errno` back into the negative errno the claims are written against, which is the shape a bare syscall hands back.) **The bug:** this kernel's `MSG_CMSG_CLOEXEC` was `0x10000` - Linux's `MSG_WAITFORONE`, the bit `recvmmsg` reads for something else entirely, and the bit `src/net/socket.rs`'s own `recvmmsg` already honours. Linux's is `0x40000000`. The no-std suite's `lin::MSG_CMSG_CLOEXEC` carried the same wrong number, so a test that read the bit out of *either* table agreed with the other and the flag was never exercised from `linux/socket.h`: a caller asking Linux's way got no close-on-exec on the received descriptor, and a caller asking `0x10000` got it while also asking `recvmmsg` for `MSG_WAITFORONE`. One constant in `src/net/socket.rs` (and the two comments that had the neighbouring bits wrong) plus `lin.rs`'s copy, and the migrated claim - written from `libc` - is what stands behind it. One *divergence* joins batch 23's four: a data-less `SCM_RIGHTS` on a stream socketpair is accepted and queues nothing on Linux, so a blocking `recvmsg` **parks** there (measured twice - `.workbuddy/probe_unix_scm.c` and, before that probe existed, a host run that had to be killed), where this kernel delivers the record with a zero-byte return; that section is accepted under `MPOS_ON_HOST=1`, where it makes the non-blocking read and prints the host's answer. x86_64 `170 programs built` / `160 staged, 10 left out` / `pass=159 fail=1` (the one is `t-drm-sync`, other work in flight), exit 33; aarch64 `125 staged` / `pass=125 fail=0`, reaching `=== END`, with `=== PASS t-unix-scm` (77 claims) and `=== PASS t-unix-dgram` in both logs; host tests 517; **Batch 25 (`t-prctl`, `t-seccomp`)** is the process-control pair, and it found two kernel bugs. `t-prctl` (32 claims) keeps the option table honest - `rustix` for the name pair, the keepcaps pair and `secure_computing_mode`, the raw door for the dumpable and bounding-set options and for the option *numbers*, which are themselves half of every claim. The bug is a **wrong constant**: this kernel wrote `PR_GET_KEEPCAPS` as **9** - the setter's 8 plus one, the shape a reader expects - where `linux/prctl.h` says **7** (9 is `PR_GET_FPEMU`), and `lin.rs` carried the same 9, so the no-std suite and the kernel agreed on a number no Linux program sends and a caller using the real `PR_GET_KEEPCAPS` was refused `EINVAL`. `rustix::thread::get_keep_capabilities` sends 7, which is what failed against the unfixed kernel; one constant in `syscall::linux::abi` is the fix, and the claim that 9 is *not* the getter is pinned beside it (measured: 9 is `EINVAL` on Linux too). `t-seccomp` (37 claims) is the filter half - `rustix` binds none of `seccomp(2)`, so the BPF program, the eight actions and `struct sock_fprog` come from `libc`'s uapi mirrors - and it found a **stale architecture gate**: the enforcement block in `syscall::linux::dispatch` was `#[cfg(not(target_arch = "aarch64"))]`, with a comment saying no filter could be *installed* there until `seccomp(2)` was ported. That stopped being true when `posix` grew the handler for both machines, and the leftovers were exactly the shape that hides: installing answered 0 and `PR_GET_SECCOMP` answered 2 on aarch64, so both halves looked right while **nothing ever judged a call**. Removing the gate is the fix, and the second machine now enforces filters. Two more things the port had to get right, both measured rather than assumed: the *judged number* comes from `libc::SYS_getpid`/`SYS_getppid` instead of the literal 39/110 the no-std file wrote down (aarch64's are 172/173, and this crate *is* staged on the second machine, so a literal would have installed a filter judging a call nothing makes); and the judged *call* is the raw `syscall`, because a libc wrapper is a second thing that can answer - under a filter denying `getpid`, `syscall(SYS_getpid)` answers `-1`/`EPERM` while `getpid()` answers `-1` with `errno` left at **0** (measured on a host Linux, `.workbuddy/probe_seccomp_self.c`), and the first draft's wrapper call reported five false failures about filters that were working. One divergence joins the list: `SECCOMP_GET_ACTION_AVAIL` for `USER_NOTIF`, which this kernel refuses (no supervisor to summon) and a host Linux serves, accepted under `MPOS_ON_HOST=1` with the host's answer printed. x86_64 `160 staged` / `pass=159 fail=1` (the one is `t-drm-sync`, other work in flight), exit 33; aarch64 `127 staged` / `pass=127 fail=0`, reaching `=== END`; `=== PASS t-prctl` and `=== PASS t-seccomp` in both logs with identical per-claim counts; host tests 517; **Batch 26 (`t-times`, `t-timer-create`)** is the time pair - CPU accounting and POSIX interval timers - and it found **two more argument-shape bugs**. `t-times` (20 claims) pins that `times()`/`getrusage()` measure real CPU: a user spin raises `tms_utime`/`ru_utime` and leaves `stime` behind (the ring split is the point), the elapsed clock advances strictly across a 50 ms sleep and only monotonically across a spin, `tv_usec` is microseconds, an unknown `getrusage` who is `EINVAL`, and a reaped child's CPU folds into `RUSAGE_CHILDREN` and `times()`'s `tms_cutime`. `rustix` binds neither call, so both go through the raw door - and the migration found that **`libc`'s `rusage` is not the uapi's record**: measured, it is **272** bytes on this target, a generic-unix superset of thirty `long`s where `linux/resource.h` has fourteen, so the 144-byte record is spelled in the test the way `t-unix-scm` spells `cmsghdr`, with a compile-time size assert (a size claim about `libc::rusage` would be false rather than tight). One no-std claim was also **vacuous** and is now real: `[0u8; 144]` was declared zero and checked all-zero after the call, which a kernel that wrote nothing would also pass; the migrated claim seeds the record with `0xAA` first, so what it pins is the kernel's write. `t-timer-create` (45 claims) is the family itself, in the shape musl builds for `SIGEV_THREAD` (a thread blocked in `rt_sigtimedwait` plus a `SIGEV_THREAD_ID` timer carrying `SIGRTMIN`); `libc` binds no `sigevent`, `itimerspec`, `timer_t`, `SIGEV_*` or `SI_TIMER` for linux at all (measured: those exist in the crate for fuchsia, hurd, aix and vxworks only), so the three wire records are spelled at the uapi's offsets with compile-time size asserts and every call is raw. **Bug one: `SIGEV_SIGNAL` is 0 and `SIGEV_NONE` is 1** in `linux/signal.h`; the kernel had them the other way round, and `lin.rs` carried the same swap, so the suite and the kernel agreed with each other and with no libc and the suite passed. What that cost a real caller: the true `SIGEV_NONE` (1) was read as `SIGEV_SIGNAL`, and since a silent timer leaves `sigev_signo` at 0 it was refused **`EINVAL`** - a `timer_create` that could never succeed - while the true `SIGEV_SIGNAL` (0) got `SIGEV_NONE`, a timer that silently never fired. The two claims that name the numbers are what found it. **Bug two: `getrusage` compared the whole 64-bit `who`.** `RUSAGE_*` are `int`s in the uapi and Linux's syscall wrapper reads one as the register's low 32 bits, sign-extended - so `RUSAGE_CHILDREN` arrives as `0x0000_0000_ffff_ffff` from every compiled call and every `syscall(2)` vararg, and as `0xffff_ffff_ffff_ffff` only from a caller that sign-extended it itself. The handler compared the full word, so a libc's ordinary `getrusage(RUSAGE_CHILDREN, ..)` was refused `EINVAL` on **both** machines; the no-std suite spelled the sign-extended form and never saw it. The fix truncates to the `int` the ABI names, and it surfaced in the first guest run of the migrated test. Three divergences join the list, all measured: `CLOCK_PROCESS_CPUTIME_ID` answers `EOPNOTSUPP` here where Linux serves it; `TIMER_ABSTIME` is refused (one clock, no absolute domain) where Linux arms it; and a **deleted timer's already-queued record** stays takeable here, where a host Linux frees the `sigqueue` - `sigpending` still shows the bit while the take answers `EAGAIN` (`.workbuddy/probe_timer_delete.c`). The first draft of that take used a *null* timeout and hung on the host for exactly that reason: the polling form (a zero timeout) is what the no-std file used and what both machines answer, and a claim that can hang is worse than one that fails. x86_64 `160 staged` / `pass=159 fail=1` (the one is `t-drm-sync`, other work in flight; `t-clock-gettime` flaked red once - a monotonic-monotonicity claim - and passed on the immediate re-run); aarch64 `129 staged` / `pass=129 fail=0`, reaching `=== END`; `=== PASS t-times` and `=== PASS t-timer-create` in both logs with identical per-claim counts; host tests 517. **Batch 27 (`t-wait-eintr`)** migrated the last of the blocking-wait family: the ten nodes a catchable signal must end - a pipe's read *and* its write, an eventfd, a signalfd, an inotify watch, a unix datagram queue, a unix accept queue, a pty slave's write into a full master, and `wait4`/`waitid` - each pinned twice (the `EINTR` with the handler run, then the positive control that the retried call answers when the thing it waited for arrives), with the two writes on the partial-count rule rather than `EINTR`. The nodes are `rustix`'s (`pipe`, `eventfd`, `inotify`, `socketpair`, `socket`/`bind`/`listen`/`accept`) and the reads/writes are `rustix::io`, which has no `EINTR` retry in its backend; `wait4` is `rustix::process::waitpid`, `waitid` the raw door `t-waitid` opened, and the one node no library binds here is `signalfd` (this libc binds neither spelling), so its descriptor is the raw `SYS_signalfd4` and its delivery the raw `SYS_tgkill`. The pty fixture is `t-pty`'s `mint` (returning `Option`, not a `panic!`): a host with no `devpts` cannot build it, so there the four pty-write claims are named and not made (guarded by `host_run`), while on the kernel a missing pair is a failed check. x86_64 `160 staged` / `pass=159 fail=1` (the one is `t-drm-sync`, other work in flight; `t-clock-gettime` flaked red once and passed on the immediate re-run, the same monotonic flake as batch 26); aarch64 `130 staged` / `pass=130 fail=0` reaching `=== END`; `=== PASS t-wait-eintr` in both logs with the **same 55 claims** on each (the pty-write four among them on both, so the guest minted a pair); host tests 517; `user/abitest` is down to **37** programs (6 syscall plus 31 integration). **Batch 28 (`t-getdents64`)** moved the raw directory stream: the syscall is the raw door `libc::syscall(SYS_getdents64, ..)` because the record's *shape* is what half the claims are - `d_reclen` strides the buffer and `d_type` labels each name, which a wrapper handing back parsed entries would hide - and `SYS_getdents64` is `libc`'s own per-machine number (217 here, 61 on the second machine), the last no-std literal this program carried. The wire layout is no longer hand-written: the field offsets come from the machine's own `libc::dirent64` (`offset_of!`), pinned against the uapi's numbers 0/8/16/18/19 as compile-time claims, so a drifting `dirent64` is a build failure rather than a silently wrong stride. **One claim moved rather than migrated**: the no-std version wrote the busybox layout down ("sh" and "cat" answer `DT_LNK`, root `/bin` answers `DT_DIR`) - a fact about the root the program happens to run in, false on a merged-`/usr` host where `/bin` is a symlink to `usr/bin` - so what is pinned now is the property a client's `ls -F` actually compares: the kernel's `d_type` **is the type `lstat` reports** (`symlink_metadata`, which answers for the link and not its target), and both names are *listed* at all. The rest is the named calls: the opens and the rewind are `rustix::fs::{open, seek}`, `EINVAL` for a buffer too small for one record, and the lowest free descriptor discovered with `rustix::io::dup` rather than assumed to be 3. x86_64 `160 staged` / `pass=160 fail=0` (the count does not move - the program was already in the sweep from the abitest side; this batch only changes which crate it rides in); aarch64 `131 staged` / `pass=131 fail=0` reaching `=== END` - **the first run of this program on the second machine**, which the aarch64 sweep's abitest-only staging had never carried; host tests 517; `user/abitest` is down to **36** programs (5 syscall plus 31 integration). **Batch 29 (`t-syslog`)** moved the kernel log's own door, and the host measured the one thing this migration could not measure anywhere else: **a gate this kernel does not have**. The door is the raw door `libc::syscall(SYS_syslog, ..)` (`libc`'s own per-machine number, 103 here and 116 on the second machine) because every answer is the number the door itself gives; the sixteen claims are the no-std file's, unchanged - the counters (`SIZE_UNREAD` a count, `SIZE_BUFFER` the 16 KiB this kernel designs in), the non-destructive read-all asked twice, the null-buffer `EINVAL`s, the console-level validation 1..=8, the five bookkeeping types, and a type outside the eleven. The probe (`.workbuddy/probe_syslog.c`) cannot get past real Linux's privilege check: this sandbox's host answers `EPERM` for *every* type - the counters included - because `dmesg_restrict` is on and a container's root holds no `CAP_SYSLOG` in the initial user namespace, and the gate comes *first*: even an unknown type (99), which this kernel answers `EINVAL`, gets `EPERM` from the host. So the host run (`MPOS_ON_HOST=1`, the named divergence: Linux validates permission before it validates anything, and this kernel has no privilege check at all) accepts the `EPERM` the host actually gave on every check; the gate keeps pinning this kernel. x86_64 `160 staged` / `pass=160 fail=0` (again the count does not move - the program was already in the sweep from the abitest side); aarch64 `132 staged` / `pass=132 fail=0` reaching `=== END`, `=== PASS t-syslog` in both logs with the **same 16 claims**, the `SIZE_BUFFER` 16 KiB pin among them on both - so the second machine's kernel keeps the same ring capacity by the same design; host tests 517; `user/abitest` is down to **35** programs (4 syscall plus 31 integration). **Batch 30 (`t-maps`, plus the `t-process-vm` removal)** is two moves in one: the `t-process-vm` no-std original was lifted out outright - its std-side successor `t-process-vm-libc` predates this batch and covers every claim the original made (the handshake, the guard-page and no-page `EFAULT`s, the `ESRCH` pids, the `EINVAL` flags word) plus several the original never had (the cursor walk, the 1024 iovec cap, the null-array `EFAULT`) - and **`t-maps`**, the `/proc/self/maps` reader, moved properly. The format is the whole contract, so the no-std parser moved with it on purpose: six whitespace fields, lowercase hex addresses, the offset zero-padded to eight digits, and the name starting in column 73 - re-measured on this sandbox's host (`.workbuddy/probe_maps.c`), where every named line starts its name there and a nameless line stops after the inode, exactly as this kernel prints them. The probe measured **two divergences**. The first is the shared anonymous object's *name*: this kernel spells it the tmpfs way, `/memfd: (deleted)`, where the host's Linux names the very same mapping `/dev/zero (deleted)` - so the pin keeps the property (a shared anonymous mapping is *named*, with a nonzero inode) and a host run accepts the host's spelling beside this kernel's. The second is a width the no-std file had pinned as a number: `dev` is `%02x:%02x`, and the host's ext4 answers `103:06` - six characters - where this kernel's device numbers are always small enough to pad to `00:00`; the shape check now pins the format (two hex digits a side, at least) rather than the width. Everything else the probe measured agrees: `rw-p`/`rw-s`, the bracketed `[stack]` and `[heap]` covering this frame and the old break, the image under its own path at `r-xp`, a memfd named after its memfd in the name column, and a private anonymous mapping with no name, no inode and `00:00`. The fixture is the named calls (`mmap_anonymous`, `memfd_create`+`ftruncate`+`mmap`, the raw `SYS_brk` of `t-brk`, `read_link`); x86_64 `159 staged` / `pass=159 fail=0` (one down from batch 29 - the `t-process-vm` original's removal is the one, and the std `t-maps` takes the `t-maps` original's place); aarch64 `133 staged` / `pass=133 fail=0` reaching `=== END`, `=== PASS t-maps` in both logs with the **same 28 claims**, the `/memfd: (deleted)` pin among them on both - the second machine's ring spelling agreed with the first; host tests 517; `user/abitest` is down to **33** programs (2 syscall plus 31 integration). **Batch 31 (`t-ptrace-attach`)** moved the attach/detach test whose whole point is the pid source the other tests never exercise: a target the caller is *not* related to - `strace -p`, built here as a forked child that forks in turn and exits, so the grandchild is reparented away without ever touching pid 1 (an attached init means nothing reaps and the machine stops moving). The constants moved from literals to `libc` - `SYS_ptrace` is the per-machine number the no-std file had pinned as x86_64's 101 (117 on the second machine), and the requests and signals are `libc::PTRACE_*`/`SIG*` - the exact migration the aarch64 sweep exists to catch; the sleeps are `rustix::thread::clock_nanosleep_relative`, and the bounded `WNOHANG` polls that replaced the wait that once hung the machine for ten minutes are kept. The probe (`.workbuddy/probe_ptrace.c`) measured **a door that does not open at all**: this sandbox's container seccomp profile answers `EPERM` for *every* `ptrace` request before the pid is resolved - attaching to the caller's own child, which yama's scope 1 would allow, fails the same way - so a host run measures nothing about the door, and the ptrace claims accept that `EPERM` under `MPOS_ON_HOST`, named as such. What the probe *did* measure is everything around the door, and it agrees on all of it: the user's `SIGSTOP` is reported through `WUNTRACED` (`WIFSTOPPED`), a plain `detach(0)` leaves that stop alone, and the user's own `SIGCONT` is what ends it - the split a debugger relies on, pinned on both machines by the non-ptrace half. x86_64 `159 staged` / `pass=159 fail=0` (the count does not move - the program was already in the sweep from the abitest side); aarch64 `134 staged` / `pass=134 fail=0` reaching `=== END`, `=== PASS t-ptrace-attach` in both logs with the **same 15 claims** - the non-descendant attach among them, which the aarch64 sweep had never carried; host tests 517; `user/abitest` is down to **32** programs (1 syscall plus 31 integration). **Batch 32 (`t-posix`)** moved the largest and the last of the syscall family, and the probes (`.workbuddy/probe_posix.c`, `probe_seccomp_mode.c`, `probe_diag.c`) earned their keep: **musl's `libc` wrapper protocol differs from glibc's** (a failure is a plain `-1` with the errno beside it, not a negative errno - every wrapped call whose errno a claim pins now travels through one `rc()` that restores the `-errno` spelling the no-std file's stubs answered in), and **this target's `libc::sched_getscheduler` wrapper answers `ENOSYS` where the raw door succeeds** (measured: the raw `sched_getscheduler(0)` answers 0 in the same process) - so all four scheduler doors are raw. The probe measured **four host divergences, each a gate or a service this kernel does not have**: the host's `setresuid(-1, 1, -1)` answers `EINVAL` (raw and glibc alike) where this kernel lowers the effective id, so the whole sudo dance and the claims that depend on it are named and not made on a host run (`t-pty`'s mint rule) while the gate keeps pinning all of them; `personality(0x1000)` the host stores where this kernel validates the table; `vhangup` the host answers `EPERM` for (`CAP_SYS_TTY_CONFIG` first, the klogctl-shaped gate) where this kernel pins the no-terminal `ENOTTY`; and `seccomp(STRICT)` the container refuses to install outright (`EFAULT`, flags or no flags) so the strict child's death shape is unmeasurable there - the child's early exit is accepted, named. `unshare(CLONE_NEWNS|CLONE_NEWTIME)` the host *serves* to root where this kernel keeps its deliberate `EOPNOTSUPP`; `getcpu` the host answers with the core it really ran on. Everything else the probe measured agrees: the fresh-process identity words, the capability words (`ffffffff`/`0x1ff` from `CAP_LAST_CAP`, nothing inheritable, the `capset` round-trip), the raw `getpriority` encoding 20..1 and its clamp, `GET_ACTION_AVAIL`'s `EOPNOTSUPP` with the caller's word untouched, the namespace refusals and their ordering, `setns`, `tgkill`'s probes, and every answer of the memory advice family including the freshly-mapped-not-resident pin. `libc::CAP_LAST_CAP` and `libc::tgkill` do not exist on this target - the constant is the uapi's value (40) and the call the raw door. x86_64 `159 staged` / `pass=159 fail=0` (the count does not move - the program was already in the sweep from the abitest side); aarch64 `135 staged` / `pass=135 fail=0` reaching `=== END`, `=== PASS t-posix` in both logs with the **same 99 claims** - the whole family, first time on the second machine; host tests 517; `user/abitest` is down to **24** programs (**0 syscall** plus 24 integration; batches 34-36 moved t-epoll-socket, t-memfd-shm, t-mountinfo, t-aslr and t-uevent) - **the syscall family is fully migrated**; the integration half is under way | **low per batch, large in total** - 29 programs remain, and each batch must first classify whether the second machine *serves* the call (a genuine gap is named in `tests.toml`'s `aarch64` group, never silently dropped). *Batch 33 (`t-sock-flags`, `t-fpu-across-switch`)*: the first integration batch, and both of its programs ran on the second machine for the first time - `t-sock-flags` pinned `recvfrom`'s number there (`207`, against `45` on the first) and got the same `EOPNOTSUPP`, which is the whole reason the sweep stages only `user/tests` on that arm. Two things a migrated integration program has to decide that a syscall one does not: **which register the claim is about** - `t-fpu-across-switch` pins `xmm7` on x86_64 but `v15.d[0]` on aarch64, because this kernel's switch saves `q8..q15` and not `q0..q7` (AAPCS64 leaves the low eight caller-saved and the kernel is entitled to use them as scratch), so pinning `v0` there would fail for a reason that is not the bug - and **whether a claim can park**: Linux implements `MSG_PEEK` and waits on an empty socket, this kernel refuses it, so a host run sets `O_NONBLOCK` before the first claim and accepts `EAGAIN` beside the refusal rather than hanging. Both programs: 4 claims, identical on both machines. *Batch 34 (`t-epoll-socket`, `t-memfd-shm`)*: the first integration batch that had to ask what a *door* is worth on each machine. `t-epoll-socket` (9 claims) needs a `SOCK_RAW` socket, and the host has no `CAP_NET_RAW` (measured `EPERM`, `probe_rawicmp_epoll.c`) - so on a host run there is nothing to watch and the program says so and finishes with **no claims** rather than inventing one. On the guest it is 9/9 on both machines, which is the first time an *IP* wakeup of a parked `epoll_wait` has been pinned on the second machine at all. `t-memfd-shm` (18 claims) has exactly one divergence - `MFD_HUGETLB`: this kernel refuses it `EOPNOTSUPP` rather than answering with an ordinary memfd the caller believes is huge-backed, current Linux creates one (`probe_memfd.c`), and the claim names that instead of pinning Linux's. Everything else - the closed flag vocabulary (`0x1000` is `EINVAL` on both), both mappings of one fd aliasing, `write(2)` reaching the mapping, the seal round-trip, and the `SCM_RIGHTS` handover whose child reads the parent's marker - is the same answer on both, which is what makes that one line the whole divergence. One bug the migration caught in itself: a `check` written *between* `fork` and `if pid == 0` is printed by both processes, so the child's copy of the report is not the one judged - the claim about the fork now follows the child's branch. *Batch 35 (`t-mountinfo`, `t-aslr`)*: **one new gap named, in a place nobody had looked.** `t-mountinfo` (19 claims) is the same answer on both machines except for the two files whose honest content is "this machine has none": `/proc/modules` is empty here and 11 KiB on a host with modules, `/sys/bus/usb/devices` is empty here and lists 31 there - both named, since the claim is that the file *exists* and reads as nothing, which is what `lsmod`/`lsusb` need, not that the machine has no modules. Everything else about the file holds identically on a current host: 119 lines, every one of them ten fields, the `-` at field seven, and field three a `maj:min` equal to the mountpoint's `st_dev`. `t-aslr` (5 claims) turned up the gap: the code address is fixed on both (this image is linked for the addresses it names, measured the same on a host), and the stack and the anonymous mapping are drawn on both - but **the heap anchor is not drawn here**: `brk(0)` comes back just above the image every run, where Linux randomises it (measured at a spread of megabytes, `0x1f99e` against `0x265ff` in one pair of runs). No program's correctness rests on it - a heap that moves is hardening, not a contract - so the claim names which answer a machine gets and leaves the gate pinning mpos's; closing it is a change to what this kernel randomises (`NOTES.md` §12.2's three landmarks) rather than a fix to a wrong number, and it is written down here as the thing to decide rather than as a divergence to accept forever. *Batch 36 (`t-uevent`)*: 23 claims, identical on both machines, and the second machine has never run this program before. Two things it cost to get there. **A precondition became a claim.** The blocking `recvmsg` this test exists for is only answerable if there is a device whose sysfs `uevent` file can be written to announce it - `udevadm trigger`'s door, so no device has to appear - and a machine without one does not answer `EAGAIN`, it answers *nothing*: the C probe written to measure the host hung there until it was killed, and the same call in a sweep would have taken the whole run with it. So the file's presence is now checked and claimed before the wait is attempted, and that is also the host's divergence - a host has neither that path nor the right to unicast between two monitors (`EPERM`, measured), so a host run says so and claims nothing. **The musl error protocol bit a third program.** Five of the twenty-three claims - the `EAGAIN` on an empty queue, `ECONNREFUSED`, both `EINVAL`s, and the sender's own broadcast - compare against a negated errno, and `libc::sendmsg`/`recvmsg` answer a plain `-1` with the errno beside it on this target; they all read `-1` and failed on both machines identically, which is the signature of a protocol mistake rather than a kernel one. One `fix()` at the door, the same one `t-posix` and `t-epoll` carry | **Between batch 36 and batch 37, the second machine's own gaps were closed instead of migrated past** (the user asked for the aarch64 hole to be filled before more migration). An audit - an Explore agent over the whole tree, every finding re-verified by hand - turned up four real ones, of which three were closed: **the evdev/uinput ioctl gates** (the 'E' and 'U' families plus three gates in `file.rs` were `#[cfg(not(target_arch = "aarch64"))]` while the machine itself has carried an input registry, `/dev/input/event0` and `/dev/uinput` all along - stale gates of exactly the shape the batch-25 seccomp one was), **the fbdev ioctl gates** (five commands behind the same shape of gate; the machine publishes `/dev/fb0` unconditionally), and **`/dev/dri` itself** (`card0`/`renderD128` were published only under `cfg(target_arch = "x86_64")` and the 'd' family was gated too, though the aarch64 render task has been publishing its card since it was ported). The boot check now resolves `/dev/dri/card0` through the mounted `/dev` on the second machine (`M8_GPU_OK` names it), and the same fixing pass found a real bug beside the gates: aarch64's `check_input` returned after the first MMIO window, so only one of the machine's two virtio-input devices ever registered - and it would have been `event0` twice, because `input::register` numbers nodes by the length of the list a single call passes; it now collects every window and registers them in one call, and the boot log shows `event0` (tablet) and `event1` (keyboard) with `M8_INPUT_OK` counting 2. The fourth gap stays open rather than faked: aarch64's debug-exit - QEMU 11.1.2's `virt` machine has no debug-exit device to attach and the second machine's harness reads the `=== END` marker rather than an exit code, so `mpos_qemu_debug_exit` still falls back to `halt()` there, recorded as blocked rather than worked around. Gates: aarch64 `143 staged, pass=143 fail=0`; x86_64 `160 staged, pass=160 fail=0`; no dead-code warnings on the newly opened paths. |
| 15.12 | **each configuration file describes both machines, in three sections.** `machine.toml` carried the second machine's values as `aarch64_`-prefixed keys beside the shared ones (`config.rs::take_for`), and `rootfs.toml`/`initramfs.toml` split the two machines across *files* (`rootfs-aarch64.toml`, `initramfs-aarch64.toml`). Each of the three is now one file with `[default]` (the baseline both machines read) and `[x86_64]`/`[aarch64]` (each machine's own half, overriding it), and keys are dotted by what they belong to (`qemu.memory`, `rootfs.arch`, `system.hostname`, `user.name`). The prefix helpers are gone: `config.rs::parse_sectioned_toml` overlays the baseline with that machine's section, and a key written before any header - or under a header that is not one of the three - is **refused** rather than skipped, because a `[qemu]`-style leftover of the flat format would drop its keys in silence and boot a machine on defaults. `RootfsConfig::load`/`InitramfsConfig::load` take the `Arch` now and every caller names `rootfs.toml`/`initramfs.toml` (the four in `aarch64.rs`, the Alpine stamp input included). **The effective values did not move**, which is the whole claim, and two had to be written *explicitly* into the second machine's half because an absent key would inherit the first machine's: `rootfs.base_packages = []` (e2fsprogs/dosfstools belong to the guest that makes filesystems) and `user.name = ""` (the desktop account is the first machine's; the second logs in as root). **The one cost, named rather than hidden**: both machines' stamps now compare against the one `rootfs.toml`, so an edit to either half re-installs both trees - the trade that split the files in the first place | x86_64 `user tests: 169 programs built` / `159 staged, 10 left out` / `=== SUMMARY pass=159 fail=0`, `X86_EXIT=0`; aarch64 `--arch aarch64` `44 aarch64 programs built` / `42 staged, 2 left out` / `=== SUMMARY pass=42 fail=0`, `AARCH64_EXIT=0`; `boot-aarch64` `EXIT=0` through `INIT_EXITED`; `alpine-aarch64` `EXIT=0` on `mpos login:` (34 packages, 16.1 MiB); `cargo xtask disk` reads the first machine's half and reports the existing 2G image; `cargo test -p mpos-xtask` **83 passed**, two of them new - the parser's two refusals, and `the_shipped_files_answer_each_machine_its_own_values` (memory 2048/256, `display` absent vs `none`, `extra_args` `["-vga","none"]` vs `[]`, arch, serial port, root device, base and extra packages, `user.name`); host tests 514; `depgraph` within the ratchet; `fmt --check` clean | **medium** - the parser's semantics changed, so the only real proof is that every booted machine answers exactly what it answered before |
| 15.13 | **the resource-limit table was indexed by this kernel's own numbering, and the migration that reads its numbers from a maintained table is what found it.** `getrlimit`/`setrlimit`/`prlimit64` speak `asm-generic/resource.h`'s `RLIMIT_*` numbers, and `crates/abi`'s `default_rlimits` held the descriptor ceiling on row `5` (Linux's `RLIMIT_RSS`) and the process count on row `7` (Linux's `RLIMIT_NOFILE`) - so a caller asking for `RLIMIT_NOFILE` read the process-count budget, `RLIMIT_RSS` reported the descriptor ceiling, and `setrlimit(RLIMIT_NOFILE)` wrote a row nothing enforced. `/proc/<pid>/limits` carried the same slip: fourteen rows where Linux has sixteen, shifted by it, so `ulimit` read the wrong names too. It was invisible because the one program that touched it (`t-prlimit64`) had the same number written into it (`lin.rs`: `RLIMIT_NOFILE = 5`) - which is exactly the failure mode S6 exists to remove, a test whose constant comes from the kernel it is checking and so cannot disagree with it. Measured against this host's own `/proc/self/limits` (Linux's sixteen rows, in `RLIMIT` order) **before** the change; the fix moves the row, the `resource == 7` fd-ceiling sync in `src/sched/runtime.rs`, and the procfs label table | `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` 514; `cargo test -p mpos-abi --target x86_64-unknown-linux-gnu` incl. `default_rlimits_budgets_stack_nofile_and_nproc`, which now also asserts row `5` is unbudgeted - the assertion that keeps the two rows from swapping back; `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` 83; `cargo xtask test` `pass=159 fail=0`; `--arch aarch64` `pass=51 fail=0`; `boot-aarch64` `EXIT=0`; `alpine-aarch64` `EXIT=0` on `mpos login:`; `depgraph` within the ratchet; `fmt --check` clean | **one row in three files** - and the shape S6 is for: the migration did not find a missing call, it found a *number* this repository had written down from its own table and then written a test against |
| 15.14 | **`setpriority` clamps an out-of-range nice; this kernel refused it.** `setpriority(which, who, prio)`'s nice is clamped to `-20..=19` by Linux (`if (niceval < MIN_NICE) niceval = MIN_NICE;`, `kernel/sys.c`), and the man page lists `EINVAL` under ERRORS only for a `which` that is none of `PRIO_PROCESS`/`PRIO_PGRP`/`PRIO_USER`. Measured on a current host: `setpriority(PRIO_PROCESS, 0, 20)` answers `0` and leaves the task at `19`, and `-21` answers `EPERM` - a *privilege* refusal for the raise the clamp turns it into, which a range check running first would have answered `EINVAL` before any privilege was consulted. This handler answered `EINVAL` for both, so a caller that asked for a nice outside the range was told the request was malformed when Linux would have taken it and picked the nearest value. The asymmetry with the modern door is Linux's and is now pinned too: `sched_setattr` *does* refuse an out-of-range `sched_nice` (`EINVAL`), so the two doors onto one nice slot disagree, and `t-priority` says so. Two programs pinned the old refusal - the no-std `t-priority` (migrated in the same commit) and `t-posix` - which is why this is one commit and not two | `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` 514; `cargo xtask test` `pass=159 fail=0`; `--arch aarch64` `pass=54 fail=0`; `boot-aarch64` `EXIT=0`; `alpine-aarch64` `EXIT=0` on `mpos login:`; `depgraph` within the ratchet; `fmt --check` clean | **one range check** - found by reading the man page's ERRORS list rather than its DESCRIPTION, and confirmed by measuring the host: the probe is `setpriority(PROCESS, 0, 20)` answering `0` where this kernel answered `-EINVAL` |
| 15.15 | **`F_SETFD` masks its word to `FD_CLOEXEC`; this kernel answered `EINVAL` for anything else.** POSIX says `F_SETFD` sets the descriptor's flags, and Linux's `set_close_on_exec` reads `arg & FD_CLOEXEC` - so a word with other bits in it is *taken*, and the one bit it names is what the descriptor keeps. The no-std `t-fcntl` pinned the refusal ("F_SETFD with bits beyond FD_CLOEXEC answers EINVAL"), which is a claim about this kernel rather than about the ABI - and the migration's rule is that a claim's answer comes from a maintained table, with the host measured when the two disagree. Measured **through the raw syscall**, so that no libc's masking can be mistaken for the kernel's: `fcntl(1, F_SETFD, 0xFFFFFFFF)` answers `0` and `fcntl(1, F_GETFD)` then reads `1`, while `F_SETFD(1, 0xFFFFFFFE)` answers `0` and clears it. This is the shape `AGENTS.md` §3 warns about, a caller that builds the word from a variable taking `EINVAL` and its fallback path where Linux succeeded. Owner: `src/syscall/linux/fs/file.rs`'s `F_SETFD` arm, which now masks. The migrated claim is rewritten to Linux's answers and made *stronger* than the original: its second half reads the stored word back, so the three possible implementations are told apart - refused (`EINVAL`), stored whole (`0xFFFF_FFFF`), and masked to `1`. | `cargo xtask test` `pass=159 fail=0`; `--arch aarch64` `pass=57 fail=0`, with `t-fcntl`'s ten `ok:` lines identical on both machines; both bare-metal `cargo check`s clean; host 514; `fmt --check` clean | **one mask** - the third kernel bug the migration has found by asking where a claim's answer came from, and the second found in a batch's own subject rather than in a maintained table |
| 15.16 | **Two divergences this batch's pins surfaced and did not fix - named, with their measurements, so neither is rediscovered as a shape.** (a) **`F_GETFL` reports no access mode.** Linux ORs the descriptor's access mode into the word `fcntl(F_GETFL)` returns, so a descriptor opened `O_WRONLY` reports `1` (measured on this host); this kernel reports the description's stored status word, whose access bits are always zero, and keeps the mode separately (`src/vfs/file.rs`'s `ACC_*`, which only the read/write `EBADF` refusal reads). So a caller that reads `F_GETFL` to learn which way a descriptor opens is told `O_RDONLY` for every one of them - the wrong *shape* rather than an absent call. It is not fixed here because it is a *stated* design ("opens here are untyped", the arm's own comment) and because folding the access bits into the reported word has a blast radius this batch did not measure: `rustix::fs::Dir` reuses an `F_GETFL` word for the `openat(fd, ".")` it opens a directory's descriptor with, so the change reaches every directory reader on both machines first. `t-fcntl` pins the current answer on both, which is what makes the divergence visible. (b) **A `flock` dies on *any* close of its description, not the last.** Linux releases the description's `flock` when the last descriptor referring to it closes; measured on this host, `flock(a, LOCK_EX)`, `close(dup_of_a)`, then a fresh `open()` of the file still answers `EWOULDBLOCK`, where this kernel has already released the lock - the close path passes the description's identity and nothing about how many descriptors still hold it. A process that dups its lock descriptor and closes the dup therefore silently loses its lock, and a second process can take it, which is what `apk`'s database lock exists to prevent. `src/vfs/locks.rs`'s own `release_on_close` record already states the Linux rule, so the code and its documentation disagreed; the record is corrected here to say which of the two the code is. Not fixed in this batch because **no claim fails on it**: `t-filelock`'s last claim answers `0` under both readings (Linux converts the description's shared lock to exclusive, this kernel has nothing left to convert), and rule 6 wants a pin with a fix - adding a thirteenth claim to a migration batch would be the change, not the migration. | (a) `t-fcntl` on both machines; (b) `t-filelock` on both machines, whose fourteen `ok:` lines are identical - the claim that would fail on a wrong *answer* is the one that tells the two readings apart, and there is none | **small each, and each needs its own pin** - both are named in `docs/UAPI.md` §5.2 with their measurements rather than left as code comments |
| 15.17 | **An absolute path does not make the `*at` family ignore its dirfd here; on Linux it does.** Linux's rule is that the dirfd is not used at all when the path is absolute, and it is the *dirfd* that stops being a question rather than the path: measured on a current host, `newfstatat(77, "/tmp", buf, 0)` with `77` not open answers `0` with errno `0` - while the same call with a *relative* name answers `EBADF`. This kernel resolves the dirfd first whatever the path looks like (one `base_of` does it, and twenty call sites go through it), so the absolute form answers `EBADF`. The no-std `t-newfstatat`'s bad-dirfd claim had an **absolute** path in it - `/etc/motd` - and so pinned this divergence as though it were the ABI, which is the class of claim batches 10, 11 and 12's §15.15 each turned up one of. The migrated claim names a relative path, which is `EBADF` on both machines and is what the claim is actually about, and this row is the divergence's name rather than its fix: the fix is one helper asked differently at twenty call sites, and every one of those needs its own answer before it is worth changing. Not a §3.2 defect - an honest refusal rather than a wrong number - but a caller holding a stale dirfd and an absolute path takes its fallback where Linux succeeded. | `t-newfstatat` on **both** machines (the relative-path claim, `ok:`-identical); `docs/UAPI.md` §5.2 carries the measurement | **twenty call sites, one helper** - found by running the migrated claim against real Linux before pinning it, which is the whole reason that step is in the recipe |
| 15.18 | **The capability model's gates are wired to the syscalls that own them.** `crates/cred` has stored the four sets, the dumpable flag and `KEEPCAPS` since it landed, and `capget`/`capset`/`prctl`'s credential options round-trip it - but `has_effective` had exactly one consumer (`FS_IOC_SETFLAGS`' immutable gate), which is why the migration batches kept meeting a shape they had to name as "no privilege check": `klogctl`'s gate on the host sits *before* the type (batch 29's unknown-type `EPERM`), `vhangup`'s before the no-terminal `ENOTTY` (batch 32), and `unshare`'s was an `euid == 0` stand-in. Five gates are now real, each asked of the caller's **effective set** through `sched::current_credentials()` and each in Linux's measured position: `syslog` *before* the type (so an unknown type from a capability-less caller answers `EPERM`, as the host's does), `vhangup` first, `chroot` before the path, `reboot` after the magic words and before the command (`CAP_SYS_BOOT`, the CAD policy included), and `unshare`'s privileged test is now `CAP_SYS_ADMIN` in the effective set rather than an identity word - the coarse check was already honest about *placement*, and only asked the wrong question. A root caller holds every capability in its effective set, so **no guest answer moved**: the whole sweep re-ran green unchanged, which is the design property - the model exists for the day there are two users, and until then it is invisible. The day is simulated by `t-caps` (new, both machines, 19 `ok:` lines identical): one identity dance - `setresuid(-1, 1, -1)` clears the effective set, `capget` reads the emptied effective word against the still-full permitted word (the saved id is root's), and the five gates answer `EPERM` while lowered, including `klogctl(99)` whose `EPERM`-before-`EINVAL` is the ordering claim in one line - then the raise-back through the saved id restores the effective set from permitted and every gate opens again, `CLONE_NEWUTS` splitting once more. `unshare(CLONE_NEWNS)` keeps `EOPNOTSUPP` the whole time: the refuse-rather-than-approximate answer no capability buys. The host run names its own limit: the host kernel answers `EINVAL` to the lowering itself (§15.11 batch 32's probe), so the dance cannot be staged there and the gated claims are mint-not-made on a host run | `cargo xtask test` `pass=160 fail=0`; `--arch aarch64` `pass=136 fail=0`, with `t-caps`' 19 `ok:` lines identical on both machines; host tests 517; `fmt --check` clean; clippy clean | **five gates, one model already stored** - the work was wiring, not inventing, and `t-caps` is the difference between a model that stores and one that gates |
| 15.19 | **the boot's self-tests become suites in a test build, and the boot path stops branching.** The kernel used to decide whether to test itself by parsing `mpos.selftest` off its own command line, while `src/board/checks/{mm,sched,vfs,ipc}.rs` was compiled into every image: five `if selftest { … }` branches in x86_64's `init` (seven in aarch64's), interleaved with the real bring-up because each family must run at the step it depends on. What the kernel has is a *build* flag, and **a `--cfg` cannot be added here** - measured: `RUSTFLAGS` *replaces* `.cargo/config.toml`'s `[target.*] rustflags` rather than extending them, so the linker script and all nine `mpos_*` capability flags go with it (4 errors in `mpos-arch`) - so the switch is the cargo feature `selftest`, the additive form of the same thing and the shape of the one it imitates (`CONFIG_KUNIT=y`). `src/board/kunit.rs` is the harness: `Suite`s of named `Case`s, each declaring the boot `Phase` it needs; a case answers `Result<(), &'static str>` rather than asserting, because the kernel is `panic = "abort"` and a case that ended the machine would hide every case after it, which is the one thing a test system must not do. The phases are load-bearing rather than decorative: `mm` asserts increments on a *fresh* heap and `vfs` needs the bootstrap root mounted, so running everything at one point before the handoff would mean re-measuring every assertion against a machine that has already been used - a test redesign, not a refactor (`kunit.rs`'s header names it as the alternative not taken). Production compiles neither the families nor the harness, and that is what made the unused-import warnings on `check_arch`'s imports *correct*: the compiler was saying those names left the shipping kernel with the check. `xtask` builds the test kernel and **copies it aside** (`utils::build_test_kernel`), because cargo holds one `mpos` per target and profile and a production build after a test build would otherwise leave the ESP staging whichever binary happened to be there; `mpos.selftest` is gone from the test command line, since a marker the kernel ignores is the shape that reads as a mechanism and is not one. The sweep asserts the harness's own line in both arms - present *and* zero failed - which is what makes "the suites ran" a claim rather than a hope | x86_64 `=== SUMMARY pass=160 fail=0` with `mpos-kunit 6 passed, 0 failed` and exit 33; aarch64 `pass=143 fail=0` with `mpos-kunit 7 passed, 0 failed`; the per-case lines in boot order - `mm.allocators`, `arch.clock_tables_address_spaces`, `sched.tasks`, `vfs.mounts`, `ipc.pipes_tty_signals`, `ipc.boot_stack_depth` on the first machine, plus `arch.console_receive` and `arch.console_transmit` on the second; both bare-metal builds with the same four pre-existing dead-code warnings and no new ones; host `mpos` 517, `mpos-xtask` 89; `cargo xtask depgraph` within the ratchet; clippy 0 errors on `x86_64-unknown-none`, `aarch64-unknown-none` and the host lib | **the mechanism, landed** - the families are one case each for now (each body still answers by panicking), and the `M*` checks' split into bring-up assertions and tests is slice 4 |
| 15.20 | **the suite vocabulary moves into the library, and `mm`'s family into `mm` - slice 2 of the boot self-test refactor, one family at a time.** Two things had to move before a suite could live beside the module it tests. **The vocabulary** (`src/kunit.rs`: `Phase`, `Suite`, `Case`, `case_check!`) went into the library, because both sides need it and the suite modules are the side that cannot reach up; the *harness* stayed in the wiring layer (`src/board/kunit.rs`), where the machine's own suites and the boot's phase markers are, and it simply names other modules' `const`s now - so the registry and the `install()` step this phase's plan had assumed turned out to be unnecessary (the plan's slice 2 was narrowed on landing, and this row is that record). **A case's answer** became `Result<(), String>` rather than `Result<(), &'static str>`: half the checks worth making are about *values* - "text at 0x… has the wrong permissions" is what a reader needs - and a static string would have to drop the address. The failure path allocates, which is the one place this costs anything and the one place it is worth it. `mm`'s family is the first to move, and it is the shape the other three will follow: one case per thing the module promises, each an answer instead of an `assert!`. It became **three** cases rather than one because the family's assertions were never one claim - `frames`, `large_block`, `heap` - and `heap` is deliberately *not* split further, because its agreement check compares the frames consumed *before* the allocations against the heap's mapped bytes *after* them, so split apart it would compare a baseline with itself and pass whatever the kernel did. The incidental panic is *not* converted (an `expect` on a frame the allocator just said it had): that is not a check, and a machine that hits one is broken in a way no later case would have reported | x86_64 `=== SUMMARY pass=160 fail=0`, `mpos-kunit 8 passed, 0 failed` (6 -> 8: `mm`'s three cases replace its one), exit 33; aarch64 `pass=143 fail=0`, `mpos-kunit 9 passed, 0 failed`; the log's lines in order - `mm.frames`, `mm.large_block`, `mm.heap`, `arch.clock_tables_address_spaces`, `sched.tasks`, `vfs.mounts`, `ipc.pipes_tty_signals`, `ipc.boot_stack_depth`; all four kernel builds (both machines, with and without the feature) with no new warnings; host `mpos` 517, `mpos-xtask` 89; `depgraph` within the ratchet; `fmt --check` clean | **`mm` landed; `sched`, `vfs` and the cross-subsystem half are the next three steps** - and `vfs`'s suite is where `sched::permanent_tasks`' fate is decided, since that accessor's two remaining callers are the `sched` and `vfs` families (the `sched` one can read the private state through its own child module; the `vfs` one cannot) |
| 15.21 | **`sched`'s family moves into `sched`, as seven named cases** - slice 2's second family, and the shape `mm` set: every deliberate check answers a value, so a failure names the scenario instead of stopping the machine at the first one in file order. The five scenarios the old `run()` called in sequence are five cases (`alternating`, `preempted`, `sleeping`, `idling`, `priority_wins`) and **two cases had to be re-derived rather than moved**, which is the part worth keeping: `reaping` was the old tail (`task_count() == permanent_tasks()`) and that claim is *absolute*, so it survives as a case unchanged - while `stacks_returned` compared the frames the five scenarios consumed against the heap's growth over the same span, so as a case of its own it would have compared a baseline with whatever the state happened to be by the end of the last one. It now measures a window it owns (spawn four tasks, wait, look) and asserts the same property there. The task-observed half of `sleeping` needed a door too: a task cannot return to the case that spawned it, so the tick count it measured comes back through a static (`SLEPT_TICKS`) and is asserted *in the case* - asserting it inside the task, as the old body did, is what turns a wrong number into a dead machine instead of a failure with the number in it. The suite uses `sched`'s **public** API rather than its private state, which is the honest reading here (everything it observes has a reader outside the suite), and that is why `permanent_tasks` is *not* deleted by this step: its last two callers are in the `vfs` family, and its fate - most likely `pub(crate)` - is settled there. `drain`/`wait_for_code` are `pub` from this suite because the two families that run after it borrow them | x86_64 `=== SUMMARY pass=160 fail=0`, `mpos-kunit 14 passed, 0 failed` (8 -> 14: `sched`'s seven replace its one), exit 33; aarch64 `pass=143 fail=0`, `mpos-kunit 15 passed, 0 failed`; the log's lines in order through `sched.stacks_returned` before `vfs.mounts`; all four kernel builds with no new warnings; host `mpos` 517; `depgraph` within the ratchet; `fmt --check` clean | **`mm` and `sched` landed; `vfs` is next and it is the one that decides `permanent_tasks`, then the syscall gate with the tty and the signal path** |
| 15.22 | **`vfs`'s family moves into `vfs`, the gate's cases into `syscall`, and `sched::permanent_tasks` leaves production** - slice 2's third family, and the step where the module ratchet earned its keep. `src/vfs/kunit.rs` answers values like its neighbours, with `ramfs_file` and `mounted_tree_multi_task` (a mount, an identity across it, the double-mount refusal, and two tasks reaching one file through it - one case rather than two, because the second has no fixture without the first, and a case that needs another case to have run first is not a case). **Wholesale movement failed, and the measurement is the interesting part**: putting `crate::syscall::linux_dispatch` inside a suite that lives in `vfs` made the depgraph baseline test report `vfs names crate::syscall:: 1 times, up from the recorded 0` and `the module graph has 9 mutual pairs, up from the recorded 8`. The answer is not to raise the constant - the gate's cases never belonged to `vfs`, they were merely written in its file - so they are `syscall.gate` and `syscall.dispatch_in_task` in `src/syscall/kunit.rs`, and the seven-argument `syscall` helper lives with them. **`sched::permanent_tasks` turned out to be a hook, and the compiler said so**: with the `vfs` reader task waiting on *the writer's own flag* (the tighter claim - it cannot be broken by another task existing) instead of on "the task count is back to the permanent number", every reader is inside `sched::kunit`, and the production builds reported `function permanent_tasks is never used` plus `unused import: runtime::permanent_tasks`. It, `PERMANENT_TASKS` and the re-export are behind `--features selftest` now: gone from a shipping kernel rather than merely `pub(crate)`. Two smaller consequences: a task's verdict became shared vocabulary (`crate::kunit::{record_verdict, take_verdict}`, one slot per suite so two suites' tasks cannot overwrite each other), and the post-`drain` "did the task end" assertions were dropped as tautologies - `drain` waits for exactly that, and `take_verdict` is what proves the task reached an answer | x86_64 `=== SUMMARY pass=160 fail=0`, `mpos-kunit 17 passed, 0 failed` (14 -> 17), exit 33, command exit 0; aarch64 `pass=143 fail=0`, `mpos-kunit 18 passed, 0 failed`, exit code 0; the log's lines in order - …, `sched.stacks_returned`, `vfs.ramfs_file`, `vfs.mounted_tree_multi_task`, `syscall.gate`, `syscall.dispatch_in_task`, `ipc.pipes_tty_signals`, `ipc.boot_stack_depth`; all four kernel builds with no new warnings, **the two `permanent_tasks` warnings gone**; host `mpos` 517, `mpos-xtask` 89 including the ratchet test; `depgraph` within the ratchet, back to **8** mutual pairs; `fmt --check` clean | **`mm`, `sched`, `vfs` and the gate landed; the tty-and-signals family is the last one, and it takes `src/board/checks/` with it** |

| 15.23 | **the last family leaves the wiring layer's checks directory, and the boot's own check moves beside the check it drives** - slice 2's last step, and the one that deletes `src/board/checks/`. `check_ipc` was one panicking chain covering three different things, and it is `ipc`'s suite in `src/syscall/kunit.rs` now: `pipes` (bytes across two tasks, EOF once the writer is gone, `EPIPE` with no reader), `tty` (line editing, echo, `Ctrl-D`, and a blocking `read(2)` on fd 0 through the gate) and `signals` (`SIGTERM`; `SIGKILL` into a task blocked *inside* the dispatch; a signal a task sends itself; the `kill(0)`/`ESRCH`/`EINVAL` probe). It is **a second suite beside the gate's rather than three more cases of it**, and the reason is the one the vocabulary is built on: a suite declares the boot step it needs, and these need the tty table and delivery (`Phase::Ipc`) where the gate's cases need the descriptor table and the scheduler (`Phase::Vfs`) - a boot step is not a position in a file. Nor could the family live in `vfs` or `sched`, though that is what it checks: every claim it makes goes *through the gate* (`pipe(2)`, a blocking `read(2)`, `kill(2)`), and §15.22's measurement is what says a suite inside `vfs` may not name `crate::syscall` (`vfs`'s recorded count is 0, and the graph's 8 mutual pairs do not include `vfs <-> syscall`); the cases were written in the checks directory, which had no subsystem of its own. The boot stack's canary was the one case that could not move into a subsystem at all - a boot stack belongs to the boot - so it is `report`'s own `SUITE` (`boot.stack_depth`), in the module that owns `check_boot_stack`, and the harness in `src/board/kunit.rs` is finally the thing its header claims to be: the list and the driver, with no suite of its own. Three smaller things the move settled. The pipe writer's count comes back through a verdict slot - the fourth and last, one per suite that spawns a task, so `vfs`'s two, the gate's one and this one cannot collide. A fed tty read that cannot fail anyway answers through `case_check!` rather than an `expect`, because a case that panicked would hide the cases after it. And the `unreachable!` inside the reader that `SIGKILL` ends stays a scaffold on purpose: a task has no case to return to, which is the same reason `sched`'s run record keeps its `assert!`. The `syscall` helper the family borrows gained the `# Safety` section its dispatcher contract needs, which closes the one clippy warning §15.22 left in that file | x86_64 `=== SUMMARY pass=160 fail=0`, `mpos-kunit 19 passed, 0 failed` (17 -> 19: one panicking family becomes three cases, and the boot stack's case becomes a suite of its own), exit 33, command exit 0; aarch64 `pass=143 fail=0`, `mpos-kunit 20 passed, 0 failed` (18 -> 20), exit code 0; the log's lines in order - ..., `syscall.dispatch_in_task`, `ipc.pipes`, `ipc.tty`, `M8_IPC_OK`, `ipc.signals`, `boot stack: intact after the self-tests`, `boot.stack_depth`, `mpos-kunit: 19 passed, 0 failed`, then the handoff's own `boot stack: intact under spawn_init`; all four kernel builds with the same four pre-existing x86_64 dead-code warnings and no new ones; host `mpos` 517; `depgraph` within the ratchet - 8 mutual pairs, `vfs -> syscall` still 0/0; `fmt --check` clean; clippy 0 errors on both bare-metal targets, with and without the feature | **slice 2 is done; the `M*` checks' split into bring-up assertions and tests is slice 3** - the classification table is an approval point before any code moves, and the structural gate that keeps a test opening from growing back is slice 5 |

| 15.24 | **the marker vocabulary leaves the boot log, the last two panicking families answer, and the rule that decides which checks stay is written down** - the refactor's last step, and the one the plan kept as a review point. **The rule.** A *bring-up assertion* is what the kernel cannot run without - a device the machine was configured with, a filesystem it must read, a devfs it must publish - so it stays in production, fails loudly, and costs nothing measurable. A *test* is worth proving but not needed to run, or too expensive to prove on every boot, so it is a case. Fifteen of aarch64's device checks are the first kind and did not move: each is an open plus a read (the one that is not, `check_serial_tx`, was already test-build-only and waits in bounded ticks). `check_arch` was both at once, and splitting it is what the split says: `arch.clock_breaks_registers` (the clock's ±10% window, `int3`, the register-table/ABI agreement) and `arch.page_tables_and_spaces` (the direct map after the switch, W^X across the whole image, the null page, the kernel stack's region, two user address spaces mapped, isolated and torn down with their tables accounted for) - and it costs **half a second of busy-waiting per call**, which is the clearest reason a test is a test. Its address-space half had to be rewritten rather than converted: CR3 is on a process's space inside the `critical_section` block, so that block's answer comes *out* of the closure and the kernel's root is restored unconditionally and checked before the verdict is returned - an early return would have carried the rest of the boot on the wrong page tables, the class §13.14/§13.15 record. `check_serial_tx`/`check_console_rx` answer too, which let their two adapters go, and the transmit case still reports its *detail* through the machine's polled writer: a queued-bytes failure is by definition a ring that is not draining, so a report filed through the console queue is one nobody reads. **The vocabulary.** Nothing had parsed `M*` markers since §15.6/§15.7 retired the command that did, and a token a reader cannot tell from a mechanism is worse than a sentence: the board's seventeen checks and the machine layer's eleven (`M1_TIMER_OK`, `M3_TABLES_OK`, `M6_*` in `mpos-arch`) now name what they are (`virtio-blk: …`, `9p: …`, `tables: …`, `space: …`), which is what the first machine's driver lines had always done. Eighteen `_OK` lines were deleted as pure repetition of the sentence above them; three that carried data of their own were reworded (the timer's interrupt count, the context switch's hand-over count, the devfs read-back). The two production *events* became sentences - `mpos: init is pid 1, not yet dispatched`, and `mpos: /init is up; a child is running under it`, which is what `has_live_child` waited for all along - and `M4_CONSOLE_OK`/`M8_NET_STACK_OK` were deleted because the line before each is the same fact. The suites' own `M0_OK`/`M2_OK`/`M4_OK`/`M6_OK`/`M7_OK`/`M8_IPC_OK` went the same way: a case's verdict is the harness's `mpos-kunit: <suite>.<case> OK`. **The rule is a gate now.** `xtask/src/gates.rs`'s `test_openings` fails `cargo xtask test` before it compiles anything when the boot's own code reads a test word off the command line, asks `feature = "selftest"` outside a `#[cfg]`, or prints an `M*` marker literal - three words no compiler can see - over a list of seven files and only those, because three comments inside that list say `mpos.selftest` on purpose to record what the kernel stopped reading | x86_64 `=== SUMMARY pass=160 fail=0` with `mpos-kunit 20 passed, 0 failed` (19 -> 20: the `arch` family splits) and exit 33, command exit 0; aarch64 `pass=143 fail=0` with `mpos-kunit 20 passed, 0 failed`, exit code 0; the log's lines in order - ..., `arch.clock_breaks_registers`, `arch.page_tables_and_spaces`, `sched.alternating`, ..., `boot.stack_depth`, `mpos-kunit: 20 passed, 0 failed`, `mpos: handing the machine to /init`, `mpos: /init is up; a child is running under it`; the aarch64 log's plain lines as they now read - `devices: 32 virtio-mmio windows, ...`, `timer: 25 interrupts in ...`, `space: 4 pages mapped ...`, `tls: set_thread_pointer(...)`, and the board's `devfs:`/`virtio-gpu:`/`9p:` findings; both gate lines print before the host tests (`Target gates: every test module is one the host compiles`, `Target gates: the boot has no test opening`); all four kernel builds with no new warnings; host `mpos` 517, `mpos-xtask` 93 (89 -> 93: the gate's four tests); `depgraph` within the ratchet, 8 mutual pairs; `fmt --check` clean; clippy 0 errors on both bare-metal targets with and without the feature; and grepping the tree's Rust for the vocabulary finds only prose | **the refactor is done.** The one row a reader can still argue about is the pair the classification flagged as debatable: W^X across the whole image and the null page are cheap, and violating either is a security hole, so a production kernel *could* assert them the way Linux's `CONFIG_DEBUG_WX` does - they are in a test build today, and moving them back is a smaller step than the one they came with |
| 15.25 | **batch 37 of the ABI migration moves `t-fbdev`, and the hand-spelled record meets musl's `c_ulong`.** The framebuffer door was the smallest device-shaped program left that both machines serve: since the gap-closing pass opened the fbdev ioctl gates, aarch64 publishes `/dev/fb0` unconditionally behind its MMIO card (the boot log prints the geometry on both), so the move needed no new group decision in `tests.toml`. What the move measured is a *layout* trap the suite had not met: `libc` binds neither the `FBIO*` words nor either `struct fb_*_screeninfo` for linux at all (measured: absent from `libc` 0.2.190's whole tree; `rustix` stops at termios), so the records are spelled here as `t-rtc` spells its `_IOR('p', …)` family - but `fb_fix_screeninfo`'s two `unsigned long`s land differently than a Rust author expects, because `libc::c_ulong` follows Rust's `c_long` rule (pointer-width unless windows) and **musl targets alias it to `u32` even at 64-bit** (measured: `size_of::<c_ulong>() == 4` on both musl arms, 8 on gnu). A field spelled with it would put `smem_start` at 16 and fail the program's own offset assert on the very payloads it exists to check - the first batch where the maintained table itself is the wrong source for a *type*, not a number. The head therefore pins every field below the two longs by compile-time `offset_of!` asserts against `linux/fb.h`'s numbers (xres@0, bpp@24, red@32, transp@68, smem_len@24, type@28, visual@36, line_length@48, mmio_len@56) - `t-getdents64`'s build-claim rule, which also caught this file's own first draft: an early version asserted the var head's *size* as 44, which is the C field sum, while the trailing bitfield triples push it to 80; the seed-survives claim reads through byte 79/51, the bytes each kernel arm actually writes. Two claims are new pins of Linux's answers rather than migrations: `yres_virtual == yres` (a single scanout has nothing to pan across) and `smem_len == line_length * yres` (the stride-times-rows relation fbdev clients size their copy from), both stated from the headers and holding on both machines. The host cannot measure any of it: this sandbox's `/dev/fb0` is `crw-rw---- root:video` and refuses an unprivileged open (measured EACCES), so a hosted run forgives only what the host's privileges refuse - `t-dsp`'s rule - while the gate keeps pinning all twenty claims. | x86_64 `user tests: 170 programs built` / `160 staged, 10 left out` / `=== SUMMARY pass=160 fail=0` with `mpos-kunit 20 passed, 0 failed` and exit 33, command exit 0; aarch64 `146 aarch64 programs built` / `144 staged, 3 left out` / `pass=144 fail=0` (was 145 staged with one stray probe binary that had no source; the clean run is 144 and its payload now carries `t-fbdev` where the abitest side carried it before) reaching `=== END`, exit code 0 - **the first run of this program on the second machine**, with `=== RUN t-fbdev` / `=== PASS t-fbdev` in both logs and the **same 20 claims**, their per-claim `ok:` lines identical on the two machines; host tests 517; `mpos-xtask` 93; `depgraph` within the ratchet, 8 mutual pairs; `fmt --check` clean; clippy clean on both musl targets; `user/abitest` is down to **23** programs (0 syscall plus 23 integration) |
| 15.26 | **batch 38 of the ABI migration moves `t-fsflags`, and the flag's two halves get a case each.** The attribute word is the door `lsattr`/`chattr` are made of, and both machines serve it since the gap-closing pass opened the ioctl families: no group decision was needed. What the move found is that the no-std program had only ever pinned *half* the rule. Its claims all reach the checks through `ioctl(2)` on a descriptor, which is the syscall door; but the same four questions (`may_modify`, `may_write`, `may_alter`, `may_remove`) are asked by every path that changes a file, and two of them have no payload spelling at all: an **append-only directory** refusing the removal of what lives in it (Linux's `may_delete` asked of the parent - `chattr +a` on a directory is not a thing any test does), and a **symlink** - a node with no attribute word, which answers the flag question as ENOTTY-shaped refusal yet is still policed by its parent's flag like any other name. Both now live as a third case of the `vfs` suite (`inode_flags_are_policed`), reading the filesystem through the module's own API rather than through the gate: §15.23's rule that a suite beside the module may read its private state, applied to a claim the sweep structurally cannot make. The migrated program keeps every claim the no-std file made - the whole-word refusal set, the IMMUTABLE seven-door list, APPEND's end-of-file rule, the pipe's ENOTTY, and the capability dance - and moves three numbers onto maintained tables: `SYS_setresuid`/`SYS_fchown` per machine (93/55 for the chown, the number the no-std literal could not carry), and the drop itself raw because `libc::setresuid` takes `uid_t` and cannot name `-1` (`t-ns-uts`'s door again). One fixture step is new for a measured reason: the payload has no non-root user and `/tmp` is root's, so the owned file is created while root and handed over with `fchown(1000, 1000)` before the effective-id drop - the no-std version dropped first and then created, which works only because this kernel's DAC check reads the *effective* id where Linux's `may_create_in` would ask the directory's write bit of a uid that owns nothing there. The host measured its side of the shape: tmpfs stores none of NODUMP/NOATIME/SYNC/DIRSYNC (errno 95) and refuses `chattr +i` outright for a caller without `CAP_LINUX_IMMUTABLE` (errno 1), so the hosted run takes named refusal branches for those sections - `t-posix`'s rule - while the gate pins all forty-one claims on both machines. | x86_64 `user tests: 170 programs built` / `160 staged, 10 left out` / `=== SUMMARY pass=160 fail=0` (the count does not move - the program was already in the sweep from the abitest side) with `mpos-kunit 21 passed, 0 failed` (20 -> 21: the flags case), exit 33, command exit 0; aarch64 `148 aarch64 programs built` / `145 staged, 3 left out` (was 144: the std side now carries `t-fsflags`, which the abitest staging used to hand it) / `pass=145 fail=0` reaching `=== END`, exit code 0, `mpos-kunit 21 passed, 0 failed`; `=== RUN t-fsflags` / `=== PASS t-fsflags` in both logs with the **same 41 claims**, their per-claim `ok:` lines byte-identical on the two machines; host tests 517; `mpos-xtask` 93; `depgraph` within the ratchet, 8 mutual pairs; `fmt --check` clean; clippy 0 errors on both bare-metal targets with and without the feature, warning count unchanged at 49; `user/abitest` is down to **22** programs (0 syscall plus 22 integration) |
| 15.27 | **batch 39 of the ABI migration moves `t-netlink`, and the route socket's answers get read on the second machine for the first time.** The whole loop the kernel owes busybox `ip` - a dump from the live stack, a mutation, the `NLMSG_ERROR` report (zero *is* success), the dump again changed - was served on both machines since §13.4 gave the second one a stack: every address claim states `src/net/mod.rs`'s own configuration (eth0 static at 10.0.2.15/24, gateway 10.0.2.2, the same either bus brings up - both boot logs print the line), so no group decision was needed. The vocabulary is hand-spelled as `t-rtc` spells its ioctl family, and the measurement says why: `libc` 0.2.190 binds neither `struct nlmsghdr` nor any `NLMSG_*`/`NLM_F_*` word for linux (those exist in its android and uapi-mirror trees only), while `rustix` speaks netlink *addresses* (`SocketAddrNetlink`, `bind`) but owns no part of the record shape - which is the subject, so the parser stays the test's. One door moved onto rustix that the no-std version did not use: the migrated program **binds explicitly**, because glibc's auto-bind-on-first-send is a wrapper convenience the raw door does not have and Linux does not promise; the bind is the shape busybox `ip` builds itself. Two claims are new pins of Linux's answers rather than migrations: an administratively up interface also reports `IFF_RUNNING` (the bit `ip link` prints inside `<UP,RUNNING>`, and a caller trusting `IFF_UP` alone misreads on a carrier-less card), and the link dump names **both** interfaces Linux's enumeration has - `lo` at index 1 with `IFF_LOOPBACK|IFF_UP`, eth0 at 2. Not run on the host, and batch 34's reason is exact: a host Linux answers every request here including all three refusals (EEXIST/EADDRNOTAVAIL/ESRCH are Linux's own numbers), but it answers them about *its* stack - 10.0.2.15 is this kernel's static address, and setting the host's real NIC down would take the sandbox's network with it - so the hosted run says so and claims nothing, finishing green with its reason printed. | x86_64 `user tests: 170 programs built` / `160 staged, 10 left out` / `=== SUMMARY pass=160 fail=0` (the count does not move - the program was already in the sweep from the abitest side) with `mpos-kunit 21 passed, 0 failed`, exit 33, command exit 0; aarch64 `149 aarch64 programs built` / `146 staged, 3 left out` (was 145: the std side now carries `t-netlink`, which the abitest staging used to hand it) / `pass=146 fail=0` reaching `=== END`, exit code 0, kunit 21/0; `=== RUN t-netlink` / `=== PASS t-netlink` in both logs with the **same 25 claims**, their per-claim `ok:` lines byte-identical on the two machines; host tests 517; `mpos-xtask` 93; `depgraph` within the ratchet, 8 mutual pairs; `fmt --check` clean; clippy clean on both musl targets; `user/abitest` is down to **21** programs (0 syscall plus 21 integration) |
| 15.28 | **batch 40 of the ABI migration moves `t-ioctl`, and three host walls get named instead of walked.** The console, entropy-pool and clock families are the boot's own services' doors - seedrng's credit, hwclock's read, the line editor's window query - and both machines serve every one: the `'K'` gates went down in the gap-closing pass, rtc0 answers on either machine (the first machine's CMOS, the second's PL031 through one `crate::hal`), and the pool is `src/net`'s generator with its designed-full 4096. The migrated program keeps all sixty-five claims and moves the numbers onto maintained tables where tables exist: `libc`'s arch-generic `TCGETS`/`TCSETS`/`TIOCGWINSZ`/`FIONREAD`, `libc::termios`'s own fields, `libc::winsize`, `libc::SYS_ioctl` per machine (16 / 29); the `'R'`/`'K'`/`'t'` words stay spelled as `t-rtc` spells its family (measured: libc binds none of them for linux). The terminal trio rides the raw door for clause (c) - rustix's `tcgetattr`/`tcgetwinsize` own their buffers and reach `TCGETS2` first (§15.23's measurement), while this file's claims *are* the wire: the seeded pixel fields, the `c_lflag` word as stored, `c_cc[VEOF]` at the uapi index. What the host measured is three walls, each a different kind: stdin is not a tty here (every terminal request ENOTTY, so the console section is probed and named-and-not-made - `t-sa-restart`'s pty rule turned inward); the real pool reports 256 bits where this kernel designs 4096 (the count claim takes the host branch, the shape stays pinned); and CAP_SYS_ADMIN is absent, which the host enforces *before* any argument check - every mutating-RND shape answers EPERM, including the ones whose order this file pins (EINVAL-before-capability, this kernel's documented divergence from Linux). So the whole privileged block sits behind a root probe with the skip printed, and the stray clock-on-pool claim names the host's errno-22 answer beside this kernel's magic-first ENOTTY rather than failing it. One fixture moved for batch 3's reason: the non-terminal descriptor is the test's own `/tmp` file, not `/etc/hostname`. | x86_64 `user tests: 170 programs built` / `160 staged, 10 left out` / `=== SUMMARY pass=160 fail=0` (the count does not move - the program was already in the sweep from the abitest side) with `mpos-kunit 21 passed, 0 failed`, exit 33, command exit 0; aarch64 `150 aarch64 programs built` / `147 staged, 3 left out` (was 146: the std side now carries `t-ioctl`) / `pass=147 fail=0` reaching `=== END`, exit code 0, kunit 21/0; `=== RUN t-ioctl` / `=== PASS t-ioctl` in both logs with the **same 65 claims**, their per-claim `ok:` lines byte-identical on the two machines; host run green with four notes naming the walls; host tests 517; `mpos-xtask` 93; `depgraph` within the ratchet, 8 mutual pairs; `fmt --check` clean; clippy clean on both musl targets; `user/abitest` is down to **20** programs (0 syscall plus 20 integration) |
| 15.29 | **batch 41 of the ABI migration moves the `slow` trio - `t-forkcost`, `t-pipecost`, `t-netlat` - and the aarch64 sweep's selection finally matches the rule written for it.** The three are instruments, not assertions: their numbers are the point (§10.2's wake bound is asserted *inside* `t-forkcost`, the pipe rows only report), which is why they sit in `slow`. What the move measured is bigger than the programs: §13.5 promised "the aarch64 sweep runs every program it built" except its own gap group, but `sweep::select` asked that promise through `runs()`, whose reason-groups (`external`/`slow`/`diagnostic`) name the *first* machine's harness needs - so every migrated program in a reason-group was built for aarch64, staged out, and run by neither arm's default sweep. Measured end to end: the trio appeared in `built` (153) with digests, never in the stamp or the payload; the guest ran exactly what the plan staged, twice over. The fix is one branch in `select`: on the second machine a *reason* never leaves a program out, only its own machine-named gap list does; x86_64's selection is byte-for-byte unchanged (pinned both ways by two new `mpos-xtask` tests). Two consequences landed with it. The aarch64 root partition overflowed the moment the payload grew to the full std suite (`mkfs.ext4` refusing block allocation at 64 MiB with a ~57 MB `/bin`), so `SWEEP_DISK_BYTES` went to 96 MiB - the number is now a measurement with its failure mode written beside it. And the x86_64 test initramfs stopped being a repacked copy of the Alpine tree: staging unpacked and re-tarred 70 MB, doubling what the guest's ramfs charges (limit a quarter of RAM, §10.4) while `t-fill-ramfs` needs the headroom; the programs now ride as ustar members appended to the base archive's tail - the base's zero-block terminator stripped, its `./init` renamed `./init.boot` (the reader refuses a duplicate regular file outright), the runner and each `/bin/t-*` written with the header/checksum rules `src/vfs/tar.rs` parses. Pinned against GNU `tar` by a new xtask test that lists the result and demands each member exactly once - the draft that shipped wrote all-zero headers, then a checksum that counted the old chksum bytes, and both readers saw nothing. The programs themselves keep every claim: `t-forkcost`'s 200+20-round loop, witness-child share proof and sub-tick reap bound; `t-pipecost`'s stamped 64 KiB chunks across the fork plus the 4096-B control phase; `t-netlat`'s twenty hand-built echo rounds matched by id and sequence, with the NETLAT summary line. On the host the trio measures Linux's own scheduler, pipes and loopback device - the first two pass honestly there, `t-netlat` names slirp's fabric as this kernel's own and claims nothing. | x86_64 `user tests: 170 programs built` / `160 staged, 10 left out` / `=== SUMMARY pass=160 fail=0` with `mpos-kunit 21 passed, 0 failed`, exit 33, command exit 0; `--groups slow` `pass=164 fail=0` with the trio green and the summary line `NETLAT first=160us min=70us avg=91us steady_avg=88us max=160us`, `fork 412.7 us / wait4 187.6 us` (32 MiB parent) against `18.6 us / 11.9 us` (no arena), `two processes 9705 MiB/s` / `one process 4372 MiB/s`; aarch64 `153 aarch64 programs built` / `**152 staged, 1 left out**` (was 147/6 - the five reason-group programs are this machine's checks now, only `t-arch-prctl` stays its gap) / `pass=152 fail=0` reaching `=== END`, kunit 21/0 - **the first run of the trio on the second machine**, `=== RUN/PASS t-forkcost`, `t-pipecost`, `t-netlat` in both logs with per-claim `ok:` lines identical except the mapped address each machine's ASLR chose; host runs green (`t-netlat` with its named fabric note); host tests 517; `mpos-xtask` 95 (93 -> 95: the selection rule's two pins); `depgraph` within the ratchet, 8 mutual pairs; `fmt --check` clean; clippy 0 errors on both bare-metal targets; `user/abitest` is down to **17** programs (0 syscall plus 17 integration) |

### 15.6 The initramfs flow — **LANDED**, all but the Alpine login pin

The stage's whole point was that the second machine stops being the exception: the kernel unpacks
`initramfs-aarch64/`, hands the machine to the script, and the *script* mounts `/dev/vda`, promotes
it into `/` and `switch_root`s into the disk's own `/init` - the flow `AGENTS.md` §4 rule 1 and
`docs/ARCHITECTURE.md` §4 require, and the one the first machine has always run. `check_disk_root`'s
mount half became `steps::mount_disk_root` (called from `init` exactly where x86_64's `init_blk`
calls it, with the wrapper deleted outright), `M7_ROOT_OK` stopped being a kernel marker, and the
`init` seam is one signature (`fn(&BootInfo) -> !`) on both machines.

**What the path cost: six defects, each measured, and only the first was the one §13.20 predicted.**

| # | Defect | The measurement |
|---|---|---|
| 1 | `enter_probe` published its save-area address as `sp - 8` in Rust while the prologue stamped `[ret, sp]` at `sp - 16`, so `eret` aimed at the saved *stack* - non-executable - and the first EL0 program to ask to leave died on an instruction permission fault | `EC 0x21`, `IFSC 0b001111`, `ELR_EL1 == FAR_EL1` inside the stack the program was running on |
| 2 | the return stub landed on the caller's `x30` directly, skipping `enter_probe`'s own epilogue - so the caller came back with `sp` 80 bytes *below* where its frame expected it, and every `[sp, #imm]` store in `check_el0` afterwards wrote into its own locals | probe-in/probe-out `sp`, `fp` and `addr_of!(space)` all identical while `word_via_at` read a different value every run: the *memory* had moved, not the pointer |
| 3 | `mpos_enter_frame` reloads all of `x0..x30` from the frame, and the leave path runs no epilogue, so callee-saved registers did not survive an ordinary call | the first instruction after the landing read `[x20, #0x8a8]` with `x20` holding a length, and faulted at `0x906`; the save area now carries `x19..x29` + `q8..q15` |
| 4 | the *test* disk's `/init` was the overlay's `rootfs-aarch64/init` - a `#!/bin/sh` script whose interpreter the disk does not have, because the test disk is the build's own payload and carries no busybox | `switch_root: can't execute '/init': No such file or directory` with every `/t` marker absent; `aarch64_init::exec_launcher_init_elf` is the answer, and it is the shape `run_sweep`'s runner already had |
| 5 | the script read the console from `/dev/console` *after* `mount --move /dev /sysroot/dev` - the wrong device (this kernel's `/dev/console` answers `read` with end of input; the queued line is on **fd 0**) and the wrong moment (`/dev` was gone) | `cat: can't open '/dev/console': No such file or directory`, then `R:` with nothing after it. `read line` from stdin, gated on `mpos.selftest` - `cat` would block forever on the second read |
| 6 | the §15.6 inittab probe never ran, so four of its hand-encoded instructions had never been executed once: `udiv` without bits 15..10 (`0b000010`) is not a divide, `msub` without bit 15 is `madd`, `strb` built on `0x3E000000` is a SIMD store at a register offset, and `cmp` built on `0xB100001F` is `cmn`. Syscall 56 is `openat` on aarch64, not `open` (`nr_aarch64.rs` records `OPEN` as an unreachable 64000), so the path was in `x0` where `dirfd` belongs | an unknown-reason exception mid-print; then `MPOS_INITTAB_OK` absent with the open having answered a negative errno. `aarch64_init`'s tests now pin every one of these words against `llvm-mc -show-encoding`, which is the check the module's header had claimed since it was written |

**The pins, measured 2026-10-07.** `boot-aarch64`: the full chain through `MPOS_SWITCH_ROOT_OK`,
`R:echo-test`, `MPOS_FAULT_OK`/`MPOS_EXEC_OK`/`MPOS_FORK_OK`, `MPOS_INITTAB_OK bytes=1957`,
`MPOS_MUSL_OK` and the whole `MPOS_DEV_*`/`MPOS_FILE_OPS_OK` set, ending `INIT_EXITED`.
`test --arch aarch64`: `pass=27 fail=0`. `cargo xtask test`: `pass=158 fail=0 (158 built)`, exit 33.
`depgraph`: within the ratchet, no edge appeared and no count grew.

**The open pin, now closed.** `alpine-aarch64` boots a real openrc runlevel - clock, local
filesystems, hostname, syslog, crond, `local` all `[ ok ]` - and reaches the prompt:
`Welcome to Alpine Linux 3.25.0_alpha20260805 (edge) / Kernel 0.1.0 on aarch64 (/dev/tty1)` then
`mpos login:`. It cost one line's placement. `steps::arm_console` was inside aarch64's
`if mpos.selftest` arm while x86_64 calls it in `handoff_to_init` on every boot, so a boot without
the test word had **no console mux at all**: the getty busybox init spawns is on `/dev/tty1`, a pty
whose master only the mux reads, so its output had nowhere to go and the log simply stopped after
`Starting local ... [ ok ]`. The arming moved out of the gate; `check_serial_tx` - the check *of* the
armed path - stayed in it. Measured: the same boot now prints the prompt in **16 s**, against the
240 s deadline it used to exhaust.

**And the prompt is not the whole verdict.** A failed openrc service prints `[ !! ]` and the runlevel
carries on, so `login:` appears whether or not every service came up - which is how §13.30's
`mount: mounting proc on /proc failed: Resource busy` sat in a log this command called green. The
command now fails on openrc's own failure marker, and the boot that reaches `mpos login:` carries no
`[ !! ]` line at all.

**The last named difference is closed.** `init_net` + `virtio::spawn_service` sat behind
`mpos.selftest` in `board/aarch64.rs`, read as §13.14's rule ("a service that did not assemble an
interface must not be handed one"). That reading was wrong, and the measurement says so: those two
calls are *bring-up*, not checks, and the first machine has always run both unconditionally
(`init_net` straight out of its PCI enumeration, `spawn_service` at the end of `init_virtio`).
This machine's adapter is assembled before either call regardless of the gate - `check_net` is part
of `devices()`, which `arch::boot` runs on every boot in front of the unpack, and the window it
opens is what `net_device()` picks up - so a boot that reached the gate had already proven a
virtio-net window exists (`check_net` halts the machine otherwise). Gating the bring-up changed
nothing about what the tree carried and only decided whether the stack above it was polled:
measured, the Alpine boot ran with a published adapter and a dead `net` task, and `run --arch
aarch64` - a session boot, so it carries `ALPINE_CMDLINE` and not the test word - never started the
input pump, which is the same task. With the gate removed the Alpine boot prints
`M8_NET_STACK_OK` on the way to `mpos login:` (13 s), `boot-aarch64` keeps its full chain, and the
sweep is `pass=27 fail=0`.

After this, no line in the boot flow is written twice for a reason that is not a bus, a UART or a
page-table format. What each body holds is the shape §"The target shape" item 4 names: the entry's
callbacks, `devices()`, the machine's own device discovery and checks, and the ordering its own
milestones need.


### What stays the machine's, by design

PCI vs device tree; MSI vs GIC line routing; the lazy-FPU trap (no `#NM` on arm64); the two UARTs;
the page-table/IDT proofs (`check_arch`); the console arming order (`steps::arm_console` is *called*
at different points on purpose). None of these is duplication to remove; the phase's rule is the one
`board/mod.rs` already states - a body holds only what would have to be answered differently on a
different machine.

### Known traps for this phase

- **The aarch64 sweep's payload is not the x86_64 harness's machine.** Measured while writing
  §13.20's pin: the x86_64 test `/init` mounts `/proc`, `/sys`, `/dev`, `/tmp`, `devpts` with
  busybox; the aarch64 runner mounts only `/tmp`. A test that needs `/proc` therefore behaves
  differently per machine, and one that does (this phase's shared checks will) must either mount what
  it needs or be honest about the difference. Closing *that* gap is itself part of stage 15.1's
  argument.
- **The marker chain is no longer asserted by a command of its own** (§15.7). The checks still
  print their `M*_OK`/`M*_FAILED` markers on every boot and a failure still halts the machine, so a
  red boot is readable off the wire - but nothing parses the chain any more: what a boot's verdict
  is, on either machine, is the sweep's `=== RUN/PASS/FAIL/SUMMARY`/`END`.
- **A check that mutates shared state damages the machine it checks** (§13.14, §13.15).
- **A device check runs on every aarch64 boot, a session included**, and halts the machine when it
  fails: `check_blk` needs the 1 MiB pattern image beside the disk, `check_fs` needs the root
  partition to carry `/hello`. Both are therefore inputs of *any* aarch64 image, not only a test's
  (`disk::blk_image`, `Arch::kernel_reads_root_probe`).

---

### 15.7 The front end: one path for two machines, and an image that is reused

**Landed 2026-10-08.** `xtask/src/aarch64.rs` is deleted (812 lines), `boot-aarch64` and
`alpine-aarch64` are retired, and every command now handles the machine `--arch` names: `build`,
`initramfs`, `disk`, `rootfs`, `all`, `run`, `test`. The second machine's three entry points had
become copies of the first machine's steps - its own command line, terminator list, marker list,
ESP layout, disk lay-out, staleness rule and `getty` decision - and what was left in that file was
the scaffolding around those steps rather than a second machine's own work. The whole of the
deletion, and what replaced each piece:

| Deleted | Replaced by |
|---|---|
| the two commands | the same commands with `--arch aarch64`, which now really build, install and boot that machine |
| the aarch64 verdict boot (its `CMDLINE`, `DEADLINE`, `TERMINATORS`, `M*` marker list, `R:echo-test` pin) | nothing: the marker chain is what a boot's wire *says*, and the sweep is the verdict on both machines |
| `alpine-aarch64` (staging tree + `fakeroot` + `mkfs.ext4 -d`) | `rootfs --arch aarch64`: one install through the block layer, `apk add --arch aarch64 --no-scripts`, as root, into the mounted partition - which is also what makes ownership right without `fakeroot` |
| `build_alpine_esp`, `run_interactive`, `ModeChoice`, `alpine_cmdline` | `disk`/`rootfs`/`run` parameterised by `Arch`, one mode word for both machines (`mode.cmdline_consoles`) |
| `install_alpine_root`, `ensure_alpine_root`, `alpine_staging`, `alpine_stamp`, `alpine_root_is_stale` | `rootfs::install_base` + the rootfs/`disk` stamps, per machine (`Arch::rootfs_stamp`) |
| `openrc_failure`/`without_ansi` | nothing: they read the retired Alpine verdict's log |
| `Arch::console`, `qemu::vsock_available` | the config's `rootfs.serial_port` (one source for `console=` and the inittab) and `virt_machine_args`' own `/dev/vhost-vsock` check |
| `@SERIAL_GETTY@` and `render_inittab` | nothing, and the measurement is why: see below |
| `disk::boot_media`'s installed-root branch | `disk::sweep_medium`, which is now only a *sweep*'s medium - it lays a complete GPT out of files without root, and it writes its own `build/aarch64-sweep.img` so it can no longer overwrite the image `run` installed |

**The bug the request started from, and its fix.** `dir_newer_than` followed symlinks
(`fs::metadata`), and an installed Alpine tree carries *absolute* ones - `/etc/init.d/functions.sh
-> /usr/libexec/rc/sh/functions.sh`, which dangles on the host that installed it. Measured on the
second `cargo xtask alpine-aarch64` in a row: `Failed to get metadata for
build/aarch64-alpine-root/etc/init.d/functions.sh`, the command exiting non-zero *before* it
booted. Every run before that had hidden it by rebuilding: the fingerprint of the boot's command
line differed between the verdict and the sweep, so the staleness check short-circuited on the
fingerprint and never reached the tree. The walk reads `symlink_metadata` now
(`utils::tests::a_dangling_symlink_in_a_tree_does_not_fail_the_walk`), and the aarch64 image is
reused by the same rule as the first machine's: a stamp per image, a *boot-file refresh* when only
the kernel, loader or initramfs moved, and a package install only when `rootfs.toml` did.

**A divergence the measurement retired.** The template's `@SERIAL_GETTY@` line rendered a getty on
the second machine's serial node whenever the session had no window. Measured with the whole
rendering removed: the windowless boot still reaches `mpos login:` on the wire, and `root` (empty
password) logs in and runs `id` - because the vcmux carries the current virtual console's output
*to* the wire and routes the typed bytes back to it, which is also exactly how the first machine's
`--mode terminal` session has always worked. So the getty was a second login prompt next to the one
busybox init respawns, and the placeholder, the renderer and the per-machine rule are gone; the
inittab is one file both images get verbatim (`rootfs::merge_overlays` no longer renders anything).

**Measured (2026-10-08), after the change:**

| Gate | Result |
|---|---|
| `cargo xtask all --arch aarch64` | exit 0: 2 GiB image created, GPT laid, ext4 formatted, **34 packages** installed into the mounted root partition, boot files written, `rootfs: completed` |
| `cargo xtask run --arch aarch64 --mode terminal` | the image is *reused* (no rootfs step at all when nothing moved), boots to `mpos login:`, `root` + `id` answers `uid=0(root)` on the wire |
| … after `touch target/aarch64-unknown-none/release/mpos` | `Updating the image (boot files only)` - no package install - then a boot whose cmdline is `root=/dev/vda2 rootfstype=ext4 rootflags=rw console=ttyAMA0` and whose script prints `MPOS_SWITCH_ROOT_OK: / is /dev/vda2 (ext4)` |
| `cargo xtask test --arch aarch64` | `=== SUMMARY pass=94 fail=10` - **the same ten names** as the pre-refactor baseline (`t-9p, t-brk, t-dsp, t-eventfd, t-inotify, t-mmap, t-mprotect, t-mremap, t-preadv2, t-statfs`), so the extraction cost no regressions; those ten are the in-flight ABI work's. Re-run once more with `build/aarch64-blk.img` deleted first, so the sweep had to lay its own block-check fixture: same summary, same names |
| `cargo xtask test` (first machine) | **stage 1 is red in this tree, and not here**: `cargo test -p mpos --lib` fails on `mm::vma::tests::mmap_placement_rules_over_a_lazy_arena` (`assertion left == right failed: no gap within one page of the cursor`), a test the *uncommitted* kernel work in this tree adds (`git diff src/mm/vma.rs` introduces it), so stage 2 never ran. The parts of the first machine's path this change touches were measured on their own: the full install below, and the boot-file refresh |
| `cargo xtask run --mode terminal` (first machine) | full install (`OK: 549.9 MiB in 164 packages`, `create_user` and the chroot half included), boots, `root` + `id` answers `uid=0(root)` |
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | 88 passed |
| `cargo clippy -p mpos-xtask --target x86_64-unknown-linux-gnu --all-targets` | clean |

**Gaps this change names rather than closes.**

- **The kernel's aarch64 comments still name `cargo xtask boot-aarch64`** (`src/board/aarch64.rs`,
  five sites; `arch/src/arch/aarch64/mod.rs`, one). They belong to the files carrying in-flight
  work in this tree, so they are updated with that work rather than from an unrelated commit; the
  sentences are about *why* a failed check halts the machine, which is unchanged.
- **Nothing asserts the `M*` chain any more.** The checks run and print, and a failure halts the
  machine - so the evidence is on the wire for a person or a log reader - but a check that
  *silently* stopped running would no longer be caught by a gate. The sweep reads the test protocol
  only. Re-pinning the chain means deciding what it should be a verdict *of* on a machine whose
  userland is a distribution (`tests.toml`'s groups are the current answer to "what must run").
- **The aarch64 sweep still stages its payload on a disk** rather than into an initramfs the way the
  first machine's sweep does. That is not decoration: the aarch64 kernel's `check_blk`/`check_fs`
  run on every boot and halt the machine without a pattern image and a partitioned ext4 root, so a
  diskless test boot is a kernel change, not an `xtask` one.

---

## Appendix — the evidence behind the status board

The nested notes and measurements that sat under `ROADMAP.md`'s checklist before
the split, in the same order. A reader following a `(§N)` pointer does not need this;
it is kept because each entry was a measurement made at a particular moment and some
are not restated in the phase sections above.

**Core (Phases 1–3)**
  - [x] a trapped signal ends a blocking terminal read with `EINTR` and one
        nobody trapped and nobody cares about does not (`t-tty-eintr`); the
        `VMIN`/`VTIME` pair decides what a read waits for at all
        (`t-tty-vtime`) (§2.5)
  - [x] `SA_RESTART` is acted on: an interrupted call whose disposition carries
        it is *rewound* through the frame the handler runs on, so the caller
        never sees the `EINTR` (`t-sa-restart`), and the calls Linux never
        restarts — `poll`, `select`, `epoll_wait`, the sleeps — still answer it
        (§2.5)
  - [x] `O_NONBLOCK` on a terminal: the measured `EAGAIN`/partial-byte table,
        through `open`, `fcntl(F_SETFL)` and `ioctl(FIONBIO)`, on the slave and
        on the master's own read (`t-tty-nonblock`, host tests) (§2.5)
  - [x] every other blocking wait a signal can end: pipe read/write, eventfd,
        signalfd, inotify, unix datagram/accept/connect, the pty write
        back-pressure, the AF_PACKET and route-netlink reads, `wait4`/`waitid`,
        and `nanosleep`/`clock_nanosleep` with the remainder in `rem`, each
        rewound under `SA_RESTART` where Linux rewinds it — including
        `ioctl`'s per-request verdict for the two DRM sync-object waits
        (`t-wait-eintr`, `t-sa-restart`, `t-drm-sync`). Named remainders: the
        console's serial transmit wait, whose park is still fatal-signal-only,
        and `FBIO_WAITFORVSYNC`, which discards its `WaitEnd` so a signal cannot
        end it at all (§2.5)
  - [x] `flock` owned by the open file description, record locks by the process
        (`t-filelock`) · read/write deliberately *not* enforced: Linux advisory
        locks never refuse an `open`ed read, and mandatory mode is a deprecated
        per-file opt-in (§3.2)
  - [x] ext4 `atime` bumped on read, with Linux's `relatime` rule so a read
        that sees no change costs no device write (§3.3)
  - [~] inode attribute flags — `FS_IOC_GETFLAGS`/`SETFLAGS`, `lsattr`/`chattr`'s
        word, with `IMMUTABLE` and `APPEND` *enforced* rather than stored
        (`t-fsflags`; ext4 persists `i_flags`, ramfs keeps the same subset) · the
        flags this kernel cannot honour are `EOPNOTSUPP`, never stored (§12.4b)
  - [x] the files the machine's `ls*` tools open: `/proc/modules`,
        `/proc/self/mountinfo` (and `/proc/<pid>/mounts`), `/sys/block` with a
        partition's `size`/`partition`, and an empty `/sys/bus/usb/devices`
        (`t-mountinfo`, `t-fsflags`) (§12.4b)

**Networking (Phase 4)**
  - [x] per-send cmsg boundaries on `SOCK_STREAM`: each handover carries the
        stream position its send ended at, and a read stops there, so two sends
        of one byte each arrive as two deliveries with their own descriptors
        (`t-unix-scm`)
  - [x] a *data-less* `sendmsg`'s handover is delivered on the read that asks
        for it: the record is due the moment it is queued and no byte will ever
        arrive *with* it, so the read answers with the record and a zero-byte
        return instead of parking on a byte that cannot come (`t-unix-scm`)
  - [x] the loopback is a real wire — `lo` is an interface over smoltcp's own
        loopback device carrying `127.0.0.0/8`, and a socket's interface is
        decided by the first address it is given, because the stack now keeps
        one `Fabric` (interface + device + socket set) per wire and smoltcp has
        no way to move a socket between them (`t-udp-loopback`). Raw/ICMP
        sockets deliberately stay on eth0's (§3.3.1)
  - [x] `listen`/`accept` on IPv4 — a listener is a *pool* of listening sockets
        plus the queue of the ones that completed, refilled after every poll;
        the contract is Linux's, each rule measured first, and closing a
        listener `FIN`s its unaccepted queue because an unsolicited `RST` is
        not a packet smoltcp can send (§3.3.2, `t-tcp-listen`)
  - [x] `SO_REUSEADDR` at `bind`/`listen` — and the rule it gates is *not* "the
        new socket asked": measured on Linux, a holder that is **listening**
        keeps its name whatever either side says, while any other holder
        (a `TIME-WAIT` 4-tuple, an established connection, a bound socket)
        yields it only to a caller that set the option **and** whose holder had
        set it too — which is why Linux copies the option onto an accepted
        socket, and why a restart comes back onto its own port (`t-tcp-listen`)
        (§3.3.3)
  - [ ] `SO_LINGER{on,0}`'s egress reset / buffer sizes still record-only
        (§3.3.3)

**Memory (Phase 5)**

**Process (Phase 6)**

**ABI gaps → `ENOSYS` (remaining)**

**Drivers (Phase 7)**
  - [~] the compositor's **mouse cursor**: the plane is now driven, and what is left is a compositor
        that hands it an empty image. Measurements taken in one boot each on a `--mode gui` image with
        the card instrumented:
          * *The plane was unusable, and that was this kernel's doing.* weston builds one atomic state
            per plane and `drm_plane_state_add_props` adds `SRC_X`/`SRC_Y`/`SRC_W`/`SRC_H` and
            `CRTC_W`/`CRTC_H` to every one of them without asking whether the plane has them; a handle
            of 0 reaches the card as property 0, is refused, and takes the whole commit down with it.
            The cursor plane published only `type`, `FB_ID`, `CRTC_ID`, `CRTC_X`/`CRTC_Y` and
            `IN_FORMATS`, so that is what happened: weston set `FB_ID` to **0** twice and never gave the
            kernel an image — the pointer moved, clicked, and was never drawn. Publishing the four
            rectangles (and honouring them by *being* the image: this card's cursor cannot scale or crop,
            so the identity is the only rectangle a commit may name and anything else is `EINVAL`) is
            the fix, and the same instrumented boot then shows weston binding `FB_ID` 100, then 101, and
            the kernel compositing the image at the pointer (`cursor.rs`'s blend, unchanged).
          * *What is left is a compositor that uploads nothing.* The same boot, with the contents of the
            bound buffer counted as it is composed: with `cursor-theme`/`cursor-size` in `weston.ini` the
            buffer is **all zero** (0 opaque pixels) on every commit, and with the theme commented out it
            carries the built-in arrow (5 opaque pixels, white) — the small cursor this card has always
            shown, now arriving through the hardware plane. So the kernel half is complete and measured,
            and the themed cursor is libweston's cursor path: a named gap, not a stub.
  - [x] **which consoles the kernel has is a `console=` list**, and nothing about
        how the machine was launched reaches it: `mpos.headless` /
        `mpos.nomirror` / `mpos.mirror` / `mpos.compositor` are gone, the kernel
        reads `console=ttyS0` (the wire) and `console=tty0` (the virtual console
        on the framebuffer), gives the log to every console named and parks on the
        last — so mirror is that list with the framebuffer last, gui is the
        framebuffer alone, and terminal and weston are the same one-word list
        (§7.1c). Two gaps are named there: `console=ttyN` pinning is refused
        rather than approximated, and `/dev/console` has no preferred-console
        plumbing.

**Wayland compositor (weston) support** — the cross-cutting work a rootful
  - [x] a pointer with real artwork — `weston.ini` names `cursor-theme=
        breeze_cursors` and a `cursor-size`; `breeze-cursors` was already in the
        image, but with no theme named weston drew only the fixed ~16px arrow
        libwayland-cursor builds in, which is what made the cursor look tiny and
        the theme "not applied" (a shell-config fix, not a kernel one)
  - [x] **and that owner is never invisible.** A master taken by a holder with no
        controlling tty — seatd handing weston the card, or a launch chain that
        skipped `TIOCSCTTY` — used to bind to *no console* at all, and an unbound
        master could not be revoked: `master_ok` passed it, `painted()` said the
        text console was free to draw, and the two raced on one scanout (the mouse
        that repaints a text VT, the shell-and-compositor flicker). `SET_MASTER`
        now binds to the console on the screen when the taker names none, so every
        held master has a console to lose.
  - [x] **and the negotiation tracks the screen.** `complete_switch` advances the
        pure handover table's `active` alongside the `ACTIVE` atomic; left at its
        boot value it asked console 1's `VT_PROCESS` promise on every switch after
        the first, so a compositor's `relsig` went to the wrong session (or none).
  - [x] **and the keyboard follows the screen, not the grab.** A keystroke reaches
        the shell unless the active console is in `KD_GRAPHICS` — the compositor's
        own `KDSETMODE` word that the screen is its to draw — so a compositor's
        typing does not land on the shell behind it whether or not libinput has
        taken `EVIOCGRAB` yet; a `--mode gui` text console (`KD_TEXT`) keeps its
        keyboard (`input::service`)
  - [x] **and the promise belongs to a process, not to a terminal.**
        `relsig`/`acqsig` go to the process that wrote `VT_SETMODE` - Linux's
        `vc->vt_pid`, delivered with `kill_pid` - and not to whichever group holds
        the foreground when the signal goes out. Two rules come with it: the last
        close of the descriptor that wrote a promise does *not* end it (seatd's
        `vt_open` sets the mode and closes the tty before the compositor has the
        screen, so clearing on close unmakes the claim in the same breath it is
        made), and a promise whose keeper has died is ended at the delivery that
        cannot land - Linux's `reset_vc`: `VT_AUTO`, no signals, `KD_TEXT`, the
        switch proceeds. Before this, quitting weston with `^C` handed `SIGUSR2` to
        the compositor or to the login shell - `ash` calling it "User defined
        signal 2" while the desktop was dying mid-release - and the seat was left
        occupied, so `./start-weston.sh` could not be run a second time. Measured
        against `drivers/tty/vt/{vt.c,vt_ioctl.c}`; pinned by `vtmode`'s host tests
        and by `t-seat` (§7.1)
  - [x] **and it comes back in graphics mode.** seatd opens the console, writes `VT_SETMODE` and closes
        the descriptor — writing it with `relsig` and `acqsig` both **zero**, the shape a seat manager
        that watches the console itself rather than waiting to be told. So the handover has no keeper to
        signal, `release_console` takes Linux's own `reset_vc` road, and the console lands back in
        `KD_TEXT`. What Linux does not do is let the word matter afterwards; there a master drives the
        screen because it is the master. Here the veto the text console paints under is
        `master_holds_screen`, and that asks `KD_GRAPHICS` as one of its three facts — so a handover
        nobody answered cost the compositor its screen for good: the text grid repainted over the desktop
        and every `MODE_ATOMIC` was answered `EACCES`. The master's own console is now a graphics console
        again the moment it is the one on the screen and a live master still holds the card; a master that
        has let go, one on another console, and one whose console is not the one being shown all leave the
        text console its screen.
  - [x] **and `VT_ACTIVATE` does not wait**, because Linux's is `set_console()`:
        the request is recorded, the console callback runs the negotiation, and the
        ioctl returns 0. The sleeping form here was attributed to a `vt_release`
        timeout that is not in the current console code at all - and it would have
        deadlocked a seat manager once the promise was aimed where Linux aims it,
        since the process asking for the switch is the keeper of the answer the
        wait is for. `VT_WAITACTIVE` is the one console ioctl that sleeps, and
        stays so; a console number the machine has not got is `ENXIO` (§7.1,
        `t-seat`)
  - [x] **a terminal question is answered by the emulator that owns the screen, and
        by exactly one of them.** Measured on Linux: a pty pair's line discipline
        forwards `ESC [ 6 n`, `5 n`, `c`, `? 6 n`, `OSC 11 ; ?` and `DCS 1 + q`
        byte for byte to the master and answers none of them (6 of 6 runs). This
        kernel answered everywhere - and so told a shell on a `--mode terminal`
        boot where its cursor was according to a screen nobody drew. `fish`'s
        prompt spacing pads the dangling line out to the *reported* column count
        (measured: `stty cols 40` turned the pad from 80 cells to 40 while the
        report still counted 80), so on any window wider than the guess nothing
        wraps and the `CR` + `ESC [ K` that close the block erase the line
        `echo -n hello` had just printed. `Attached` now decides per write from who
        reads the master, and the cursor a report answers with is the cursor the
        grid paints: both drive one `term::cursor::Cursor` - window size, scroll
        region, deferred autowrap, scroll at the bottom, `DECSTBM`, save/restore,
        `ESC D/E/M`, alternate screen, and the echo of what was typed, which the old
        private model never saw. Pinned by the 37-stream report/grid parity test and
        the answering-and-forwarding pair on the host, and by `t-tty-query` in the
        guest (§7.1b). Three gaps named there: the answering half needs a boot with
        a framebuffer to be pinnable at all; a serial line's real width is not
        knowable from the guest, which is also Linux's answer; and one byte is still
        counted as one cell, which is wrong for UTF-8 and needs a measurement first.
  - [x] **a last-resort stack for `#DF`.** No IST slot existed anywhere and the TSS
        carried `RSP0` alone, so a fault whose kernel stack could not take a frame -
        overflowed off its bottom, or walked over by a wild write - promoted to
        `#DF`, which could not push either, and the third fault reset the CPU into
        QEMU's `-no-reboot` exit without the kernel writing one byte: a machine that
        vanished with no panic was structurally unreportable. `#DF` now runs on its
        own 32 KiB stack, so `fatal()` names the vector and the `rip` (remaining,
        named: no NMI or machine-check handler is installed, so only `#DF` has the
        last resort - §7.1)
  - [x] and the device is the owner's while it holds it: a reader that is not
        the grabber is told nothing - not the records, and not a readable answer
        either - because every client has a *position* in the device's window
        rather than a turn at one shared queue. Two readers of an *ungrabbed*
        node each see every event, which the shared queue could not do either
        (whoever read first consumed it for everybody)
  - [x] **the manager's handoff to a worker** — `udev_monitor_send_device(monitor,
        worker->monitor, dev)` hands the device over `NETLINK_KOBJECT_UEVENT`, and
        the family had no write side at all: the call is `sendmsg` with a 12-byte
        `struct sockaddr_nl`, which the send path read as a `sockaddr_in` and
        refused for being short of 16 bytes (`EINVAL`), after which the manager
        killed every worker that would not take its message. The uevent family
        now carries userland messages: a port id per socket (the process's pid
        when it is free, a unique number otherwise - Linux's order of preference,
        and libudev reads its own back with `getsockname`), unicast by `nl_pid`,
        group delivery for a send with no port id, the sender's address and
        credentials stamped on every delivery, and the payload left opaque
        because the wire format is libudev's. **Verified on the desktop image**:
        `/dev/input/event0` now carries `ID_INPUT=1`, `ID_INPUT_KEY=1` and
        `ID_INPUT_KEYBOARD=1`, `/run/udev/data/c13:64` holds them, and the kernel
        log has no handoff failures left (§4.4 has the measured rules,
        `t-uevent` the pins)
  - [x] **and the handoff was only the first of three reasons libinput would not
        take the device** - the other two are in the interfaces a compositor
        *reads* rather than in the events, and neither is a defect the log
        names. `evdev_device_have_same_syspath` re-derives a device's `/dev`
        node from its own `fstat` devnum and refuses it unless the string it
        resolves is the one udev gave it, so `/sys/dev/char/M:m` has to point
        at the *device* and not at another link to it - it pointed at
        `../../class/input/event0`, the two answers disagreed, and the keyboard
        was dropped without a word. The links carry the device tree's own path
        now (`../../devices/virtual/...`), block devices excepted because this
        tree has no device directory for them. And `EVIOCGPHYS`/`EVIOCGUNIQ`
        answered `ENOTTY` for the strings a virtio input device does not have,
        where `libevdev`'s `libevdev_set_fd` tolerates that failure only for
        `ENOENT` - the errno Linux's own `str_to_user` returns for a NULL
        attribute - so the wrong one made the whole read fatal, and libinput
        gave up with "failed to create input device" for every node on the
        machine (§7.2; `t-evdev` pins the `phys`/`uniq` answer *and* the link's
        one-hop target - against the string udev knows the device by, so the
        comparison libinput makes is the thing held, not the spelling of the
        path)
  - [x] **and a node's hang-up had to become sayable** - the wait udev parks on
        reads only that. `spawn_wait` watches its child's pipe with epoll, takes
        a read of 0 as "look again" and ends on `EPOLLHUP` alone, so an
        end-of-stream reported only as `EPOLLIN` left the device manager sitting
        on every `IMPORT{program}` rule until its own event timeout expired.
        `Inode` therefore answers `poll_hup`/`poll_err` beside `poll_ready`: a
        pipe's read end calls a lost writer a hang-up and its write end calls a
        lost reader an error, while a socketpair folds the peer's departure into
        the hang-up and stays readable beside it, because `unix_poll` sets
        `EPOLLIN` and `EPOLLHUP` together where `pipe_poll` drops the read
        condition. `poll`, `select` and `epoll` report the pair whether or not
        the caller asked for them, which is Linux's rule and sometimes the only
        report a parked waiter will ever get (§1.1, `t-epoll` pins the shape)
  - [x] **and the read itself had to stop parking** - the fourth gap, and the one
        the other three hid. `libinput` opens every evdev node with `O_NONBLOCK`
        and pumps it from `epoll`; this node's read never looked at the flag, so
        the first read with an empty record window parked on the service task's
        wakeup list. weston therefore created `event0`, logged it as a keyboard,
        and then stopped - no pointer, no failure, its event loop gone with
        nobody left to answer the input it was waiting for. `EAGAIN` is what
        Linux answers there and what libinput's loop is built around; the bit is
        mirrored into the node by the same `sync_nonblocking` every other
        blocking node here is told by, so nothing invented a second route for it
        (043c9c6). **Verified on the desktop image in the mode weston actually
        runs in** (`cargo xtask run --mode weston`; the kernel console is off the
        scanout, so the serial line is trustworthy as well as readable): one run
        logs `event0 - QEMU Virtio Keyboard: is tagged by udev as: Keyboard ...
        device is a keyboard`, `event1 - QEMU Virtio Tablet: is tagged by udev
        as: Mouse ... device is a pointer`, `libinput: configuring device` for
        both, and both associated with output `Virtual-1` - where before it was
        `failed to create input device` twice and `no input devices found`.
        That run is an operator's, though, and a compositor's keyboard is too
        much to leave resting on one: `t-evdev` pins the read rule in the gate -
        an empty node opened `O_NONBLOCK`, and the same node with the bit set
        through `fcntl(F_SETFL)` afterwards, each answering `EAGAIN` instead of
        parking - so a fifth time this regresses, the sweep says so and no one
        has to watch weston's log for the silence
  - [ ] `SOCK_SEQPACKET` still carries no boundaries over a *named* socket (the
        stream this family has), which is the one documented divergence left on
        the path from `udevadm` to a tagged device node

**Compositors beyond weston (niri, wlroots)** — what a *second* compositor family

**Security (Phase 8)**

**Performance (Phase 10)**

**Architectures (Phase 9 — aarch64 stages 1a-1f, 2, 5a, 5b and 6a landed: the
  - [x] 13.1 `hal` is arch-neutral: its 14 `target_arch` gates became capability flags
        (`mpos_msi`/`mpos_serial_ring`/`mpos_fpu_trap`/`mpos_segments`/`mpos_console_input`)
        declared per target in `.cargo/config.toml`; `src/hal` `target_arch` is now 0, both
        bare-metal clippy targets and the depgraph are green
  - [~] 13.2 callers stop spelling the architecture: `target_arch` outside `src/arch/`
        (250 at audit → 169 now). Landed: `console.rs` 10 → 0 (`mpos_framebuffer_console`,
        `mpos_serial_ring`), `uevent.rs` 4 → 0 (`mpos_net`), `mm/mod.rs` 4 → 0
        (`mpos_dynamic_mmio_window`), `sched/runtime.rs` 8 → 0 of the FPU-trap protocol
        (`mpos_fpu_trap`), `syscall`'s socket dispatch + read/write legs 22 → 0
        (`mpos_net`), `rand.rs` 2 → 0 (a new baseline `hal::Entropy`, the seed sources moved
        into `arch/<arch>/`). And a later slice took six more out for a *different* reason:
        the four `ifreq` link ioctls (`fs/ioctl.rs` 22 → 18) and the unix-socket half of
        `fs/mod.rs`'s nonblocking push (4 → 2, both of the remainder the input driver's, which
        is a real absence) had been gated when `net` was the first machine's alone, so §13.4
        made their gates **stale** - a deletion, not a flag. `user/tests/src/bin/t-ifreq.rs`
        now drives all six requests on both machines, which is what keeps a gate from going
        stale there unnoticed again (a read arm and a write arm of one family disagreeing is
        exactly what it catches). And the register-ABI relocation's first stage landed:
        `hal::Gate` is the **gate frame's** seam - the operations over the opaque `*mut ()`
        the syscall gate passes - so `handle_execve`'s two per-machine arms became one
        `hal::set_exec_return`, and the seccomp filter's instruction pointer comes from
        `hal::user_ip` (`proc.rs` 23 → 20). It is deliberately a *separate* trait from
        `UserFrameOps`: on x86_64 the gate's frame is the entry stub's register block on the
        kernel stack, on aarch64 it is the task's own `UserFrame`, and no type both machines
        share can be named without inventing one - which would be the leak the seam exists to
        remove. Introducing it also made `linux_dispatch`/`linux::dispatch` **`unsafe fn`**,
        which is what they always were: clippy's `not_unsafe_ptr_arg_deref` (deny) found a
        `pub` fn dereferencing its raw-pointer argument behind a safe signature. Both machines'
        gates and the kernel's own self-check calls now say so explicitly. The **signal frame**
        followed it (Stage 2): `hal::SignalFrame` (`enter`/`exit`) is the capability, the x86
        `rt_sigframe`/`sigcontext` layout moved into `src/arch/x86_64/signal.rs` with the
        `SIGFRAME_*`/`UCONTEXT_*` constants out of `sched::signal`, and `proc.rs`'s
        `build_frame`/`handle_rt_sigreturn` became policy over `hal` plus the two scheduler
        calls the machine cannot make - so `struct RegsFrame` is deleted and the signal family
        is gated on `mpos_signal_frame`, not on an architecture - and Stage 3b gave the flag a
        second declarer: `src/arch/aarch64/signal.rs` lays arm64's `rt_sigframe` out behind the
        same trait, so the second machine delivers handlers too (the pin is
        `t-signal-frame`, and it found three defects on the way: a thread pointer zeroed by the
        frame's own `apply_frame`, the handler's `x0` overwritten with the call's result, and
        `orig_x0` missing from the trap frame for `SA_RESTART`'s rewind). `lib.rs`'s 16 module gates and
        `main/`'s per-arch arms stay by design; still open: `syscall` 41 (`ioctl` 18, `file` 9,
        `mod` 5, `proc` 3, `nr` 2, `fs/mod` 2, `abi` 2), `drivers` 36, `vfs` 7, `mm/{heap,elf}`
        4, `logging` 6 (gate now *stale*) - `sched` names no architecture at all since the
        ptrace/stop block's width became a machine fact. Stage 3a of the same line is the
        EL0 trap's register preservation: `t-regs-across-syscall` measured that this
        machine destroyed a program's SIMD state on its own `svc` (one of 41 registers,
        `v0`), and both EL0 trap stubs now carry the interrupted `q0..q31`/`FPCR`/`FPSR` in
        their own frame. Nothing above the machine layer changed for it, which is why the
        count is untouched
  - [x] 13.3 aarch64 disk root: the ext4 `check_fs` mounts is **registered** with the block
        layer (`vda`, major 254, `/dev/vda`), `xtask` lays the repository's `rootfs/` overlay
        and the three init images on that disk, and the machine now **switches its root** onto
        it - `/` is the disk, the ramfs moves to `/oldroot`, `/dev` is re-mounted, and `/init`
        is read from the disk and run as pid 1 (`M7_DISK_OK`, `M7_ROOT_OK`). The init's whole
        file lifecycle runs on ext4, which is the strongest form the claim has. *Deviation*:
        the disk is a bare ext4, not a partitioned image, so `rootfs.toml`'s
        `image_size`/`esp_size` geometry is still x86_64-only - a partition table buys nothing
        for a machine whose firmware boots from the ESP
  - [x] 13.4 aarch64 `net`: **landed - the second machine runs the same stack over its
        own adapter**. `drivers::virtio` carries the transport-neutral `NetDevice`/`VsockDevice`
        contracts and `net`/`net::vsock` hold `Arc<dyn …>`, naming no transport;
        `VSOCK_PER_CONNECTION_BUFFER` moved beside the contract and the `/proc/net` collector
        took `procfs`'s own gate, not a new capability name. `mmio::MmioNet` is the MMIO
        adapter as a `NetDevice` (persistent, like `MmioBlk`: the ring it posts stays posted),
        `check_net` opens it once, asserts the MAC and the TX queue **through that handle** and
        publishes it, `board/aarch64.rs`'s `init_net` hands it to `net::init` +
        `net::spawn_service` (`M8_NET_STACK_OK`), and `lib.rs`'s gate is `target_os = "none"`
        with `mpos_net` declared by both machines. Measured: the aarch64 sweep stages 10 of
        the 14 std-side programs and passes **10/10** - including a TCP
        connect/accept/read-write round trip, UDP over the loopback and an `AF_UNIX` name -
        which is the pin this job chose (real EL0 programs, no kernel-side ping hook). The
        flip exposed three stale "`net` is x86_64's" gates in shared code: `close(2)`'s
        `close_last_socket` and `unlink`'s `unix::unbind_node` (`not(aarch64)`; the first was a
        measured port *leak* - `t-socket`'s 34567 still held when `t-tcp` bound it) and
        `ioctl`'s two `ifreq_*` helpers (aarch64 arms answering `None`, so interface ioctls fell
        to `ENODEV`). And `/tmp` is a tmpfs on this machine now, as on x86_64: ext4 cannot hold
        a socket node, which is what `t-unix-rustix` measured (`EOPNOTSUPP` from
        `create(name, SOCK_FILE)`) before the mount. What the `aarch64` group still names is
        harness-only: `t-stdspawn`/`t-socketpair` run another program out of a `/bin` this
        machine's root does not have — and 13.6 gave it one, so the group is empty now
  - [~] 13.5 `cargo xtask test --arch aarch64`: the **silent wrong-run is closed** —
        `test --arch aarch64` refuses explicitly (`test_sweep_machine`, pinned by an
        `mpos-xtask` unit test) instead of falling through to the x86_64 sweep and reporting a
        verdict for a machine it never booted. Next: the aarch64 sweep itself — build the user
        programs for aarch64, pack them with a generated `/init` that runs `/bin/t-*`, boot
        under AAVMF and parse the same `=== RUN/PASS/FAIL/SUMMARY`/`END`; a group that cannot
        run on aarch64 yet is named in `tests.toml` with its reason, never silently skipped.
        Landed: cargo builds the musl user programs for aarch64 (the target names `rust-lld`;
        the host `ld` rejects `--fix-cortex-a53-843419`, measured — 28 programs, static
        `ET_EXEC`), and the sweep is wired — a musl runner (`aarch64_init::build_sweep_init`) is
        packed as `/init` beside them and the run reads the same protocol. The first run is
        **red with the reason located**: the runner's `posix_spawn` takes an EL0 write fault
        (ESR `0x9200004f`, address `0x3fadafe0`) at `pc 0x272d90` — inside the runner's own R+X
        segment, so it is musl's spawn and not a test — and the machine loses its init, so no
        summary prints. **Fixed**: the handler read `WnR` from `ESR_EL1` bit 7 instead of bit 6,
        so every fault looked like a read and the COW resolver was never reached. The run now
        passes that point and stops at the next gap — two faults in the runner (a write to
        `0xffffffffffffff5c`, translation fault L1; a read of `0x58`, L2) — where the aarch64 ABI
        is still short. **Both fixed**: a write fault now reaches the COW resolver (the `WnR`
        bit), and the outgoing task's live thread pointer is captured on switch-out (musl
        installs its base with a direct `msr`, so the record had gone stale and the base was
        reset to 0 on the first switch after a spawn). The payload also stages only ELF files
        now — cargo's `.d` dep-info files start with `t-` and were being run as programs
        (`exit=127`). **The run completes**: 14 programs, `=== SUMMARY pass=0 fail=14`,
        `=== END`. **And the same class struck again, in the copy**: `spawn_copy` and
        `spawn_thread` inherited `Task::tls` — the *record* — rather than the base the parent was
        running with, so the sweep's first `posix_spawn` child (whose parent had set its own base
        after its last dispatch, so the record still said 0) ran with `TPIDR_EL0 = 0` and faulted
        writing musl's `errno` at the same `0xffffffffffffff5c`. arm64 Linux's `copy_thread`
        hands the child the parent's live `tp_value` when `CLONE_SETTLS` is absent; both call
        sites now ask the register (`running_tls`). Measured: `t-rename` and `t-stdspawn` — the
        two that faulted — reach their own code and fail on the *harness* (no `/tmp`), not on the
        kernel. **Classified, and the sweep is green on what this machine can serve**: of the
        14 std-side programs, 3 run and pass (`t-rename`, `t-rmdir`, `t-stdargs` —
        `=== SUMMARY pass=3 fail=0`, exit 0) and the other 11 are named in `tests.toml`'s new
        `aarch64` group with their reasons. The vehicle is a rule that matters as much as the
        list: **a group named after a machine is that machine's own** — invisible to every other
        machine's sweep — so naming them cannot take them out of the x86_64 gate (pinned: every
        one of the 11 still runs there by default). Ten are the socket family (this machine's
        table answers `socket`/`socketpair` with ENOSYS, measured: syscall 198/199 — §13.4's
        flip is what closes it) and `t-stdspawn` is a *harness* gap, not a kernel one: its
        subject is other programs (`/bin/true|echo|cat|false|sh`), which the x86 harness has
        from busybox and this machine's disk root has no `/bin` for — while the kernel's own
        spawn path is exercised on every program by the runner itself. Reaching three took the
        machine setup the x86 `/init` does with busybox: the runner now makes `/tmp` (the x86
        side mounts a tmpfs; this one has no busybox and a writable disk root, so a directory)
        and hands `t-stdargs` the argv the x86 harness's PATH lookup hands it
        (`arg0("t-stdargs") one two three`), and a `=== SETUP_FAIL` line now refuses the
        verdict outright rather than letting an unprepared machine read as failing tests.
        `--groups`/`--all-groups` stay refused for this arm: the file's vocabulary also names
        the `no_std` raw-syscall suite, which has no aarch64 port
  - [x] 13.6 aarch64 `/bin`: **the harness gap closes and the second machine's own list is
        empty**. The gap was never the ABI — `t-stdspawn` runs
        `/bin/{true,false,echo,cat,sh}` and `t-socketpair` runs `/bin/cat`, while the
        machine's ext4 root carried no `/bin` at all (the payload's own programs sit flat at
        `/`). **The tools are built in-tree, not installed**: `xtask/src/aarch64_bin.rs` is
        five applets of one multi-call binary, compiled by the
        `rustc`/`aarch64-unknown-linux-musl` path the sweep's runner and its musl helper
        already require. Installing busybox — the x86 harness's answer — would have put an
        Alpine fetch *inside the aarch64 gate*, whose failure would read as a broken kernel;
        keeping the gate's reds about the kernel is what `tests.toml`'s groups exist for. `sh`
        is a small shell and its own comment says exactly which subset it implements
        (`;`/newline separation, `'`/`"`/`\` quoting, `<`/`>`/`>>`/`>&N`, the
        `echo`/`exit`/`cd`/`pwd` builtins, external programs by path or `PATH`) — honestly *not*
        busybox — and it moves real descriptors instead of modelling them, because
        `t-stdspawn` asserts the child put `captured-err` on **stderr**. The staging step also
        learned the two things this needed: a payload name may carry a directory (the kernel's
        tar unpacker already creates missing parents on demand) and every file it lays down is
        mode 0755, which `mkfs.ext4 -d` preserves — without that `/bin` would land 0644 and
        `execve` would answer `EACCES`. **Measured: the aarch64 sweep is
        `=== SUMMARY pass=12 fail=0`**, `t-stdspawn`'s `output() captured both of the child's
        streams` and `t-socketpair`'s `read back what the pair carried` among the passes; x86
        is unchanged (`pass=142 fail=0`, the same 10 left out). `tests.toml`'s `aarch64` group
        is **empty** and stays as an empty group - the mechanism is what names a gap, and the
        day a call cannot be served is the day a name goes back in. What remains outside the
        machine is `external`, and its reason is the internet
  - [x] 13.7 aarch64 `SerialRing`: the PL011's line (SPI 33) is routed through the GIC and the
        second machine queues its console output instead of waiting on the wire — with the queue
        itself lifted out of `drivers/serial.rs` into `drivers::tx_ring`, behind a three-method
        `Transmitter` both machines implement. `M5_SERIAL_TX_OK` counts what writers queued against
        what the *handler* handed over, so a polled write (or a line that never fired) fails rather
        than passing; x86_64's sweep is unchanged
  - [x] 13.8 aarch64 interactive shell: `run --arch aarch64` stops booting a hand-encoded read/echo
        loop and hands pid 1 to the machine's real shell — an in-tree `/init` that makes `/tmp` and
        `execve`s `/bin/sh`, with the `sh` applet reading a line at a time (prompt on a terminal,
        silent on a pipe) and `ls` added to the `/bin` image. The superseded `looping_init_elf` is
        deleted rather than left as dead code, and both new behaviours are pinned by `t-stdspawn`
        on **both** machines
  - [x] 13.9 aarch64 `uaccess`: the range check accepts a page the tables do not describe yet
        (the live VMA table answers for an absent page, a copy-on-write page counts as writable,
        `len == 0` is accessible — the three rules x86_64 has always had), and the **EL1**
        synchronous entry resolves the data aborts that fallback makes reachable, because the
        kernel fills a caller's buffer through the caller's own tables. Pinned by a new
        `t-lazy-buffer`         (a 1 MiB `mmap` read into whole, and a `fork` child reading into a page
        its parent wrote) on both machines; both failed with `EFAULT` before. It also found a
        **named gap**: a non-empty `std::fs::remove_dir_all` faults at `pc 0` on this machine,
        which nothing in either suite had exercised — refuted and closed by 13.10, whose cause
        was a `struct stat` sixteen bytes too long rather than the tree walk the fault looked like
  - [x] 13.10 aarch64 `struct stat`: the writer filled 144 bytes where this machine's struct is
        128, so the length is now the ABI's own (`STAT_BYTES`) — for the bounds check and the
        zero-fill as well as for the fields. Found by §13.9's named gap, and the fault was a
        return through a zeroed link register: musl's `stat`/`lstat`/`fstat` do not hand the
        kernel the caller's struct at all (`__fstatat` stats into one of its own on the stack and
        copies the fields out), so the sixteen extra bytes landed in the frame past *that* struct
        — which is why every program's own `stat` call had survived them, and why what died was
        `fdopendir`, whose frame held the return its caller needed. Pinned by a new
        `t-stat-buffer` (the kernel handed a guarded `struct stat` through
        `SYS_fstat`/`SYS_newfstatat`; it reports two guard words written against the old length)
        and by `t-rmtree` (a flat and a nested non-empty tree removed with
        `std::fs::remove_dir_all`), both on **both** machines
  - [x] 13.11 `statx` on Linux's layout: `struct statx` is 256 bytes and one layout on every
        machine, and the writer filled 152 of them with its clocks at 96/112/128 and its rdev
        pair at 72 — where Linux puts four 16-byte timestamps at 64/80/96/112, `stx_rdev_*` at
        128/132 and the file's device at 136/140. Found by §13.10's fix and named there rather
        than folded in: 152 bytes into a caller's 256 is an *under*-write, not a write past, so
        it needed its own measurement. The clients that read those fields are the ones that
        call `statx` explicitly — uutils' probes and musl's own wrapper — because Rust's `std`
        has no `statx` on this libc and `rustix`'s `stat` family only reaches for it on 32-bit
        targets. Pinned by a new `t-statx-agree` (one file's device, inode, mode, size, link
        count and three clocks, read through `statx` **and** through `lstat`, which have to be
        the same numbers) on **both** machines, and by the bare-metal `t-statx`, whose clocks
        and struct length were the two things it had deliberately skipped — and that pin found a
        second, older one on the way: x86_64's `struct stat` wrote `st_mtime` and `st_ctime` at
        80 and 88, where Linux has `st_atime_nsec` and `st_mtime`, so a program reading either
        clock read the next one along and `st_ctime` (104) read zero. The three clock seconds
        sit at 72/88/104 in *both* ABIs, so they now come out of the per-machine branches and are
        written once
  - [x] 13.12 aarch64 real Alpine userland: **the second machine boots a genuine Alpine `aarch64`
        installation to a login prompt**. `xtask/src/aarch64.rs` installs one (`apk --arch aarch64` under
        `fakeroot`, so the build stays unprivileged *and* the image keeps root ownership - `mke2fs -d` must
        run under the same `fakeroot` state or it writes a root filesystem owned by the build user, which is
        what `login` reads `/etc/shadow` with), lays it on the machine's bare ext4, and boots it: `busybox`
        init reads `/etc/inittab`, `openrc` runs its runlevels, and `getty` prints `localhost login:` on the
        PL011. The new `cargo xtask alpine-aarch64` is the verdict. It carries **no initramfs** - the disk
        root is the whole userland, and the kernel's root switch is what puts it in reach, so the handoff
        learned to boot a disk root without one. The Alpine root's `/init` (the new `rootfs-aarch64/` overlay)
        does the two things a cross-arch install cannot: it makes busybox's applet farm at first boot (the
        post-install script is the *target's* binary) and mounts `/proc`, `/sys`, `/tmp`, `/run` and `devpts`.
        **Three real defects came out of it, each fixed here** (see §13's table for the measurements): `USER_LIMIT`
        was x86_64's `1<<47` on a machine whose `mmap` cursor starts at `MMAP_BASE = 0xff00_0000_0000`, so every
        `MAP_FIXED` mapping a dynamic loader makes answered `EINVAL` and the first `dlopen` failed
        (`librc.so.1: Invalid argument`); `/proc` and `/sys` were behind a module-level `target_arch` gate whose
        deferral was over; and `boot::set_cmdline` was never called on this machine, so `/proc/cmdline` was
        empty. Also landed: the console tty's node is the *machine's* serial name (`ttyAMA0` here, `ttyS0` on
        x86_64 - one node, the name the psABI gives). Measured: `cargo xtask alpine-aarch64` exits 0 on
        `localhost login:`, x86 gate `pass=151 fail=0`, host 511, `boot-aarch64` green, depgraph 9/9, fmt clean.
        **Named, not fixed:** the virtual consoles (`tty1`..`tty4`) cannot reach the wire because this machine
        has no console multiplexer, so exact parity there needs the `vcmux` port - **closed by §13.16**;
        `hwclock`/`modules` are x86-only services the shared runlevel table still enables here (no
        `/dev/rtc`, no `/lib/modules`) - **closed by §13.15**; and the install writes no
        `/etc/hostname`, so the prompt says `localhost`, which still stands. (Its third named gap -
        `syslog` unable to start - was **not** a filesystem fact at all and is fixed by 13.13: the kernel's
        `open(2)` flag numbers were x86_64's, so arm64's `O_LARGEFILE` was read as `O_NOFOLLOW`.)
  - [x] 13.13 `open(2)`'s flag **numbers** are the machine's: arm64's `asm/fcntl.h`
        overrides `O_DIRECTORY`/`O_NOFOLLOW`/`O_DIRECT`/`O_LARGEFILE` before including
        the generic header, so the kernel's one table - x86_64's - read a *different
        flag from the one the caller set* on aarch64. The overlap is not academic:
        arm64's `O_LARGEFILE` (`0x20000`) **is** x86_64's `O_NOFOLLOW`, and musl's
        `open()` adds `O_LARGEFILE` to every open, so **every `open()` of a symbolic
        link on aarch64 answered `ELOOP`**. Found by §13.12's boot: `openrc` could not
        start its syslog service - `Error loading shared library libcap.so.2: Symbolic
        link loop`, the dynamic loader opening the versioned symlink
        `libcap.so.2 -> libcap.so.2.78`. Diagnosed by instrumenting the dispatcher to
        print the number and arguments of any syscall answering `ELOOP` (there is
        exactly one producer, the `O_NOFOLLOW` branch): the log gave
        `syscall 56 args [AT_FDCWD, ptr, 655360, 0, 0, 0]`, i.e. `openat` with
        `O_CLOEXEC|0x20000`, and the aarch64 `musl-dev` header named `0x20000` as
        `O_LARGEFILE`. `O_DIRECTORY` and `O_NOFOLLOW` are now per-machine (and
        `O_DIRECT`, `O_LARGEFILE` documented as the other two arm64 overrides;
        `O_LARGEFILE` stays undefined because the kernel ignores it, which is what
        matters). Pinned by `user/tests/src/bin/t-open-flags.rs` on **both** machines -
        it drives `libc`'s own constants through this kernel and fails on three checks
        on the old table (`open(symlink, O_RDONLY|O_CLOEXEC)` is `ELOOP` instead of
        opening, `open(file, O_DIRECTORY)` succeeds instead of `ENOTDIR`, and
        `O_TMPFILE`'s pair is refused with `EINVAL` instead of being read as the pair)
  - [x] 13.14 a boot check was leaving the wall clock at the year 36812: `check_wallclock`
        (the `M6_WALL_OK` check, which runs on **every** aarch64 boot - not only under
        `cargo xtask boot-aarch64`, because this machine's boot runs its stage checks as
        the boot's own verdict, in front of the sweep or a distribution's `init`) proves
        `set_boot_epoch` moves the anchor by setting it to `1 << 40` and **never put it
        back**, so every program after the check read a `clock_gettime(CLOCK_REALTIME)`
        seconds value of 1099511627776 - `36812-02-20`. That is what killed `syslogd`:
        musl's `asctime_r` reaches `a_crash()` when the timestamp it formats does not fit
        its 26-byte buffer, and "36812-02-20 00:36:16" is 26 characters. Fixed by
        restoring the anchor (and checking the restore), with the reason written into the
        check: a check must leave the machine as it found it. Pinned by
        `user/tests/src/bin/t-localtime.rs` on **both** machines - the first test either
        suite has had for `localtime`/`asctime` - which reads the anchor back as
        `CLOCK_REALTIME - CLOCK_MONOTONIC`, ranges every `struct tm` field, and fails on
        the old kernel at `the year is a live one (got 36812)` before dying in
        `asctime_r` exactly as `syslogd` did. Measured after: aarch64 sweep
        `pass=23 fail=0`, x86 gate `pass=153 fail=0 (153 built)`, `alpine-aarch64` exit 0
        with `syslog` **running and logging** (`/var/log/messages` carries `syslogd
        started: BusyBox v1.38.0` and every `init:`/`crond` line after it, written
        through the daemon), `boot-aarch64` green, host 512, depgraph 9/9, fmt clean,
        clippy 0 errors
  - [x] 13.15 the second machine's `hwclock` and `modules` - §13.12's last two named services:
        the RTC device node (`/dev/rtc0`/`/dev/rtc`, the `RTC_*` ioctls, and the module
        index the `modules` service asks `modprobe` about) was x86_64's by a stale gate,
        and the PL031 underneath it was wrongly believed to be a read-only counter. Both
        closed; the Alpine boot's runlevel output is now `[ ok ]` on every line, with no
        `[ !! ]` and no `modprobe` warning at all. Pinned by
        `user/tests/src/bin/t-rtc.rs` on **both** machines. Measured after: aarch64 sweep
        `pass=24 fail=0`, x86 gate `pass=154 fail=0 (154 built)`, `alpine-aarch64` exit 0
        with `Setting system clock using the hardware clock [UTC] ... [ ok ]` and
        `Loading modules ... [ ok ]`, `boot-aarch64` green, host 512, xtask 55,
        depgraph 9/9, fmt clean, clippy 0 errors

**Architecture debt (Phase 11 — audit done, Steps A/B/C/D/E/F/G landed, HAL S1 landed (§11.4), V-1/V-4/V-13 closed)**
  - [x] 13.16 the second machine's virtual consoles (`tty1`..`tty4` - the four the shared
        `/etc/inittab` starts, of the six `term::vc::VC_COUNT` mints) - the `vcmux` port, and the
        assembler bug it was blocked on.
        **The port.** Measured before anything was written: un-gating `vfs::vcmux` costs exactly
        **7** errors and all seven are `drivers::{video::render,drm}` - seven references in all: four
        `render::notify` calls, `drm::master_held`, and the `revoke_now`/`grant_and_resent` pair - so
        the port is that seam and nothing else.
        Written: the display half moved to four `crate::console` seams (`display_notify`,
        `display_master_held`, `display_handover`, `display_card_grabbed`), each with the
        `#[cfg(mpos_framebuffer_console)]` / `#[cfg(not(..))]` pair whose no-machine body is a no-op
        or the answer a machine with no card owes - which is the pattern `console`'s
        `paint_grids_if_live` already used, and the mux keeps the headless case it always had
        (`grids_live` narrates a switch on the wire as `[ttyN]`); the `'V'`/`'K'` ioctl arms and
        `Tty::{vt,kd}_ioctl` un-gated to `target_os = "none"`; `pl011::drain_input` calls
        `vcmux::route_input` instead of the console tty (the 16550's handler's exact shape); and
        `vcmux::init()` goes up before the console interrupt is armed, the order x86_64's boot uses.
        Both kernels and the loader build; fmt clean. Pinned by `user/tests/src/bin/t-vt.rs` on both
        machines: the ioctl words are read out of the shipped headers, the mode and keyboard-mode
        families are round trips over `/dev/tty1`, the `VT_*` family runs over `/dev/tty0` (a serial
        tty answers `ENOTTY` to it, which is what the harness hands the program), the switch is
        driven with `VT_ACTIVATE`/`VT_WAITACTIVE` and *seen* on the wire, and the test leaves the
        machine on the console it found (`VT_GETSTATE` is printed rather than asserted on - see
        13.18).
        **The blocker, and what it actually was.** With the port in, the aarch64 stage-2 context
        check died at the same address on every run - `EXCEPTION: synchronous`,
        `ESR_EL1 0x8600000f` (EC 0x21, an instruction abort at the *same* EL),
        `ELR_EL1 = FAR_EL1 = 0xffffffff80110380`, deterministic over three boots. That address is
        `KERNEL_VIRT_BASE + mpos_context_round_trip`, and the linker's own map (from
        `-C link-arg=-Map=`) says why it was not executable: the `global_asm!` block that defines the
        EL0 test program (`arch/aarch64/mod.rs`) opens `.section .rodata.el0_program, "a"` and never
        switches back, and every `global_asm!` in a crate is emitted into *one* assembly unit - so
        the three code blocks in `arch/aarch64/context.rs`, which name no section at all, were
        assembled into that data section. The map's input section `.rodata.el0_program` is `0x284`
        bytes and holds `__aarch64_el0_program` and then `mpos_enter_frame`,
        `mpos_context_round_trip`, `mpos_context_switch` and `mpos_task_trampoline`: the program is
        `0x28` of them and the routines the other `0x25c`. `.rodata` is mapped read-only (the W^X
        plan), so the first **call** to one was an instruction abort.
        So the earlier reading of this crash was right about the bytes and wrong about the cause. The
        fault address *did* hold a function's prologue (`stp x29, x30, [sp, #-128]!` is
        `mpos_context_round_trip`'s first instruction) - that is exactly what a call into a
        non-executable page looks like: the routine itself, not a return address overwritten by a
        stolen stack (the boot-stack concern recorded as §13.17 is not a bug - the kernel already
        refuses that region, and 13.17 says so). And the trigger is not the port's
        code either: nothing the port adds runs before stage 2, and what decides the placement is
        which blocks share an assembly unit, which the port perturbed only by changing how much code
        there is to partition. At HEAD the same writing happened to land in `.text`, which is what
        made this latent rather than already broken - the search for a "size" threshold was looking
        for the wrong variable.
        **Fixed** in `arch/aarch64/{mod,context}.rs` - the data block enters its section with
        `.pushsection` and leaves it with `.popsection`, so it cannot move anything assembled after
        it, and every code block names `.section .text.context, "ax"` so its placement cannot depend
        on emission order at all - and **pinned in `arch/aarch64/linker.ld`**, which now asserts each
        of the four asm code symbols is inside `__text_start..__text_end`: measured, the assertion
        fails the *link* against the old sources and names all four, which is where the mistake was
        made rather than two layers later at boot. Measured after: `boot-aarch64` exit 0
        (`ABABABABABABAM2_CONTEXT_OK: 13 hand-overs`, chain through `INIT_EXITED`); the aarch64 sweep
        `pass=25 fail=0` exit 0 with `t-vt` staged and passing (was 24); the x86 gate
        `pass=155 fail=0 (155 built)` (was 154 - `t-vt` runs on both machines); `alpine-aarch64`
        exit 0 on `localhost login:`, its getty reporting `Kernel 0.1.0 on aarch64 (/dev/tty1)` and
        no `[ !! ]`; host 512; xtask 55; depgraph 9/9 within the ratchet (`vfs/mod.rs -> drivers|net`
        fell to 0, which the ratchet allows); fmt clean; clippy 0 errors on `x86_64-unknown-none`,
        `aarch64-unknown-none` and the aarch64 loader.
        **The general rule this bug leaves behind** - three defences, all now in place, for any
        assembly that switches section: enter a section with `.pushsection` and leave it with
        `.popsection`; name the section in every *code* block (`.section .text.context, "ax"`) so
        placement cannot depend on emission order at all; and let `arch/aarch64/linker.ld`'s
        `ASSERT`s catch it - they fail the link and name the symbol, which is where the mistake was
        made rather than two layers later at boot. A bug that appears "when the image grows" is
        often this one: what changed is which blocks share an assembly unit, not the size. The
        symptom class is a faulting fetch landing on a function prologue whose address reads as
        "code in `.rodata`" - which is exactly what it was.
  - [x] 13.17 **not a bug**: the kernel does not hand back its own boot stack, and the record was
        never offering it. Found on the way as "the loader offers the kernel the stack it is running
        on", and the first half of that is true - the record's regions *are* the frame allocator's
        zones, and the 64-page boot stack does sit inside one of them. What was missed is the *kind*,
        and `MemKind::is_allocatable` answers it in its own doc comment: only `Usable` is
        allocatable, and `BootloaderReclaimable` - the kind the stack's region has, `mpos-boot`
        allocating it as UEFI `LOADER_DATA` - is excluded *because* it holds the stack the kernel is
        still running on. Measured by printing the record at boot: `stack 0x44f07000 in region
        0x44d9d000..0x456d1000 kind BootloaderReclaimable allocatable false`, and `pmm: 5 zones`
        built out of the record's five `Usable` regions.
        **The tell was in the work itself.** The "24 MiB discrepancy" that held the fix back is not a
        discrepancy: the carve changes nothing the allocator sees. With it and without it,
        `test --arch aarch64` reports `181 MiB` / `46345 frames` and `boot-aarch64` reports `205 MiB`
        / `52575 frames` - the only movement is the record's region count, `16` against `14`, which is
        the three-way split and nothing else. The two *totals* are two different boots: the sweep's
        initramfs is `12890 KiB` against `boot-aarch64`'s `420 KiB`, and the loader pays for it out of
        the same `Usable` regions. A number that moves for a reason nobody can name is evidence that
        the change does nothing, not noise to file away - and here it was the only signal that said
        so, because the change was otherwise plausible and its stated premise was half right.
        **What lands instead** is the invariant the whole thing was about, asked where the record is
        read: `boot::handoff::collect` refuses a record that lists the boot stack as allocatable
        memory, by name and with the address. Measured against the counterfactual of admitting
        `BootloaderReclaimable` to `is_allocatable`: `KERNEL PANIC: the handoff offers the boot stack
        at 0x44eef000 as allocatable memory: the kernel would hand back the stack it is running on` -
        so the check is not vacuous, it is what stands between the kernel and a loader that describes
        the stack as `Usable`. The loader is left alone: carving a `Reserved` hole in a region the
        allocator never touches buys nothing. The two WIP stashes that hold that carve are therefore
        dead - their other half is the landed port, and the carve is the correction above - so
        `git stash drop` on both is safe whenever someone wants the list tidy.
  - [ ] 13.18 `VT_GETSTATE`'s layout is not Linux's. Linux's `struct vt_stat` is
        `{ v_active, v_signal, v_state }`; this kernel writes `{ v_mode, v_num, v_state }` - the
        *video mode* where Linux has the console that is on the screen, and the console number where
        Linux leaves `v_signal` alone (its use is `SIGIO` delivery, which this kernel does not do);
        the third short is Linux's, `1 << (console - 1)`. Found while writing `t-vt`, which therefore
        prints the three shorts and asserts only that the call succeeds - a test that asserted this
        kernel's answer would be pinning the divergence. `user/abitest/src/bin/integration/t-seat.rs`
        *does* pin it (`stat_shorts(&st, 0)` is asserted to be the graphics mode after `KDSETMODE`),
        so a fix moves that test with it. Small, and written down rather than quietly fixed because
        the shape is published ABI: a seat manager reading `v_active` from a kernel that answers with
        a mode has a bug with no symptom until something switches consoles.
  - [x] 13.21 the neutral checks become **one copy**, and the second machine's clock is measured against
        them. Phase 15's first stages, landed here because each found a *machine* fact rather than a
        refactor.
        **What moved.** `check_mm`, `check_sched` (with its five scenarios and the run record),
        `check_vfs`, `check_ipc` and `ensure_child` left the x86_64 body for
        `src/board/checks/{mm,sched,vfs,ipc}.rs`, called by both bodies. The dispatch helper they share
        (`linkat`-shaped: one entry into `syscall::linux_dispatch` from kernel context) lost its
        `#[cfg(target_arch = "x86_64")]` with the move - another stale gate: it was written when only
        the first machine had a table to dispatch into. The init seam became **one signature**,
        `fn(&BootInfo) -> !`: the aarch64 machine layer no longer copies the archive's pointer and
        length into two statics (`INITRAMFS_PTR`/`INITRAMFS_LEN` are gone) but stashes the address of
        `boot`'s own `BootInfo` local - the same value the first machine's body is handed, whose
        slices point into the direct map and whose frame never returns - so both machines read
        `info.initramfs`. A misattached doc comment (the x86_64 handoff's prose sitting above
        `check_mm`) went back onto `handoff_to_init`. Measured: x86 gate `pass=157 fail=0 (157 built)`
        exit 33, aarch64 sweep `pass=27 fail=0`, host 512, both bare-metal clippy 0 errors, depgraph
        8/8 within the ratchet, fmt clean - the x86_64 numbers *identical* to the run before the
        move, which is what makes it a move.
        **What adopting them on aarch64 cost, and what it found.** `checks::mm` and `checks::sched`
        run green there (`M0_OK`, `M2_OK`, `M4_OK`, and the `pmm`/`hhdm`/slab/alternation lines), and
        the second of them found a defect the machine had carried since its scheduler landed: **the
        clock never preempted.** x86_64's timer handler does three things - `tick`, `charge_cpu`,
        `preempt`, with EOI between the last two - and the aarch64 IRQ path did only the first, so a
        task that never yielded kept the core for ever. Wiring the twin the same way made
        `checks::sched`'s `preempted` scenario pass ("a task that never yielded was preempted for 10
        ticks"), and then killed every EL0 program on the machine: the sweep's first test took a
        level-3 translation fault at `0x221af0`, and the verdict's `/musl` helper never printed its
        line. **That second failure is the real find.** A trap from EL0 leaves the program's resume
        state in `SP_EL0`/`ELR_EL1`/`SPSR_EL1` - *system registers*, not on the stack - and the EL0
        **interrupt** stub, unlike the sync one, saved none of them: it saved x0..x18/x29/x30 and the
        FP state and returned. That is sound only while an IRQ always returns to the context it
        interrupted, which is what it was; the moment the clock can preempt, the task is switched
        out, another task's traps overwrite those three registers, and this task's `eret` resumes
        into whatever they hold - i.e. into another address space's idea of a program counter. Fixed
        in the stub (three words saved after the FP block and written back before the `eret`, with
        the same "scratch registers before the register file" ordering the FP state needs, `#736`
        frame, and `TRAP_IRQ_STATUS_OFFSET` asserted beside the other layout constants). With it in,
        the sweep is `pass=27 fail=0` with preemption live, and the verdict's `/musl` helper runs.
        **What preemption then cost the verdict, and the second fix.** `M6_INIT_OK` was printed after
        the handoff, under the words "it has not run yet" - true only while nothing could preempt the
        boot task on the way. With the clock live, a tick between the spawn and the print lets a
        short init run to completion, and pid 1 leaving prints `INIT_EXITED` and **halts** the
        machine: measured, the marker the verdict polls for was simply absent while the init had
        clearly run. The spawn and the two lines now happen inside one interrupt-masked window, and
        the lines go out through the machine's *polled* writer because `println!`'s queue can park a
        writer and parking with interrupts off is a deadlock - the same rule the failure reports
        beside it already followed.
        **`checks::vfs`/`checks::ipc` on aarch64: shipped, once the stub below was fixed.** Both
        reach their own markers on this machine (`M6_OK`, `M7_OK`, `M8_IPC_OK` - the syscall gate,
        the ramfs root, a mounted tree, two tasks on one fd, the device set, pipes, the tty line
        discipline and signals), and `cargo xtask boot-aarch64` is green with them wired. The
        **verdict's** boot was not: it took an instruction abort at EL1 on the next preemption after
        them - `EC 0x21`, `ELR_EL1 == FAR_EL1` at a direct-mapped address (`0xffff800043e4f000`),
        which is where the inference in this paragraph went wrong. It read "the machine resumes into
        a page-table frame", and the sweep's address (`0xffff80004309c000`) was one; the verdict's is
        the *console's 559-byte transmit buffer*, a heap allocation. What the fault actually was is
        §13.22: the kernel's own interrupt stub did not keep the resume state of the context it
        preempted, so the task the clock took the core from resumed at whatever the last trap on the
        core had left in `ELR_EL1`/`SPSR_EL1`. Both families therefore went back into the aarch64
        body after that fix, which is why this entry's "measured after" lists their markers - and the
        payload-shape correlation (the sweep's 34 initramfs members against the verdict's 3) is a
        statement about *where the buffer landed and which task was preempted*, not about the checks'
        assertions.
        Measured after: `boot-aarch64` exit 0 through the full marker chain - the machine's own
        `M4_CONSOLE_OK` … `M6_INIT_OK`, `INIT_EXITED` and the init program's `MPOS_*` lines, plus the
        shared `M0_OK`, `M2_OK`, `M4_OK`, `M6_OK`, `M7_OK`, `M8_IPC_OK`; aarch64 sweep
        `pass=27 fail=0` exit 0; x86 gate `pass=157 fail=0 (157 built)` exit 0; host 512; depgraph
        8/8 within the ratchet; fmt clean; clippy 0 errors on both bare-metal targets.
  - [x] 13.22 the kernel's own interrupt stub keeps the resume state it interrupted - the EL1 half of
        §13.21's EL0 finding, and the abort that held two check families out.
        **What was seen.** `EXCEPTION: synchronous`, `ESR_EL1 0x000000008600000f` (EC 0x21, an
        instruction abort at the same EL), `ELR_EL1 == FAR_EL1`, at a page-aligned direct-map address
        just after `check_serial_tx` had put its 512-byte line on the wire:
        `0xffff800043e4f000` on the verdict's boot, `0xffff80004309c000` on the sweep's. It needed the
        two shared check families wired (§13.21) to appear at all, which is what made it read like a
        bug *in* them.
        **How it was localized.** Three measurements, none of them a guess:
        1. *QEMU's exception log* (`-d int`) showed the last two `eret`s before the abort both
           returning to `0xffffffff80108048` - the task trampoline's `mov x0, x19`, one instruction
           after it enables interrupts - with no exception in between. A `ret`/`br` into a data page
           would have shown as a fetch with no eret before it; an `eret` to a stale address shows
           exactly this.
        2. *The registers at the abort*, caught at its vector entry under `gdb` (`hbreak` on the EL1
           sync vector, `-S` so the breakpoint is armed before the kernel image exists): `x19 = 0x22f`
           (559), `x20 = 0xffff800043e4f000` (the console's transmit buffer), `x30 =
           mpos_task_trampoline + 16`. The CPU was executing the trampoline's `blr x20` with a *copy
           loop's* scratch registers - i.e. a context that had no business being in the trampoline at
           all.
        3. *The stack*, read upward from `SP_EL1` while the machine sat in the halt loop: the EL1 sync
           stub's 160-byte frame and its `bl sync` return address are where the faulting context's
           `x0..x18`/`x30` are, which is how the interrupted register file above was attributed to the
           boot task inside `check_serial_tx` (its `x1`/`x2`/`x10` all point into the 559-byte buffer).
        **The mechanism.** An exception from EL1 does not put the interrupted `PC` and `PSTATE` on the
        interrupted stack - the CPU writes them to `ELR_EL1`/`SPSR_EL1`, one pair per core - and the
        EL1 IRQ stub called `irq`, which ends in `preempt`, which *switches tasks*. Saving `x0..x18`,
        `x29` and `x30` is the call ABI's answer and it is not enough: the incoming task's own traps
        overwrite the pair, so a preempted kernel context's `eret` returned to whatever the last trap
        on the core had recorded. Measured consequence: the boot task, preempted mid-`check_serial_tx`,
        resumed into the task trampoline that another task's dispatch had left in `ELR_EL1`, ran its
        `blr x20` with the copy loop's `x20`, and fetched instructions from a heap buffer. §13.21 fixed
        the EL0 twin of this (three words: `SP_EL0`, `ELR_EL1`, `SPSR_EL1`) and the EL0 stub's own
        comment already argued the rule; the EL1 stub needed two words of the same shape and kept
        none.
        **The fix.** `arch/aarch64/vectors.rs`'s `__vec_irq` saves `ELR_EL1`/`SPSR_EL1` at
        `TRAP_EL1_IRQ_STATUS_OFFSET` (176, after the 176 bytes of registers it already saved; frame
        192, asserted beside the other layout constants) and writes them back through `x9`/`x10`
        before the register restore, which is the only window they can be used in. The EL1 **sync**
        stub deliberately still saves none, and it now says why: nothing on that path switches tasks
        (two resolvers that allocate and copy), and a resolver taught to block would need the pair.
        **Pins.** The boot-level counterfactual, measured: with the two families wired and this stub
        unfixed, `boot-aarch64` fails 3 runs out of 3 at `check_serial_tx` with the abort above; with
        the fix it is exit 0 end to end - and `xtask`'s verdict now *requires* `M6_OK`, `M7_OK` and
        `M8_IPC_OK` in the chain, so the wiring that reaches the preempting path cannot be dropped
        silently. What is *not* pinned is the stub itself: the natural clobber is a valid kernel text
        address (another task's own interrupted `PC`), so a non-fatal corruption is invisible, and a
        sharp pin would need an instrument that writes `ELR_EL1` - a machine register no kernel
        operation writes - which `AGENTS.md` §3 rule 4 does not buy. That gap is the reason the
        measurement above is written down with its addresses rather than summarized.
        Measured after: `boot-aarch64` exit 0 with the full chain plus `M6_OK`/`M7_OK`/`M8_IPC_OK`;
        aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 on `localhost login:`
        (the machine's own userland, checks off); x86 gate `pass=157 fail=0 (157 built)` exit 33; host
        512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none`,
        `aarch64-unknown-none`, the aarch64 loader and xtask.
  - [x] 13.23 the boot's reporting, its exit hooks and its log sink become one copy each - Phase 15's
        stages 15.4 and 15.5, and the first capability gates the input stack's own switch.
        **What moved, and what was measured before it.** Five functions that were x86_64's alone:
        `banner`, `check_boot_stack`, `park` (`hal::halt` on the second machine instead),
        `report_stack_use` and `panic_context` (an empty `fn {}` on the second machine). Every line in
        them is a function of `BootInfo`, the scheduler, or a register read, so they are now
        `src/board/report.rs`, called by both bodies; the one machine question left in them -
        "which register holds the stack pointer" - became `hal::Cpu::stack_pointer` (`rsp` / `sp`), so
        the walk itself is not machine code. The panic walk also stopped carrying a *base constant*:
        it recognizes return addresses inside `arch::kernel_sections().text`, which is the exact
        answer rather than x86_64's `0xffff_ffff_8000_0000` floor, and one a port cannot get wrong.
        The boot-stack canary was spelled **twice** - once in `mpos-boot`, once in
        `board/x86_64.rs` - each with a comment claiming there was "no crate between" the two sides;
        the leaf both already import (`crates/bootinfo`, which carries `HANDOFF_MAGIC`) has it now, so
        the two ends cannot drift into a canary that reports a misspelling as an overflow. The second
        machine gains the check itself: measured on the verdict's boot, `boot stack: intact after the
        self-tests` and `boot stack: intact under hand_to_init` - the same two checkpoints, reading
        the same word the loader wrote through the same `boot_stack_base` field.
        **One exit-hook registry.** `steps::register_exit_hooks` is now the one list, in the one order
        (locks, then the state keyed by the open file description: socket, inotify watch, evdev node,
        uinput device), with the two input hooks under `mpos_input_registry`. Before it, x86_64
        registered five hooks plus two `FD_CLOEXEC` ones and aarch64 one - the socket's - so the
        second machine released a dying process's record locks and inotify watches never, and a
        `FD_CLOEXEC` evdev position could not exist there to be told about. The *stack's* gates moved
        with it: `drivers::virtio::input`, `drivers::uinput` and the `/dev/input` + `/dev/uinput`
        block in `drivers/devices.rs` were `#[cfg(target_arch = "x86_64")]` and are now
        `#[cfg(mpos_input_registry)]` - the capability the boot, sysfs' `build_input` and these hooks
        all ask, so a port that lands the input stack flips one switch instead of editing four files.
        **The net bring-up.** `net::init` + `net::spawn_service` + the "up at 10.0.2.15/24 …
        MAC" line were written in both bodies; they are `steps::bring_up_net(dev)` now, and each
        machine keeps only "find the adapter" (PCI bus vs device-tree MMIO window) and its own honest
        line when there is none. x86_64's line had no MAC and aarch64's did; the shared one prints it
        on both. The `mpos.selftest` question was the same story at one line's scale - a closure in
        one body, an `.any(..)` in the other - and it is `boot::cmdline_has(word)` now, next to
        `boot::cmdline` whose grammar it is: two boots that read the word differently are two boots
        that run different boot sequences for one command line.
        **The log sink, and a gate whose premise had expired.** `src/logging.rs`'s subscriber - the
        whole `tracing` sink - was `#[cfg(all(target_os = "none", target_arch = "x86_64"))]` with the
        reason written beside it: "aarch64 has neither [console nor clock] until its console (stage 4)
        and clock (stage 1c) land". Both landed stages ago and the gate stayed, so the second machine
        had **no sink at all** and every `tracing` line in shared code was silently dropped there. It
        is `#[cfg(target_os = "none")]` now - one subscriber, one level vocabulary, both machines -
        and the honest measurement is that this buys parity rather than output: the kernel has **0**
        `tracing`/`log` call sites on *either* machine, so nothing emits into the sink yet. The
        aarch64 kernel grows by it (1817 → 1843 KiB), and "the kernel's logging story is
        unbuilt" is the gap that leaves, named here rather than papered over by the flip.
        Measured after: `boot-aarch64` exit 0 with the full chain, the two `boot stack: intact` lines
        and the banner (`hhdm offset`, `memory map (16 regions)`, `allocatable: 205 MiB`) on the
        second machine's wire; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at
        `mpos login:` (its own userland, checks off, banner and canary check included); x86 gate
        `pass=157 fail=0 (157 built)` exit 33 with host 512 - and the x86_64 boot still printing
        `boot stack: intact after the self-tests` / `under spawn_init` from the shared function; both
        bare-metal targets and the aarch64 loader build; depgraph 8/8 within the ratchet; fmt clean;
        clippy 0 errors on `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and
        xtask.
  - [x] 13.24 the second machine's input stack is the **same registry**, and the port's flip is one
        error - Phase 15's stage 15.2, first class (the input registry).
        **Measured before the seam was designed** (§13.4's rule: measure the flip before designing the
        seam). Declaring `mpos_input_registry` for `[target.aarch64-unknown-none]` and building costs
        exactly **one** error: `E0432: unresolved import super::hal::transport_all` at
        `drivers/virtio/input.rs:40`, whose item is `#[cfg(target_arch = "x86_64")]` in
        `drivers/virtio/hal.rs`. The plan predicted **four** (three more in `vfs::sysfs::build_input`);
        those are gone because §13.23 flipped that block, the devfs block and the exit hooks from
        `target_arch` to the capability - so the flip is one word in `.cargo/config.toml` and one
        import, which is what the capability was introduced to mean. Everything else the second
        machine gains by declaring it (`drivers::uinput`, `/dev/input` + `/dev/uinput`,
        `/sys/class/input`) already compiles the moment the word is there.
        **The contract.** `drivers::virtio::InputDevice` is what the registry holds: the device's name,
        `ev_bits`, `abs_info`, `prop_bits` and `ids` - the capability picture a reader asks for before
        any event - and `pop_pending_event`. `VirtioInputDev<T: Transport>` is the one implementation
        of those six over the crate's `VirtIOInput`, so a second transport supplies a `T` and no second
        copy of them; `input.rs`'s registry, `read_caps` and `service` traded
        `VirtIOInput<HalImpl, PciTransport>` for `Arc<dyn InputDevice>`. The one door in is
        `input::register(devices)`, which takes the contract. `input::init()` - the PCI enumeration -
        remains the first machine's way in and is `#[cfg(target_arch = "x86_64")]`;
        `mmio::input_device(virt, size)` is the second's.
        **The device stays open.** `mmio::probe_input` opened the window, negotiated, read the device's
        configuration-space identity and dropped it - a proof that the transport reached the class,
        and nothing anyone could read. It is `mmio::input_device` now: the live handle goes to the
        registry, and from there it *is* the machine's keyboard. `check_input` (still in `devices()`,
        where the device tree is read and before there is a `/dev`) opens it and registers it, and the
        aarch64 boot's wire carries `virtio-input: event0: QEMU Virtio Keyboard`.
        **The marker moved, and why.** `M8_INPUT_OK` used to be printed by the transport probe. The pin
        this class can carry is the registry's own - event flow needs a keystroke and the harness is
        headless - and reading the registry *through a path* needs a mounted `/dev`, which exists only
        after `check_dev`. So the class's marker is `check_input_registry()`'s now, and it asserts three
        things: the registry is not empty, `/dev/input/event0` resolves through the mounted `/dev`, and
        the node reports `rdev() == (13, 64)` - evdev's own pair, the one udev keys its database by and
        libinput finds the device through, which a node reporting `0,0` fails on a machine that looks
        healthy everywhere else. `M8_INPUT_FAILED` is unchanged and still the terminator, used by both
        failure points (a window that will not open, a registry that reads back wrong).
        **The pump.** The task that harvests the device rings was wholly x86_64's (`spawn_service`),
        because it also drove `net::vsock` and the GPU's `VIRTIO_GPU_EVENT_DISPLAY`. It is gated on
        `mpos_input_registry` now, with those two halves `#[cfg(target_arch = "x86_64")]` where their
        drivers live - so both machines run one shared task's input half and the second machine does not
        grow a second copy of the loop. The aarch64 body starts it beside `init_net`, which is where a
        scheduler exists for it to park in.
        Measured after: `boot-aarch64` exit 0 with
        `M8_INPUT_OK: the evdev registry holds 1 device(s), /dev/input/event0 resolves as (13, 64)`,
        no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at `mpos
        login:` carrying the same two input lines (its checks run on every boot); x86 gate
        `pass=157 fail=0 (157 built)` exit 33 with host 512; depgraph 8/8 within the ratchet; fmt
        clean; clippy 0 errors on `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader
        and xtask.
  - [x] 13.25 the second machine's host-driven console is the same `/dev/hvc0` - Phase 15's stage
        15.2, second class (virtio-console).
        **Measured before the seam was designed** (§13.4's rule). Un-gating
        `drivers/virtio/console.rs` for aarch64 costs **one** error: `E0432: unresolved import
        super::hal::transport`, the PCI probe helper the module's `init` reaches for. Nothing else in
        it is x86_64's - the `CharDevice` face, the blocking read and the write-through are pure
        policy over the crate's driver. The device side was measured too, because a class nobody can
        attach is not a class: `qemu-system-aarch64 -device help` answers `virtio-serial-device` (bus
        `virtio-bus`, i.e. the MMIO transport) and `virtconsole`, so the second machine can carry the
        same shape the first machine attaches on PCI (`virtio-serial-pci` + `virtconsole`).
        **The contract.** `drivers::virtio::ConsolePort` is what the kernel holds: `size`, `drain`,
        `send` - the three things `/dev/hvc0`'s stream face and the boot's own probe ask. The crate
        supports one port and fixes its queues at 0 and 1, so `virtio-serial-device` carrying a single
        `virtconsole` is exactly its shape. `VirtioHvc<T: Transport>` is the one implementation of
        the three over the one crate driver - so the second transport supplies a `T`, not a second
        copy - and it lives here rather than in `mod.rs` because this module is compiled on both
        machines, which `input` is not (it is capability-gated, and that is why its adapter is beside
        its contract). The global is `Arc<dyn ConsolePort>` now, `console::install(port)` is the one
        door, and `init_global()` - the PCI probe - is `#[cfg(target_arch = "x86_64")]`.
        **The machine.** `mmio::console_device(virt, size)` opens the window the device tree named as
        the same `VirtIOConsole` over `MmioTransport`. `check_console` (in `devices()`) finds the
        window whose `DeviceID` is `3`, opens it, installs it, and exercises the write face with a
        probe line the port's queue has to take - which claims nothing about a reader, because the far
        end is a host socket. `check_console_dev()` runs after `check_dev` and resolves both names the
        port answers to, `/dev/hvc0` and the `hvc` alias Linux keeps for "the first one", through the
        *mounted* `/dev`. The devfs gate went with the transport: the `#[cfg(target_arch = "x86_64")]`
        on the `hvc0` registration is deleted, so the node is published by whichever machine opened
        the port - absent, not faked, on a machine whose window never opened.
        **The marker.** `M8_CONSOLE_OK` is not a terminator and `M8_CONSOLE_FAILED` is;
        `xtask/src/aarch64.rs`'s `TERMINATORS` grew by that one line (24 → 25), and `aarch64_qemu`
        attaches the device: `-chardev socket,id=console0,path=build/aarch64-console.sock,server=on,wait=off`
        beside `-device virtio-serial-device -device virtconsole,chardev=console0`. The socket is the
        first machine's arrangement (`build/…/console.sock`): `wait=off` keeps a machine with no
        reader from blocking on the port it just opened.
        **Named, not fixed.** `/dev/hvc0`'s device number is `(0, 0)` on **both** machines, because
        `DevFs::register` gives a `CharDevice` no number - Linux's `hvc0` is `229:0`, and
        `register_id` is the door for a real one. The verdict therefore asserts the two names resolve
        and not a number, since there is no number to pin; fixing it would change the first machine's
        node too and belongs with whichever class next touches the tty numbering, not here.
        Measured after: `boot-aarch64` exit 0 with `virtio-console: up at /dev/hvc0` and
        `M8_CONSOLE_OK`, no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit
        0 at `mpos login:` carrying the same two console lines; x86 gate `pass=157 fail=0 (157 built)`
        exit 33 with host 512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask.
  - [x] 13.26 the second machine's display is the **same card and the same render task** - Phase 15's
        stage 15.2, third class (virtio-gpu + framebuffer console), and the one class whose seam was
        not one helper.
        **Measured before the seam was designed** (§13.4's rule), and the measurement was two-sided
        in a way the first two classes were not. Un-gating `virtio/gpu.rs` + `gpu_ctrl.rs` + `video/`
        + `drm/` and declaring `mpos_framebuffer_console` for aarch64 costs **3 errors**, all PCI-only
        hal helpers inside `gpu.rs`'s `init` (`transport`, `super::hal::device_config`,
        `super::hal::claim_drm_card`) - a number that reads like `net`'s and is not. The great majority
        of the stack needed nothing: **one** `pci::` reference in the whole 7.6 kLoC of `gpu.rs`,
        `gpu_ctrl.rs`, `drm.rs` (6680 lines) and `video/render.rs` (946), and `src/console.rs`'s
        framebuffer arm already had `#[cfg(not(mpos_framebuffer_console))]` no-ops on the other side.
        What the number hid is that **`virtio-drivers`' `Transport` is not object-safe** -
        `begin_init<F: Flags + BitAnd + Debug>` is a generic method and `VirtQueue::new`/`add_notify_wait_pop`
        take `&mut T where T: Transport` - so there is no `Box<dyn Transport>` shortcut and the type
        had to be erased somewhere else.
        **Where it was erased.** `gpu_ctrl.rs`'s `Gpu` became `Gpu<T: Transport>`, and the new
        `GpuDevice` trait names exactly the sixteen commands `video::render` sends (and nothing else:
        a method no caller has is a promise nothing keeps) with `impl<T: Transport + Send> GpuDevice
        for Gpu<T>` delegating to the inherent ones. `render.rs` - which owns the one device in a
        `static` and is the only context that may call `flush` - holds `Box<dyn GpuDevice>` and passes
        `&mut dyn GpuDevice` to its helpers, so the render task, the DRM/KMS layer above it and the
        framebuffer below it are one copy on both machines. That is the whole of the refactor: no
        command, no order and no default changed, which is what `t-fb` and `t-drm` in the x86_64
        sweep are the evidence for.
        **The machine's half.** `VirtioGpuDev::install(device, config)` is the one door - it takes the
        *device*, asks the host's resolution, builds the resource and pins the frames, so both
        machines share it. `gpu.rs::init_global()` stays the first machine's: it probes the PCI bus,
        records the function's device-configuration window (`super::hal::device_config`) and *claims*
        the function as the card's home for `sysfs` (`super::hal::claim_drm_card`). The second
        machine's is `mmio::gpu_device(virt, size)`: it opens the window the tree named, and its
        configuration window is one constant offset - `virt + 0x100`, the virtio-mmio device-specific
        space - where PCI has to walk capabilities to find the same bytes. `check_gpu` (in `init`,
        before `check_dev`) finds the window whose `DeviceID` is `16`, opens it, and spawns the render
        task; it stands there rather than in `devices()` because the task needs the scheduler
        `steps::start_scheduler` started, and before `check_dev` because that call is what publishes
        `/dev/fb0`. `check_gpu_dev()` runs after it and asserts three things: the card is installed,
        `/dev/fb0` resolves through the *mounted* `/dev`, and one row of pixels (`width * 4` bytes)
        reads back whole - a node whose length was a claim rather than a mapping short-reads there.
        It also asserts the render task is running, which is the part a class with no compositor
        could otherwise fake: `write(2)` to this node parks until that task has flushed, so a card
        installed without it is a node a writer blocks on for ever. A pixel *value* is deliberately
        not asserted - the buffer holds whatever the host last put there, and a boot has no way to
        say what that is; the write path is `t-fb`'s, and the sweep runs it on this machine.
        **The framebuffer console came with it.** `mpos_framebuffer_console` is declared for aarch64,
        so `src/console.rs`'s framebuffer arm is live there - the grids paint into the scanout and
        `grids_live()` still decides whether the kernel's log goes to the screen, so a boot whose
        console list names the wire alone is unaffected (which is why the verdict and Alpine boots
        read identically before and after).
        **Named, not fixed.** The second machine has no `/dev/dri/card0` and no `/sys/class/drm`: both
        hang off a *PCI function*, which only the first machine has, and `sysfs`'s `build_drm` finds
        its card through that mark. The card itself is the same device - `t-drm`'s subject - so this
        is a statement about the bus and the sysfs tree, not about the display. The DRM device node
        is the next class's problem if it is one at all.
        Measured after: `boot-aarch64` exit 0 with `virtio-gpu: 1280x800, 4000 KiB at /dev/fb0` (the
        host's own preferred mode, followed rather than assumed) and `M8_GPU_OK`, no `EXCEPTION`;
        aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at `mpos login:` carrying the
        same card lines; x86 gate `pass=157 fail=0 (157 built)` exit 33 with host 512 and `t-fb`/
        `t-drm` green; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts
        unchanged on both machines (aarch64 lib 47, x86_64 lib 1) - 7.6 kLoC of graphics code joined
        the second machine's build and added no lint.
  - [x] 13.27 the second machine's `/dev/dsp` is the same sound card - Phase 15's stage 15.2, fourth
        class (virtio-sound).
        **Measured before the seam was designed** (§13.4's rule). Un-gating
        `drivers/virtio/sound.rs` for aarch64 costs **one** error: `E0432: unresolved import
        super::hal::transport` - the same single helper `console` cost, and for the same reason (the
        PCI probe is the only machine-specific line in the module). The device side was measured too:
        `qemu-system-aarch64 -device help` answers `virtio-sound-device` (bus `virtio-bus`, the MMIO
        transport), and the audio backend the first machine already uses (`-audiodev none,id=snd0` - a
        backend that consumes the stream and plays silence) is QEMU's, not the machine's.
        **The contract.** `drivers::virtio::SoundCard` is the three methods the device face and the
        boot actually ask for: `format`, `configure`, `play`. `VirtioSnd<T: Transport>` is the one
        implementation, so the card's whole OSS face - the parameter ioctls, the rate rounding, the
        period-sized staging buffer, the blocking hand-off - is one copy, and a second transport
        supplies only a `T`. `sound::install` is the one door; `install_probed` holds the probe's
        absent-vs-refused distinction (a machine with no card is a quieter boot, a card that answered
        and would not play is worth a line); `init_global()` - the PCI probe - is
        `#[cfg(target_arch = "x86_64")]`. The `Probe` enum became `pub` because `VirtioSnd::new` now
        returns it, which is the one API change the split needed.
        **The machine and the pin.** `mmio::sound_device(virt, size)` opens the window the tree named
        and hands it to `install_probed`, so the stream listing, the rate intersection, the format
        check and the initial `SET_PARAMS`/`prepare`/`start` are the first machine's code running over
        the second machine's transport. `check_sound` (in `devices()`) finds the window whose
        `DeviceID` is `25` and prints the same `44100 Hz, 2 channels` line the first machine prints;
        `check_sound_dev()` runs after `check_dev` and asserts the card is installed, `/dev/dsp`
        resolves through the *mounted* `/dev`, and the stream takes a 512-byte burst in full. That
        last one is the data path - `pcm_xfer` through the crate, into the host's `none` backend - and
        not just the config space, which is the one thing a node that merely resolves could otherwise
        fake. The burst is silence on purpose: the backend plays nothing, and a machine whose backend
        did play would still be asked for silence rather than noise.
        **Named gap, in this class's own coverage.** The OSS *parameter* face
        (`SNDCTL_DSP_SPEED`/`CHANNELS`/`SETFMT` and their cousins) is exercised by `user/abitest`'s
        `t-dsp`, which the aarch64 sweep does not carry - and `check_sound_dev` deliberately does not
        move the stream's rate, because a boot check that rebuilt the running stream would be changing
        the machine it is checking (§13.14's rule). So this class's marker says the card is there and
        moves bytes; it does not say the ioctls work on this machine.
        **One flake, measured and not believed.** The first x86_64 gate run after this change reported
        `FAIL t-clock-gettime` on its monotonicity line ("two reads never go backwards": two successive
        `CLOCK_REALTIME` reads, where the wall clock is the CMOS second plus a tick-derived fraction
        sampled around it). Nothing in this class touches the clock, and the re-run is
        `pass=157 fail=0 (157 built)` exit 33 - so it is a wall-clock flake, recorded rather than
        fixed: it is neither this class's subject nor new (the same shape as the `rand` flake
        `AGENTS.md` §5 already names).
        Measured after: `boot-aarch64` exit 0 with `virtio-sound: 44100 Hz, 2 channels at /dev/dsp`
        and `M8_SOUND_OK`, no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64`
        exit 0 at `mpos login:` carrying the same card lines; x86 gate `pass=157 fail=0 (157 built)`
        exit 33 with host 512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts
        unchanged on both machines.
  - [x] 13.28 the second machine's `AF_VSOCK` is the same family over the same kind of device - Phase
        15's stage 15.2, fifth class (virtio-vsock), and the one class whose device the host cannot
        always provide.
        **Measured before the seam was designed** (§13.4's rule). Un-gating `drivers/virtio/vsock.rs`
        for aarch64 costs **one** error: `E0432: unresolved import super::hal::transport` - the same
        single helper `console` and `sound` each cost. And, as with the input registry, the number hid
        the design: `VirtioVsock` held `PciTransport` *inside* its own struct (the trap `AGENTS.md` §5
        names), so the module could not compile for a second machine until the type was erased. The
        device side was measured too: `qemu-system-aarch64 -device help` answers `vhost-vsock-device`
        (bus `virtio-bus`, the MMIO transport) beside the PCI form, and the crate's
        `DeviceType::Socket` is `19` - the `DeviceID` an MMIO window answers.
        **The contract.** `drivers::virtio::VsockDevice` was already there (it is what
        `src/net/vsock.rs` holds), so this class needed no new contract: `VirtioVsock` became
        `VirtioVsock<T: Transport>` - the crate's `VirtIOSocket` over whatever `Transport` a machine
        opened - and its ten methods, which the family's phases, ports and accept queue all sit on,
        are one implementation over the one crate driver. The PCI probe became `init_global()` (a free
        function answering `Arc<dyn VsockDevice>`, `#[cfg(target_arch = "x86_64")]`), the module lost
        its `#[cfg(target_arch = "x86_64")]`, and `net::vsock::init(dev)` is handed the contract
        rather than a bus, so nothing above `drivers` learns which machine it is on.
        **The machine and the pin.** `mmio::vsock_device(virt, size)` opens the window the tree named
        and hands the device to `net::vsock::init`; the board's `check_vsock` (in `devices()`) finds
        the window whose `DeviceID` is `19`, prints the same `virtio-vsock: guest cid N answers on
        AF_VSOCK` line the first machine prints, brings the family up and asserts it answers the
        device's own CID - the one claim `getsockname` rests on - then prints `M8_VSOCK_OK`. The pump
        was already shared: `virtio::spawn_service`'s vsock half lost its `#[cfg(target_arch =
        "x86_64")]` and is now the contract's, like the input half before it (a machine whose device
        never probed has no family, and `net::vsock::service` returns without touching one).
        **The class no host can always provide, and the loud skip.** vsock is the one class this phase
        has driven whose device QEMU does **not** emulate in userspace: `vhost-vsock-*` is backed by
        the host's vhost kernel module, and QEMU *refuses to start* a machine naming the device without
        `/dev/vhost-vsock`. Attaching it unconditionally would stop `boot-aarch64` booting on a bare
        host - the trade `machine.toml` already refuses for the first machine ("a boot that never
        starts is worse than a machine with no vsock"). So xtask attaches
        `vhost-vsock-device,guest-cid=4` only when `/dev/vhost-vsock` exists (CID `4`, not the first
        machine's `3`, so the two machines' boots can be up at once without colliding), requires
        `M8_VSOCK_OK` only then, and otherwise prints which case it took - "the host has no
        /dev/vhost-vsock, so this boot attaches no vsock device and M8_VSOCK_OK is not required" - and
        its one-line summary says "seven devices" rather than "eight". The skip is never silent: the
        kernel says the absence in its own voice too (`virtio-vsock: no device on the tree; AF_VSOCK
        answers EAFNOSUPPORT`). `M8_VSOCK_FAILED` (a window that exists and will not open) is a
        terminator, `M8_VSOCK_OK` is not (`TERMINATORS` 27 → 28).
        **A correction found in passing.** `virtio::spawn_service` also drove the GPU's
        `VIRTIO_GPU_EVENT_DISPLAY` poll under the same `#[cfg(target_arch = "x86_64")]`, and its
        comment still said the second machine had "no GPU driver yet" - false since §13.26. That gate
        went with the vsock one, so the shared task now polls everything both machines have, and the
        poll is honest with no card (`CONFIG` is 0, so it answers `false` and costs one read a tick).
        Measured after: `boot-aarch64` exit 0 with `virtio-vsock: guest cid 4 answers on AF_VSOCK` and
        `M8_VSOCK_OK`, no `EXCEPTION`; the skip path was measured too (the host check forced false for
        one run): exit 0, the kernel's absence line, xtask's skip line, and a summary claiming seven
        devices and no `M8_VSOCK_OK`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at
        `mpos login:` carrying the same vsock line; x86 gate `pass=157 fail=0 (157 built)` exit 33 with
        host 512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts
        unchanged on both machines (aarch64 lib 47, x86_64 lib 1).
  - [x] 13.29 the second machine's `/mnt/share` is the same host share - Phase 15's stage 15.2, sixth
        class (virtio-9p), and the one that cost three separate findings, two of them defects the
        first machine had carried all along.
        **Measured before the seam was designed** (§13.4's rule). Un-gating `drivers/virtio/fs9p.rs`
        *and* `vfs/p9.rs` for aarch64 costs **one** error: `E0432: unresolved import
        super::hal::transport_all` - the same single helper `console`/`sound`/`vsock` each cost. The
        design cost the number hid was the usual one: `Virtio9p` held `PciTransport` inside its own
        struct and `vfs/p9.rs` named `Arc<Virtio9p>`, so the type had to be erased again. The device
        side was measured too: `qemu-system-aarch64 -device help` answers `virtio-9p-device` (bus
        `virtio-bus`, the MMIO transport) beside the PCI form, and the crate's `DeviceType::_9P` is
        `9` - the `DeviceID` an MMIO window answers.
        **Finding 1: the probe had no caller.** `fs9p::init_global()` - the only function that fills
        the `DEVICES` registry - was called from nowhere in the tree. So `fs9p::devices()` was always
        empty, `mount_share()` printed `virtio-9p: no share on the bus` on **every** boot of **both**
        machines, and `/mnt/share` was never mounted - while QEMU exported `-virtfs
        ...,mount_tag=hostshare` on each of them. Measured in the logs (`build/serial.log` line 61,
        `build/test-serial.log` line 101) and by history (`git log -S 'fs9p::init_global'` never
        found the string, so the call never existed - the mount was written against a registry
        nothing filled). PLAN had the *symptom* on record - §6's DRM block already notes "the boot
        prints `virtio-9p: no share on the bus`, so there is no file channel into the guest" - as an
        instrument note, with no remediation. The driver, the client and the `mount -t 9p` dispatch
        were all complete and host-tested, and none of them had ever run.
        **Finding 2: the client speaks a dialect this host's QEMU refuses.** With the probe wired, the
        mount failed with `Unsupported`. Measured, in one boot, by asking for each dialect in turn:
        QEMU 11.1.1 answers `"unknown"` to a plain `9P2000` `Tversion`, and `9P2000.u` to a `9P2000.u`
        request (and `9P2000.L` to `.L`). So the plain dialect the client asked for is one no modern
        server serves - a second, independent reason the share was unreachable. The client now speaks
        **9P2000.u**: the version string, `n_uname` on `Tattach`, the `extension`/`n_uid`/`n_gid`/
        `n_muid` tail on `Twstat`'s `statu` record. Everything else on the wire is unchanged - walk,
        open, read, write, clunk, remove and `Ropen`/`Rcreate`'s `iounit` are the same in both
        dialects - and `parse_stat` reads only up to `name`, so `.u`'s tail is stepped over rather
        than parsed. `Rerror` still carries the prose `ename` the client maps by phrase; `.u`'s
        `ecode` is deliberately unused (the phrase mapping is what the host tests pin, and QEMU sends
        usable prose).
        **The seam.** `drivers::virtio::P9Device` is the two methods the client and the mount table
        ask for (`mount_tag`, `request`); `Virtio9p<T: Transport>` is the one implementation, so the
        fid table, the `Twalk`/`Tread` encoding and the `Inode` mapping above it are one copy whichever
        bus answered. `fs9p::install(devices)` is the one door; `init_global()` (the PCI probe) stays
        the first machine's; `mmio::p9_device(virt, size)` is the second's; `vfs/p9.rs` holds
        `Arc<dyn P9Device>`; and the `mount -t 9p` dispatch arm lost its `#[cfg(not(target_arch =
        "aarch64"))]`. `mount_share()` moved out of `board/x86_64.rs` into the shared
        `board/steps.rs`, and that move is what made the second machine's share possible at all: it
        was the *probe* that was missing there, not the mount, and a mount written twice would have
        hidden which.
        **The pins, one per machine's convention.** The second machine's is `check_p9` (in `devices()`
        - find every `DeviceID` 9 window, open it, `fs9p::install` the lot) plus `check_p9_dev()`
        after `switch_root()` - `/mnt/share` resolves and `mpos-9p-probe`, a file xtask writes into
        the exported directory (`utils::lay_p9_share`), reads back through the VFS: the bytes travel
        host -> 9P2000.u -> `vfs::p9` -> the mount table, so a share that merely mounted fails there.
        The first machine's is `user/abitest`'s `t-9p`: the boot's own `/mnt/share` serves the probe,
        a program's own `mount -t 9p hostshare /mnt/t-9p` reaches the same bytes over the syscall
        path, and an unexported tag answers `ENOENT`. It runs on the first machine only - the aarch64
        sweep stages the `std`-side suite, not `abitest` - and it is the program that fails, loudly,
        if the probe call is ever dropped again.
        **And the parser joins `fuzz/`.** `parse_stat` walks a record whose own length word decides
        where it ends, over bytes a server chose, and the listing reader steps by the count it
        answers - so it is now driven directly by `fuzz/fuzz_targets/p9_stat.rs`, asserting that a
        placed record lies inside its buffer. It is `pub` for that; `Stat` is `pub` with private
        fields to keep the interface honest (AGENTS §3 rule 2 - the p9 wire half had no target
        before, which this closes).
        Measured after: `boot-aarch64` exit 0 with `virtio-9p: "hostshare" exported over MMIO`,
        `"hostshare" mounted at /mnt/share` and `M8_P9_OK: /mnt/share is the host share over MMIO,
        mpos-9p-probe read back (39 bytes)`, no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0;
        `alpine-aarch64` exit 0 at `mpos login:` carrying the same three lines; x86 gate `pass=158
        fail=0 (158 built)` exit 33 with host 513 (was 512; `t-9p` is the 158th) and the test boot's
        log carrying `virtio-9p: "hostshare" mounted at /mnt/share` and `=== PASS t-9p`; depgraph 8/8
        within the ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none`,
        `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts unchanged on both
        machines (aarch64 lib 47, x86_64 lib 1); the new fuzz target compiles
        (`cargo check --bin p9_stat --target x86_64-unknown-linux-gnu`). `TERMINATORS` 28 → 29.
  - [x] 13.30 a procfs without `/proc/<pid>/environ` made OpenRC mount over a live `/proc` - the
        fourth `[ !! ]` the Alpine boot hid, and the first one to hide behind a *passing* verdict.
        OpenRC's `init.sh` decides whether `/proc` still needs mounting by reading
        `/proc/self/environ` twice under two different values of a variable it sets on the command
        line (`VAR=a md5sum /proc/self/environ` against `VAR=b md5sum ...`) and treating two
        *different* digests as proof that a live procfs is already there; identical digests mean the
        path is stale cruft in the root filesystem, which is the case the test exists to catch. This
        kernel answered `ENOENT` at that name, so `[ -e /proc/self/environ ]` failed, the check never
        ran, and OpenRC concluded "not mounted" and mounted over the procfs the disk `/init` had
        already mounted: `mount: mounting proc on /proc failed: Resource busy`, the `/proc` service
        reporting `[ !! ]` - while `cargo xtask alpine-aarch64` still reported success, because it
        keys on `login:` and the runlevel carries on past a failed service either way. The gap was in
        the *ABI*, not the boot: `Task` already carried `argv` for `/proc/<pid>/cmdline` and had no
        `env` to carry the environment beside it, so `execve`'s `envp` laid the block on the initial
        stack and was then dropped. `Task` and `TaskSnapshot` now hold `env` - set by `spawn_init`,
        `try_spawn_user` and `exec_replace`, inherited by `fork` and by `CLONE_THREAD` - and
        `/proc/<pid>/environ` and `/proc/<tid>/environ` read it back NUL-separated, present but empty
        for a task that was never exec'd, which is Linux's own shape. The verdict grew the pin that
        was missing: `openrc_failure` scans the log for openrc's marker (stripping the escapes that
        make it coloured) and names the service by its own line, so a failed service now fails the
        command rather than sitting in a log nobody greps. Measured after: the same boot prints
        `* /proc is already mounted`, carries **no** `[ !! ]` line anywhere, and reaches `mpos login:`
        in 13 s, exit 0; `boot-aarch64` unchanged (full chain, `INIT_EXITED`); aarch64 sweep
        `pass=27 fail=0`; x86 gate `pass=158 fail=0 (158 built)` exit 33; the new xtask unit test
        `an_openrc_service_failure_is_named_by_its_own_message` pins the checker against the captured
        failing block, escapes and all; fmt clean.
  - [x] 13.31 `ptrace` on the second machine, and the two defects that hid behind the gate - the
        third of Phase 15's named differences, and the one a user found by hand. `dispatch` carried
        `#[cfg(not(target_arch = "aarch64"))] nr::PTRACE` and `handle_ptrace` the same attribute,
        read as "the request numbers and `user_regs_struct` are x86_64's". Half true: the request
        *words* are one protocol (the same numbers in `abi`'s table on both machines), and what is
        really the machine's is **which slot of the register file a role lives in** - so `abi::uregs`
        now has two columns (27 words of `user_regs_struct`, 34 of `user_pt_regs`), the handler reads
        roles rather than names (`RESULT`/`IP`/`STACK` in place of `RAX`/`RIP`/`RSP`),
        `FPREGS_BYTES` is 512 with the `fxsave` image on one machine and 528 with the
        `fpsr`/`fpcr` pair swapped on the other, and `AUDIT_ARCH` is the machine's - `strace` refuses
        a `GET_SYSCALL_INFO` answer whose `arch` is not the one it was built for. The three
        remaining x86-only names (`UREGS_COUNT`, `uregs_by_name`, and the x86 column itself) are
        `cfg`-gated with the boot check that reads them.
        **Two defects came out of the new test, both older than the port.** (1) `PTRACE_PEEKTEXT`/
        `PEEKDATA` and `PEEKUSER` wrote the word *through* `data` as if it were a buffer, where Linux
        answers it as the call's result - so `ptrace(PEEKDATA, pid, addr, 0)`, which is the form every
        libc makes, hit `buffer_ok(0, 8, ...)` and answered `EFAULT`. (2) On aarch64 the syscall
        **entry** stop's register round-trip put the call's number into the ABI's result slot, and on
        this machine that slot is `x0` - the call's first argument, not a number register (the number
        is in `x8`). Because the gate re-reads the arguments out of the frame after the stop, the
        tracee made its call with the number as `arg0`. Measured: the first syscall after
        `PTRACE_TRACEME` - a traced `write(2, ...)` - wrote to descriptor 94, and the `_exit` that
        followed became `exit_group(94)`, so the tracee vanished with its own syscall number as its
        exit status. The fix is a `Gate` operation that asks the question the slot actually poses
        (`syscall_entry_result_slot`): the number on x86_64, where `syscall` left it in `rax`, and
        the frame's own `x0` on aarch64.
        **Pin.** `user/tests/src/bin/t-ptrace.rs`, on **both** machines - `TRACEME` and the stop it
        produces, `GET_SYSCALL_INFO` at a signal stop (`NONE`, the machine's `arch`, the tracee's own
        `ip`/`sp`, which is what `strace`'s startup probe reads and rejects the whole structure over),
        `SETOPTIONS` and its refusal of an unknown bit, `GETREGSET`/`NT_PRSTATUS` with the `iovec`'s
        length written back, `PEEKDATA`/`PEEKUSER` answering a word, `SYSCALL` to a boundary stop
        whose `ENTRY` names the call and whose `EXIT` reports its return value, `CONT` and the
        tracee's own exit. `strace` itself ships in the second machine's image
        (`rootfs-aarch64.toml`) and was read back on it: `strace -f -e trace=openat ls /` printed
        `openat(AT_FDCWD, "/", O_RDONLY|O_LARGEFILE|O_CLOEXEC|O_DIRECTORY) = 3` and `-c` printed its
        summary table. Also fixed: `t-clone`'s thread wrote its release word (`THREAD_WROTE`) before
        the two values the reader then reads - a race in the test, invisible until this cut's extra
        test shifted the timing, and already right in `t-futex`.
        Measured after: `boot-aarch64` full chain + `INIT_EXITED`; aarch64 sweep `pass=28 fail=0`;
        x86 gate `pass=159 fail=0 (159 built)` exit 33; `alpine-aarch64` exit 0 at `mpos login:`
        with no `[ !! ]` line; `depgraph` within the ratchet; fmt clean.
  - [x] 13.19 `reboot(2)` answers Linux's shape on **both** machines - two mechanisms behind one
        symptom, and the symptom was on every aarch64 boot.
        **What was seen.** `mpos: syscall 142 -> ENOSYS`, once, between the kernel's handoff and the
        login prompt of the second machine's Alpine boot. `142` is `__NR_reboot` on arm64 (it is
        `sched_setparam` on x86_64), and the caller is busybox `init`, which asks for
        `LINUX_REBOOT_CMD_CAD_OFF` about a second into its startup - the call that takes
        Ctrl-Alt-Del for the `::ctrlaltdel:` line of `/etc/inittab`. The kernel's own comment beside
        the handler had already measured that caller ("the first thing that ran after the handover -
        busybox init - asked for all of them within a second of starting"), and on x86_64 it is
        served.
        **Mechanism 1: a stale gate.** The dispatch arm and the handler were both
        `#[cfg(not(target_arch = "aarch64"))]`, written when a *stop* word ended at the x86_64
        `isa-debug-exit` port. That port is a `hal` capability now (`hal::exit_qemu`, which a machine
        without `mpos_qemu_debug_exit` answers by halting), and §14.3's S2 record says in as many
        words that this is what the second machine's `reboot` path should get "rather than a second
        body of code" - so the gate was a marker left behind by the capability, and nothing in the
        handler names a register, a port or a machine. Both gates deleted.
        **Mechanism 2: the words are 32 bits wide.** The pin written for mechanism 1 then failed on
        *both* machines, and not vacuously: `reboot(LINUX_REBOOT_CMD_CAD_ON)` was `EINVAL` while
        `CAD_OFF` (zero) was served. Linux's entry point is
        `reboot(int magic1, int magic2, unsigned int cmd, void *arg)`, so the kernel narrows each
        word to its low 32 bits - and it is the *libc's* prototype that decides what lands in the
        64-bit register: musl declares `reboot(int type)`, so every word at or above `2^31` arrives
        **sign-extended** (`0x89abcdef` reaches the kernel as `0xffffffff89abcdef`), which the
        full-word comparison read as a command no kernel knows. Three of the five words are up there -
        `CAD_ON`, `ENABLE_CAD` and `HALT_SYSTEM` - so busybox `init`'s shutdown half and `halt -f`
        were among the callers being refused, on x86_64 too, since the handler was written.
        `abi::reboot_command` now reads the three words as `u32`.
        **Pins.** `user/tests/src/bin/t-reboot-libc.rs` on both machines: it drives `libc::reboot`
        (the entry point a C program reaches, with musl supplying the magics), asserts both policy
        words are served, and asserts an unknown command is `EINVAL` rather than `ENOSYS` - the
        difference between "no such word" and "no such call". It never touches a stop word, and it
        skips as a non-root caller (see the named divergence below). Host-side,
        `abi::reboot_reads_its_words_as_linux_narrows_them` pins the narrowing itself, with the
        extension asserted to be *real* for the two words that have it, so the test cannot pass
        against a kernel that narrows nothing.
        **Named, not fixed:** the handler does not ask who is calling. Linux requires `CAP_SYS_BOOT`
        and answers `EPERM` to an unprivileged caller; this kernel answers `0` to anyone, on both
        machines. That is a capability check this kernel does not model, it is older than this entry,
        and the pin says so in its own header rather than asserting the divergence.
        Measured after: aarch64 sweep `pass=27 fail=0` exit 0 (`t-reboot-libc` staged and passing, was
        26); x86 gate `pass=157 fail=0 (157 built)` exit 33; host 512; depgraph 8/8 mutual pairs
        within the ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none` and
        `aarch64-unknown-none`.
  - [x] 13.20 the root slot knows its filesystem - the invariant a whole class of answers rests on,
        and the bug that broke the second machine's package manager.
        **What was seen.** On the aarch64 Alpine tree, every `apk update` answered
        `WARNING: updating and opening https://…/APKINDEX.tar.gz: Cross-device link` for both
        repositories and then `2 unavailable, 0 stale; 27 distinct packages available`: the index was
        fetched, could not be given its name, and was therefore re-fetched on every run. `apk`'s
        index write (`apk-tools`' `apk_ostream_to_file_safe`) is four steps -
        `openat(cache_fd, ".", O_RDWR|O_TMPFILE|O_CLOEXEC, 0644)`, write,
        `linkat(AT_FDCWD, "/proc/self/fd/N", cache_fd, "…tmp.PID", AT_SYMLINK_FOLLOW)`, then
        `renameat` into place - and the `EXDEV` is the kernel's answer to step 3, about an inode born
        in the very directory it was being linked into.
        **How it was localised, in three runs.** (1) `user/tests/src/bin/t-tmpfile-link.rs` drives
        those four steps as a program; its first aarch64 run was red with **errno 2** - not the bug,
        but the *harness*: the aarch64 sweep's runner mounts only `/tmp`, so `/proc/self/fd/N` did not
        resolve (the x86_64 harness's `/init` mounts `/proc` with busybox, and a real boot has one
        already). The test now mounts a procfs of its own when the payload has none, and says which
        it used. (2) With `/proc` present the run was red with **errno 18**, reproducing the report
        exactly. (3) One temporary `print!` in `handle_linkat` gave the whole answer in one line:
        `via_desc=true served=(false,false)` - the descriptor path *was* taken, and **both**
        `fs_serving_inode` calls returned `None`, which the identity test reads as "different
        filesystems". `fs_serving_inode` answers a mount's own root by that mount, and *anything
        else* by `root_fs` - so the root slot was empty.
        **Two omissions, one shape.** `Vfs::mount_root` recorded the root's type, device and mount id
        but never the filesystem itself; and the aarch64 boot attaches its disk through `Vfs::mount`,
        which recorded `fs: None` - a *name* attachment with no instance behind it - which
        `pivot_root` then promoted into the root slot. The first machine never hit it because its
        root is attached by **userland**: the initramfs's `mount -t ext4` goes through the `mount(2)`
        handler, which records the instance, and busybox `switch_root` promotes that with `MS_MOVE`.
        So the two machines' roots differed in exactly the way the report's instinct said they
        might - the second one assembles its root in the kernel, along a path the first machine's
        boot never takes.
        **Fix.** `Mount::fs` is an `Arc<dyn FileSystem>` rather than an `Option`: a mount record
        always names the filesystem it attaches, and the type is the invariant. `Vfs::mount_root` and
        `Vfs::mount` take the instance (every caller updated, including the host tests), `pivot_root`'s
        parked old-root entry carries the instance the replaced tree was served by, `bind_mount`
        borrows the serving instance or refuses (`NoRoot`), and the aarch64 board attaches its ext4 as
        `Arc<dyn FileSystem>` - the same shape the `mount(2)` handler builds.
        **The same root cause, two more symptoms on that machine, fixed with it.** `statfs("/")`'s
        block numbers and `mount -o remount,rw /` both read the root slot (the id-0 arm of
        `fs_serving_path`, and `Vfs::remount`), so both answered "no filesystem" there - the first as
        the syscall layer's fabricate-a-filesystem fallback, the second as a silent no-op. OpenRC's
        `root` service asks for the second on every boot.
        **Pin.** `t-tmpfile-link` runs on both machines: on aarch64 its root is the ext4 disk, so it
        drives all four steps for real and was red with `EXDEV` before the fix; on the x86_64 gate,
        whose root is the initramfs's ramfs, `O_TMPFILE` answers `EOPNOTSUPP` and the test drives
        `apk`'s own named-temporary fallback instead - and *says so*, because the ext4 half is the pin
        and the ramfs half is only the honest path that machine can run. That asymmetry is named here
        rather than hidden: the x86_64 gate has no ext4 root to test the `O_TMPFILE` half on.
        Measured after: aarch64 sweep `pass=27 fail=0` exit 0 (was 25, with this test red); x86 gate
        `pass=157 fail=0 (157 built)` exit 33; host 512; depgraph within the ratchet
        (`vfs/mod.rs -> drivers|net` 0/0, 8/8 mutual pairs); fmt clean; clippy 0 errors on both
        bare-metal targets.

**ABI truthfulness (Phase 12 — Group A landing, gate now runs to a summary)**

**The sweep's *set* is now a configuration, and its default is smaller on purpose.** Every test

  reds that `AGENTS.md` had to call "load flakes"; they, and `t-udp`/`t-tcp-close`, are now run
  by name. A default red is therefore the kernel's, which is the only property that makes the
  gate worth reading.
  §'s work was red on `t-times`'s *the elapsed clock advances across a spin* while the check
  directly after it — that `tms_utime` grew — passed, and a re-run was green. The mechanism was
  in the arithmetic and not in the kernel: `times()` returns the wall tick count and
  `tms_utime` the process's own CPU tick count, both scaled by the same truncating
  `ticks * 100 / TICK_HZ` (`handle_times`), so the spin — whose exit condition is the *CPU*
  counter crossing a `USER_HZ` step — can compare two wall readings that floor to the same
  value. The kernel's answers are Linux's; the assertion was the unsound part, and it is now
  two checks where there was one. Monotonicity is asserted over the spin, which is all a spin
  can prove, and the *strict* advance is pinned where it is deterministic: `times(NULL)` read
  around a 50 ms `nanosleep`, which is five `USER_HZ` steps at the ABI's 100 Hz and so cannot
  be truncated away.
