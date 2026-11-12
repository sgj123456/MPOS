# §1 Phase 1: Syscall Completion — **DONE**

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

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

**The reported word is `revents & interest`, and setting `EPOLLRDNORM` beside
every `EPOLLIN` was a bug that stalled the desktop.** `report()` used to OR
`EPOLLRDNORM` into any readable report and `EPOLLWRNORM` into any writable
one, so a caller that asked for `EPOLLIN` was handed `0x41`.
`weston-desktop-shell` registers its timerfd for `EPOLLIN` alone and refuses
to dispatch a report carrying anything else — it printed
`unexpected timerfd events 41` (the value is hex), returned without reading
the timer, and never finished starting up, which is the "desktop sometimes
never comes up" symptom this repo had been working around. Linux answers
`revents & interest` from the file's own poll mask, so the bit a caller named
is the bit it gets; the two spellings still arm the same report. Fixed in
`report()`, pinned by a timerfd case in `t-epoll` (`EPOLLIN` asked →
`EPOLLIN` back, no `EPOLLRDNORM`; `EPOLLRDNORM` asked → that bit) and by the
three host tests in `vfs::epoll::tests` whose expectations were themselves the
bug (they asserted `65`). Measured: with the fix the shell's message is gone
from weston's log on the same boot sequence that produced it twice before.

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
| `vhangup` | — | Done | dispatched, both cases honest. With no controlling terminal it answers `ENOTTY` (Linux's answer, gated by the corrected `t-posix`); with a controlling terminal it answers `EOPNOTSUPP` — the hang-up itself (tty re-init + `EIO` on open fds) is `docs/notes/` §12.2 B4, a tty-layer feature the kernel does not perform, so it refuses rather than fakes the success. The nice round-trip that used to sit on this row (`getpriority`/`setpriority`) landed in Phase 12 §12.2 B5 and moved back to `Done`. `src/syscall/linux/posix.rs` |
| `accept4`, `sendmmsg`, `recvmmsg` | 288/307/299 | Done | `src/net/socket.rs` |
| `rt_sigpending` | 127 | Done | `src/syscall/linux/posix.rs`; reads `current_pending()` |
| `io_uring_setup/enter/register` | 425-427 | Served (measured subset) | the ring ABI was measured and the synchronous subset served - `docs/notes/06-process-management.md` §6.32, item 115 |


---

### 1.4 The inventory's own invariant, re-measured — **it was false** (2026-11-12)

**What was claimed.** `docs/uapi/index.md` states the rule beside a one-line generator: extract
`pub const NAME: u64 = N` from `nr_x86_64.rs`, keep the names that appear as `nr::NAME` in the
dispatch, and *"what matters is arms = rows: a syscall landing without a row here is the review
finding"*. That is the right rule. Nobody had run it.

**What it measured.** Three commands, on the tree as it stood:

```
grep -oE 'pub const [A-Z0-9_]+: u64' src/syscall/linux/nr_x86_64.rs | wc -l   # 301
grep -oE 'pub const [A-Z0-9_]+: u64' src/syscall/linux/nr_aarch64.rs | wc -l  # 301
grep -c '| `' docs/uapi/01-syscall-table.md                                    # 318
```

The two tables agree with each other and both dispatch arms are complete - 301 names, 301 distinct
`nr::NAME` arms, no orphan in either direction. The prose was the part that had drifted: it claimed
`**293**` arms and `**251**` §1.0 rows, and neither number was true. Comparing names rather than
counting rows is what turns a count into a finding:

- **three arms with no row**: `sync_file_range` (277), `futex2_wake` (454), `futex2_wait` (455);
- **two rows with no arm**: `futex_wake` at 454 and `futex_wait` at 455. `FUTEX_WAIT`/`FUTEX_WAKE`
  are **operation words** of the one `futex(2)` call - `nr_x86_64.rs` declares them `usize` for
  exactly that reason - so the rows named a number from the wrong namespace, and the same two
  numbers were *also* listed correctly as `futex2_*` a few lines below;
- and `fsopen`/`fsconfig`/`fsmount` sat in **§1.1 ("Deliberately `ENOSYS` — refused")** while
  their own rows said "served". They were not refusals; the section heading had not been revisited
  when they landed.

**Why a count would not have caught it.** 318 rows against 301 arms looks *over*-covered, not
under-covered - and four of those rows were §1.1/§1.2 refusals, which are supposed to be there.
The defect is only visible as a **set difference**, which is why the fix is a function rather than a
number in a document: `xtask/src/gates.rs::syscall_inventory` extracts the arms from the dispatch
and the §1.0 row names from the table and reports both differences. It costs two file reads, runs
in `cargo xtask test` beside the other two structural checks, and is pinned twice:

- `inventory_drift_is_reported_in_both_directions` - a fixture tree carrying one arm with no row and
  one row with no arm, asserting exactly those two and nothing else (a checker that cannot see the
  defect agrees with itself and pins nothing);
- `the_syscall_inventory_covers_every_dispatch_arm` - the real tree, which would have reddened on
  this date. Its counterfactual was run: delete the `sync_file_range` row and the test fails with
  `what: "`sync_file_range` has a dispatch arm but no §1.0 row"`.

**The lesson, and it is the file's own subject.** A document that says how it should be checked,
beside a check nobody runs, accumulates exactly this: the prose ages, the rule stays true, and the
gap is invisible because both halves are individually plausible. The tell is a *count* stated in
prose with a *command* beside it - if the command is cheap, it belongs in the gate, not in the
document.

**Not a code change.** No behaviour moved: the dispatch was complete and correct throughout, and
every one of the 301 arms still answers what it answered. What was wrong was the record of it.
