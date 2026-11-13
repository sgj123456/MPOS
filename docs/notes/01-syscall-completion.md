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

### 1.5 The same blindness, twice more: the fuzz workspace and `cargo fmt`

§1.4 recorded a document that stated its own invariant beside a check nobody ran. Two more
instruments turned out to have the same shape, found the same week by asking of each "what would
make this go red?":

**`fuzz/` was in no gate.** It is its own cargo workspace (correctly - the kernel's
`[build] target = x86_64-unknown-none` cannot link a libFuzzer binary), and nothing in
`cargo xtask test` built it. Eight of its twenty-two targets had not compiled since the kernel's
APIs moved underneath them. The workspace's own build cost **13.4 s cold, 9.9 s to merely check** -
versus a gate that spends 28 s on stage 1 and 116 s per arm - so the check was affordable the whole
time and simply was not there. Fixed in item 260 (`docs/uapi/05-work-list.md`), with the two claims
kept apart: declarations are a file walk, builds-and-runs are a cargo invocation, and only the
second can find a defect that compiles.

**`cargo fmt --check` never saw `fuzz/` either.** The root `Cargo.toml`'s `members` list does not
include it (`fuzz/` is its own workspace root), so "the tree is fmt-clean" was a claim about a tree
that excluded twelve percent of the repository's source files - and those files were not clean
(eleven diffs across five targets). The instrument was right; its subject was smaller than the
sentence it was quoted for.

**Rule, and it is §1.4's rule generalised:** for every convention this repository states, the
question to ask is *which command makes this false?* A convention whose command does not exist, or
whose command cannot see the subject, is a claim - and the cheap ones (a file walk, a `fmt --check`,
a `cargo check`) belong in the gate, because the gate is the only place a convention is re-measured.
The tell in all three cases was identical: prose with a command beside it, and nobody running the
command.

### 1.6 The same blindness a fourth time: `cargo clippy`, and no stage ever ran it (2026-11-13)

§1.4 and §1.5 found three instruments nobody pulled: a `docs/uapi` inventory, the `fuzz/` workspace,
and `cargo fmt --check`'s view of that workspace. The fourth was found the same way it always is -
by asking, of `AGENTS.md` §2's list, *which command makes this sentence false?* - and the answer for
two of the five lines was **none**.

```
$ grep -rn 'clippy\|cargo", "fmt\|rustfmt' xtask/src/
$ echo $?
1
```

Neither `cargo clippy` nor `cargo fmt --check` appeared anywhere in the gate's four cheap checks, its
stage-1 invocations, or any command. Both had been in `AGENTS.md` §2 as commands a change runs since
the document was written. Worse, the first one was **red**:

```
$ cargo clippy --target x86_64-unknown-none
error: this loop never actually loops
   --> src/net/socket.rs:500:5
    = note: `#[deny(clippy::never_loop)]` on by default
