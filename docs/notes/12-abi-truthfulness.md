# §12 Phase 12: ABI truthfulness — **AUDIT DONE, NOTHING FIXED**

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

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
  load flake — the last of these pointed at the working copy's then-current `AGENTS.md`, which no
  longer records it). That is a *newer* state than §11.1.12's "green", which described the last
  committed baseline (Step A). **Nothing in this phase is verifiable until the gate is green
  again**, so restoring it is task zero, ahead of B and C.
- **Some of these are recorded as `Done` where they are not.** `docs/notes/`'s own table lists
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
  holds here because the no-follow resolver stops at the link instead of reading a target: it is the
  ordinary walk with `LastLink::NoFollow` on its last name now (`docs/uapi/` §5.1 item 45, which is
  what made it cross a mount on that name too), and before that it looked the last name up with
  `Inode::lookup`, which read no target either — in both shapes ramfs hands back the link node
  itself, and the walk's checkpoint is the one that says so generically. Pinned by the host
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
| **B2** | IPv4 tails at `socket.rs` `recvfrom`/`recvmsg` (compute `nonblock` from `O_NONBLOCK` only) | `MSG_PEEK` does not consume; `MSG_DONTWAIT` earns `EAGAIN` | **was: `MSG_PEEK` on TCP/UDP consumed the datagram and returned success** (silent data corruption); `MSG_DONTWAIT` ignored | **LANDED + gated.** All four IP recv/send tails now route the wait through `dontwait(fd, flags)` (honours `MSG_DONTWAIT`), and the two recv tails return `EOPNOTSUPP` on `MSG_PEEK` - the same honest refusal `AF_UNIX`/`NETLINK`/`PACKET` already give. The peek refusal it left was **served on 2026-10-20** (`docs/uapi/` §5.1 item 72): the flag no longer consumes, a short datagram copies what fitted and keeps the message whole, and `MSG_TRUNC` answers the message's own length. Pinned by the new `t-msg-peek` beside `t-sock-flags`, which keeps the `DONTWAIT`→`EAGAIN` half. `pass=131 fail=2`, no net regression. |
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
- `SO_SNDBUF`/`SO_RCVBUF` (`record-only`) — already tracked in §3.3 / the ABI board;
  this phase does not duplicate it.
  (`/proc/<pid>/smaps` was on this list and is not any more: `docs/uapi/` §5.1 item 54
  served it, with the accounting rule this kernel can state named field by field
  beside the three numbers it cannot derive.)
  (`SO_REUSEADDR` was on this list and is not any more: it is applied at
  `bind`/`listen`, §3.3.3. `SO_LINGER{on, 0}` was too, and is not any more: the
  last `close` resets, §3.3.4.)
- Real `MSG_PEEK` over TCP/UDP was on this list and is not any more: it is served with Linux's
  own reading of the flag pair (`docs/uapi/` §5.1 item 72), and the prerequisite this line used
  to name never had to exist - smoltcp's `tcp::peek`/`udp::peek` were already there. What stays
  a sub-gap is real `rseq` state and real per-task CPU accounting: named here as sub-gaps,
  planned where the prerequisite (the clock, the scheduler's own timekeeping) is scheduled.

---


### R.12 (moved from ROADMAP's status board) — the ioctl buffer-direction audit, and what Phase 12 left open

**A9 ioctl buffer-direction audit.** The whole `handle_ioctl` surface was swept for the one class Phase 12 is about — a request that answers success having done less than Linux. Every `_IOR`-style getter that fills the caller's buffer was checked against the `buffer_ok(…, write, …)` direction it validates: `TIOCGWINSZ`/`TIOCGPGRP`/`TIOCGSID`/`TIOCGPTN`/`TCGETS`, the `VT_OPENQRY`/`VT_GETMODE`, `RNDGETENTCNT`, `RTC_RD_TIME` and every evdev `give`/`give_state` correctly require a **writable** destination (Linux's `put_user` faults a read-only one with `EFAULT`), and each `_IOW` setter correctly requires only a readable one. The sole outlier is the block-device size trio (`BLKGETSIZE64`/`BLKGETSIZE`/`BLKSSZGET`): they write the size into `buf` but validated it `write=false`, so a read-only destination was accepted and filled where Linux answers `EFAULT`. **Fixed in the Phase-12 leftovers pass** (docs/uapi/ §5.1): the three validations read `true` now, so a `PROT_READ` destination faults as Linux's `put_user` does — measured on a real device, all three are `EFAULT` against a `PROT_READ` mapping. It remains **unpinned in the gate** for exactly the reason it was recorded rather than fixed the first time: the host `user_accessible` stub answers accessible for both directions, and the selftest boots **without a disk**, so no block node exists to open and reach the path. When a block node enters the test image the pin is a `t-ioctl` `PROT_READ`-buffer `EFAULT` line.

**What backing individual options would mean.** `prctl`'s catch-all is now `EINVAL`, which meets the objective; options like `PR_SET_DUMPABLE`/`PR_CAPBSET_*` remain unbacked by real state, and *serving* them per-device from live console state (rather than refusing) is the structural Phase 11 B2 (V-2) move, with the measured Linux values recorded in §12.1 A2 so B2 implements rather than guesses.