error: could not compile `mpos` (lib) due to 1 previous error; 147 warnings emitted
$ echo $?
101
```

**The defect the lint was pointing at was real, and it was introduced by a fix.** `git log -L` on the
lines: `48917cf` wrote `fn wait_channel` as `while !ready() { … } 0`, then `d3d22d4` - *"a socket's
timeout bounds its wait"* - wrapped it in `loop { while !ready() { … } return 0; }`. The wrapper's
body always returned on its first pass, so the `loop` restarted nothing; the timeout it was added to
express lived entirely in the inner `while`. Reading the function, the `loop` looks like a retry and
there is no retry. Removing the wrapper restores the shape the fix meant: a `while` around the park,
with the deadline consulted inside it. Nothing about the runtime behaviour changed - which is exactly
why no test could have caught it, and why the lint was the only instrument that saw it.

**Why a ratchet, and not a clean baseline.** The tree is not one warning away from green. Measured
with `--message-format=json`, level ∈ {warning, error}, after the four fixes below:

| arm | diagnostics | lints | errors |
|---|---|---|---|
| `mpos` (`x86_64-unknown-none`, release) | 162 | 39 | 0 |
| `mpos` (`aarch64-unknown-none`, release) | 168 | 39 | 0 |
| `mpos --lib` (`x86_64-unknown-linux-gnu`) | 389 | 26 | 0 |
| `mpos-user-tests` (`x86_64-unknown-linux-musl`, release) | 273 | 44 | 0 |
| `mpos-xtask` (`x86_64-unknown-linux-gnu`) | 3 | 2 | 0 |
| `mpos-boot` (`aarch64-unknown-uefi`) | 1 | 1 | 0 |

`cargo clippy --fix --allow-dirty --allow-staged` was the obvious route to zero and it does not work
here: it ran for **9080 ms** and left the tree **byte for byte identical**, still advising
`cargo clippy --fix --lib -p mpos -- --no-deps` for 51 of the suggestions. Removing 162 diagnostics is
a round of work with its own measurements; making sure the 163rd never arrives is a `const` table and
two seconds. So each arm's diagnostics are counted per lint, recorded in `xtask/src/clippy.rs`, and
the gate fails on **three** things: an `error`-level diagnostic at all, a count that grew, and a lint
that has never been seen in that arm. A count that *fell* is not a failure - it prints the line
telling whoever lowered it to lower the constant, because a ratchet left high can no longer catch a
regression back up to it.

**Six arms, because a lint is reachable on one and not another.** `dead_code` is 21 on x86_64 and 30
on aarch64; `missing_safety_doc` is 5 and 1; and on the host `--lib` half `src/syscall/linux/abi.rs`
alone answers **252** - every `AT_*`/`O_*` constant whose caller sits behind `target_os = "none"`.
One "the kernel" row would be a number that describes neither machine. The host arm needs `--lib`
for a different reason: a bare host `cargo clippy` also builds `src/main.rs` and `src/board/`, which
are machine-only, and fails `E0432: unresolved import mpos::boot::handoff` (measured; 395 warning
lines before the error). `--lib` is the half that exists on the host.

**The four fixes, each a claim that was false rather than a style preference.**
`clippy::mut_from_ref` at `user/tests/src/bin/t-io-uring.rs:232` was the tree's one *error*: a
hand-built io_uring `fn push(&self) -> &mut Sqe` handed a caller a mutable borrow of memory it held
only shared. The test binds its ring immutably and then mutates through it, so the honest fix is the
`&mut self` chain (`push`, `one`, and the two `let ring`/`let tiny` bindings). `clippy::erasing_op`
found `let managed = i + (x as i64 & 0)` in `t-iret` - an anti-optimisation that `& 0` folds away,
i.e. a barrier that had never been one; it is `core::hint::black_box(x)` now.
`clippy::absurd_extreme_comparisons` found `after.field(3) >= 0` in `t-diskstats`, where the field is
a `u64`: a vacuous half of a claim, now just the half that can fail. And `clippy::never_loop` found
the socket wait above. Each was a test or a function asserting something about itself that was not
true, which is the reason `AGENTS.md` §3 exists.

**The parsing is crude on purpose, and validated anyway.** `xtask/Cargo.toml` depends on `anyhow`,
`clap`, `libc` and `ratatui` - no `serde_json`, and adding a JSON parser for a lint count is the
wrong trade. So a line is a diagnostic iff it contains `"reason":"compiler-message"`, the level is
read after the **last** `"level":"` on it (children and nested diagnostics repeat the key earlier),
and the lint after the **first** `"code":{"code":"`. That scanner was checked against a real
`serde_json` parse over all six arms' captured output - 996 diagnostics - with **zero mismatches and
zero false positives**, and never more than one `"code":{"code":` token per line. Crude is fine
when it is measured; §1.5's rule is about an instrument nobody runs, not one that is plain.

**Cost, because a check that costs a minute gets deleted.** Warm, on a tree the gate has just built:
124-155 ms for each of the two kernel arms, 121-122 ms xtask, 110 ms boot, 139-214 ms the user suite,
83-135 ms the host half. Cold (`CARGO_TARGET_DIR` under `build/`, nothing built): 8401 ms and
8470 ms for the two machine arms, 6322/3440/6139/7059 ms for the other four. That is why the step
sits in **stage 1** beside the other cargo invocations rather than in the cheap checks ahead of the
sweeps - warm it is under a second, and it reads the artefacts stage 1's own build produced.

**`cargo fmt --check` arrived with it**, as the same step, because it is 1.5-1.8 s and §1.5 had paid
for the lesson already: the root workspace's `members` list does not contain `fuzz/`, so "the tree is
fmt-clean" was a claim about a tree missing twelve percent of its source. Both workspaces are asked
now, the second one by `--manifest-path fuzz/Cargo.toml`, and the pair is the reason the function is
not one command.

**The pins.** `xtask/src/clippy.rs` carries ten unit tests over the scanner and the verdict logic -
a child note is not a second diagnostic, an uncoded warning counts under its own name, every arm's
baseline is sorted and unique, every arm names a target and no two arms share arguments, a grown
count and a never-seen lint are both violations, and a falling count is an improvement rather than a
failure. The end-to-end pins were three deliberate breaks, each reverted: lowering `mpos-xtask`'s
`too_many_arguments` from 2 to 1 reddened with *"is at 2, up from the recorded 1"* (`build/scratch/cf-count.log`,
exit 1); a `fn clippy_probe_new_lint(x: u32) -> bool { if x > 3 { true } else { false } }` appended to
`src/vfs/fdinfo.rs` reddened three arms with *"`clippy::needless_bool` is at 1, and the tree has never
had that lint"* (`build/scratch/cf-newlint3.log`); and a blank line in `fuzz/fuzz_targets/ext4_image.rs`
made `cargo fmt --check` on that workspace exit 1 with the diff naming the file. Raising the constant
to 5 instead printed `fell from 5 to 2 - lower the constant` and passed, which is the half that keeps
the ratchet honest rather than merely strict.

**Gate:** `build/gate-clippy.log`, `GATE_EXIT=0` - stage 1 **1310 tests, 0 failed** (161 of them
xtask's, ten new), x86_64 `pass=307 fail=0`, aarch64 `pass=306 fail=0`, kunit 35/0, fuzz 23 targets,
lint 162/162 + 168/168 + 389/389 + 273/273 + 3/3 + 1/1, formatting clean in both workspaces.
