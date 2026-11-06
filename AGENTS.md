# AGENTS.md

How to work in this repository. Read this before your first edit. The other docs split the rest:
`README.md` = what MPOS *is*; `docs/ARCHITECTURE.md` = how it is put together, the module graph and
where the layering is violated; `docs/ROADMAP.md` = the plan (phase-by-phase roadmap, status board,
remaining work, what to do next); `docs/notes/` = the evidence behind it (one file per phase plus an
index split from the old single `NOTES.md`; a `§N.M` pointer routes through `docs/notes/index.md`):
every phase's measured status, the ledger of every gate run and measurement, and the home of any new
plan; `docs/uapi/` = the userspace-visible surface as one row per promise. This file is only **how to
work here**, and points at those rather than repeating them.

---

## 1. Repository map

| Path | What lives there |
|---|---|
| `src/` | kernel crate `mpos`: `lib.rs` (host-testable) + `main.rs`, the one architecture-independent entry and panic handler, over `src/board/` — the boot: one `start` and one flow for every machine (`flow.rs`: console, device step, ramfs, archive, display, wire, stack, pid-1 handoff), the steps with ordering contracts (`steps.rs`) and the banner/canary/park/panic reporting (`report.rs`). The flow carries no architecture; which *transport* carries a device is the probe's question (`src/drivers/virtio/probe.rs`), and the kernel mounts nothing but the ramfs — `/dev`/`/proc`/`/sys`/the root/the share are the initramfs script's (`assets/initramfs/core/init`). Why here and not `mpos-arch`: `docs/ARCHITECTURE.md` §3's downward-only rule. A test build adds `suites.rs` + `kunit.rs` (the one place that lists suites); the boot narrative and its measurements are `docs/notes/` §15.19–§15.23 |
| `arch/` | `mpos-arch`, the machine layer: `hal/` (the capability traits, their forwards, `hal::kernel`'s table of kernel services), `arch/{x86_64,aarch64}/` (the two ports, their linker scripts, everything that *is* the machine) and `early/` (the same machines as the *bootloader* sees them — descriptors, wire, jump — the tree compiled for `-unknown-uefi` rather than `-unknown-none`, `docs/notes/` §14.6). A crate so the machine layer cannot name the kernel (§3); `src/lib.rs` re-exports `{arch, hal}` so kernel paths are unchanged. Not under `crates/` — that means "host-testable leaf", and this cannot compile for the host |
| `crates/` | downward-only leaf crates (§3): `err`/`abi`/`pci`/`date`/`binfmt`/`cred`/`uts`/`seccomp`/`term`/`ahci`/`gpu`/`evdev`/`uinput`/`blk`/`fdt`/`bootinfo`/`vm` |
| `boot/` | `mpos-boot`, the UEFI loader — its own target (`-unknown-uefi`), no kernel dep, no architecture in it at all (the three machine things it needs are `mpos-arch`'s `early/`). A **library plus a thin `#[entry]` binary**: `src/lib.rs` (`mpos_boot::run`) holds the whole sequence and `src/main.rs` is the firmware entry that calls it, so the kernel can become the second caller (`docs/notes/` §14.7). The handoff record is one definition in the `crates/bootinfo` leaf, read by both machines and both sides of the seam |
| `xtask/` | `mpos-xtask`, the build/run/test front-end; `xtask/README.md` documents every command |
| `user/` | `mpos-user` (`std` programs); `user/tests/` is the whole ABI suite — the `no_std` suite that sat beside it in `user/abitest/` finished moving across (§15.11's last batch) and was deleted, as was the wired-into-no-build `user/staging/`; a measurement's instrument now lives beside the claim it measured (a probe under `.workbuddy/`, a table in the `docs/notes/` row) or in the test itself |
| `fuzz/` | `mpos-fuzz`, **its own workspace**: libFuzzer targets over the kernel's parsers |
| `assets/rootfs/`, `assets/configs/rootfs.toml` | the guest image's contents, for both machines: `rootfs.toml` is one file in three sections (`[default]` is the baseline both read, `[x86_64]`/`[aarch64]` are each machine's own half), and `assets/rootfs/x86_64/` + `assets/rootfs/aarch64/` are the two overlay trees; `assets/rootfs/core/` is the shared half merged before either |
| `assets/initramfs/`, `assets/configs/initramfs.toml` | the early-userspace tree the kernel unpacks as its first root, and its packages — the **same three-part overlay as the rootfs** (`core/` merged first, carrying the one `/init` both machines run; each machine's own tree over it, empty today), built by the **same pipeline** (`xtask/src/initramfs.rs`) through its toml section |
| `assets/configs/machine.toml` | what the image is *booted* on — memory, cpus, devices, run mode — in the **same three sections**. Host-side only, so editing it never reinstalls |
| `assets/configs/tests.toml` | which test programs the sweep runs by default, and its groups — one file in the same three sections as every config file, where the only exclusion-shaped group is a machine's `gaps` |
| `assets/configs/busybox-applets.txt` | the applet paths (`bin/ls`, `sbin/init`, …) one per line, from `busybox --list-full` — the farm `rootfs::lay_applet_farm` links on every build, *before* the overlay; pinned on both machines by `t-busybox-farm`. Why the build lays it: `docs/notes/` §15.34 |
| `build/`, `target/`, `build-test/` | build output and scratch. Never edit, never commit |

Root `Cargo.toml`: `default-members = ["."]` is load-bearing — without it a plain `cargo build`
would build the musl user programs for the host.

---

## 2. Build, run, verify

Prereqs: the six targets and the host tools (`qemu-system-{x86_64,aarch64}`, OVMF/AAVMF, the image
and `musl` toolchain pieces) are listed in full in `xtask/README.md` — check them there.
`rootfs`/`initramfs` fetch Alpine packages over the network.

| Command | What it does | Root? |
|---|---|---|
| `cargo xtask build` [`--arch x86_64\|aarch64`] | kernel + UEFI loader, **for the machine `--arch` names** — the switch drives both `--target`s (`x86_64\|aarch64-unknown-none` and `-unknown-uefi`), not just which image gets booted | no |
| `cargo xtask disk`/`rootfs`/`all` [`--arch x86_64\|aarch64`] | the disk image and the Alpine tree installed into it, **for the machine `--arch` names**: `disk` creates its raw image, `rootfs` formats the GPT, installs the packages through the block layer and writes its boot files, `all` does the lot after the kernel and the archive. One implementation for both machines; the per-machine parts are the package architecture, the boot file name and the overlay tree. A kernel/loader/initramfs change refreshes **boot files only** — no package install | yes |
| `cargo xtask run` [`--mode terminal\|gui\|mirror`] [`--arch x86_64\|aarch64`] | boot in QEMU, rebuilding stale bits. `--mode` picks the window backend and the guest's `console=` list together; `--arch` picks the image — each machine boots its own installed Alpine root (pid 1 is the disk's `/init`) | when needed |
| `cargo xtask test` [`--arch x86_64\|aarch64`] [`--all-targets`] | **the gate**: host unit tests once, then the *same* sweep on every machine it names. **Both machines are the default** (`--all-targets` spells it out); `--arch` narrows the run to one, `--no-all-targets` is the old single-machine default on purpose. Each arm runs the same lanes, the same live dashboard (`tui::Panel`) and the same report — the payload's *channel* is the only difference (the first machine's boot archive is its root; the second stages onto a disk its lanes lay). Neither arm can silently run the other machine | no |
| `cargo xtask depgraph` | measure the module graph against the Phase-11 ratchet; fails when an edge appears or a count grows | no |
| `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` | just the host unit tests | no |
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | xtask's own tests | no |
| `cargo xtask clean` | remove `build/` and `target/` | when needed |

```bash
cargo fmt --check
cargo clippy --target x86_64-unknown-none            # kernel, bare metal
cargo clippy --target x86_64-unknown-linux-gnu       # kernel, host
cargo clippy -p mpos-xtask --target x86_64-unknown-linux-gnu
cargo clippy --target aarch64-unknown-none           # everything under arch/src/arch/aarch64/ is
(cd boot && cargo clippy --target aarch64-unknown-uefi)   # #[cfg]-ed out of every x86 target above
```

**Two build traps.** (1) **`--target` is mandatory for host work**: `.cargo/config.toml` pins
`[build] target = "x86_64-unknown-none"`, so a bare `cargo test` builds for bare metal and cannot
run. (2) **The aarch64 target links in `--release`
only**: a debug build fails with `relocation R_AARCH64_ABS64 …` from the *sysroot's* `liballoc`, and
no gate notices — anything hand-verifying an aarch64 change must pass `--release`.

**The self-test protocol.** Stage 1 = host unit tests. Stage 2 = builds the *test* kernel
(`--features selftest`, copied out of the shared target dir by `utils::build_test_kernel` — a
production build carries neither the boot's check families nor the harness that runs them,
`docs/notes/` §15.19), packs both user suites into a throwaway initramfs with a generated `/init`,
and parses the serial log for the kernel's own `mpos-kunit: N passed, M failed` line plus
`=== RUN/PASS/FAIL t-name` / `=== SUMMARY pass=N fail=N` / `=== END`. **Both** halves are asserted:
the sweep must end with a summary **and** QEMU exit code 33 (`isa-debug-exit`), and the kernel's line
must be there and say zero failed — a suite that silently never ran leaves its cases out of the
count. A test that hangs, crashes or never prints `=== PASS` counts as failed — no `#[ignore]`
escape hatch; a test that cannot work is deleted or made to pass.

Both machines read the protocol from the same `TEST_INIT` runner and the same verdict reader; what
differs is where the payload sits — the first machine's programs ride in that throwaway archive
(its boot keeps it as its root), the second machine's ride on the root partition of a medium the
sweep lays for itself (`disk::sweep_medium`, its own `build/<arch>-sweep.img`, so it cannot
overwrite the image `run` installed), because its boot switches root. The *selection* is one
question on both arms now: a program runs when it is in no group or in a group the run asked for,
`--groups`/`--all-groups` mean the same thing on either machine, and the only group neither may
ask for is a machine's own `gaps` — its list of what it cannot serve yet (§15.36).

**`tests.toml` picks which programs run.** All are built; grouped ones are left out of the default
sweep. `[default]` holds the groups both machines read — `external` (needs the host network or a
listener), `slow` (measurement/seconds), `diagnostic` (not a check at all) — and a machine's own
section *adds* to the baseline group of the same name rather than replacing it. `--groups <a,b>`
replaces the file's `default`; `--all-groups` runs every group a machine can see; an undefined group
name is an error — a sweep that quietly runs fewer tests than asked is the one failure this must not
have. A new test lands with a group decision; silence is the default. Nothing a test runs can leak
into the image `cargo xtask run` boots (test artefacts live under `build/`).

**Reporting the gate — never from a stale run.** This file carries **no test counts and no sweep
composition figures**: a snapshot here goes stale within a commit or two (the last such snapshot —
per-arm program counts — sat wrong for whole sessions). The measured state lives in `docs/notes/`
(its latest §12.4a summary line; every phase records what it measured). Re-run the gate and quote
the summary line + exit code — or say plainly it did not get there.

---

## 3. Code conventions

### Every design here is Linux's behaviour — never a toy of it

This governs everything below. MPOS exists so **unmodified Linux binaries run**, which makes failure
asymmetric: a wrong *shape* sends the caller down a path it cannot recover from; an honest errno
sends it down its own fallback. **Nearly-right is worse than absent.**

1. **Measure before designing.** A behaviour a real program depends on is observed on real Linux
   first, and the observation is written down with the number beside the claim. A man page does not
   say which half of the rule the caller branches on.
2. **Answer truthfully, or refuse.** `ENOSYS` (call absent here), `EOPNOTSUPP` (present, cannot do
   that), `EINVAL` (argument refused), `EACCES`/`EPERM` (will not grant). The `_ => ENOSYS` arm in
   `src/syscall/linux/mod.rs` should stay crowded. **Returning success for something not implemented
   is the single worst thing this codebase can do.**
3. **No private dialects.** A flag, struct or protocol gets its Linux shape, not a convenient one.
4. **Refuse rather than approximate.** When the honest alternative to doing it properly is doing it
   *nearly*, refuse.
5. **A gap gets a name, not a stub.** Whatever cannot be built today is a named gap in
   `docs/ROADMAP.md`, with its measurement and missing prerequisite in `docs/notes/`. A `TODO` in shipped code, a handler
   that returns 0, and a shape that looks finished while doing less are the same violation.
6. **A gap gets a pin, or it is not done.** Host test, `t-*` program, or both. "Works, verified by
   hand once" is how this repo lost the same bug four times.

### Five standing checklist rules (before anything lands)

1. **Kernel-space discipline.** What Linux runs in userspace is not this kernel's job — it provides
   the ABI userspace expects and nothing more.
2. **High cohesion, low coupling.** Before adding a feature, locate the module that owns the
   concept. A change reaching into another module's internals is in the wrong module. This and
   #5 below are governing rule 1 and the downward-only boundary as review questions.
3. **No toy or temporary designs.** Governing rule 3 + 4 as a checklist item; never waived for convenience.
4. **Every behaviour ships with a pin that would fail without it.** Counterfactual as the criterion:
   a test that passes with *and* without the change pins nothing.
5. **Respect the designed dependency graph.** Modules depend on each other only along the edges the
   design allows; update `docs/ARCHITECTURE.md` (§2 + §3) *before* adding an edge, not after.

### Module boundaries

The table, the enforcement story and the two rules that make it usable (violation-is-not-precedent;
a needed edge changes the design record in the same commit) are in `docs/ARCHITECTURE.md` §2/§3 —
that file and this one move together. The working copy of the rule: a module may call **downward
only**; `cargo xtask depgraph` re-measures every `crate::` reference and fails on an unrecorded
edge; the purest leaves live in `crates/` so an upward edge from them is a **compile error**.

### Seven extended rules (what a change carries, what it may not touch)

Each exists because of a failure this repo has actually paid for.

1. **A test is a contract, not an obstacle.** When the gate goes red, the code changes, not the
   test. Where a test's *expectation* is genuinely wrong, the commit body carries the measurement
   proving the kernel's answer is Linux's.
2. **Every new parser gets a fuzz target.** Any place the kernel reads user- or wire-supplied bytes
   joins the targets in `fuzz/` in the same commit, or `docs/notes/` says why not.
3. **A published ABI is append-only.** Syscall numbers, struct layouts and ioctl encodings are
   contracts once shipped. Renumber/repack/reinterpret only with an explicit migration in `docs/notes/`.
4. **Worst-case rule.** The global allocator still panics on failure (`docs/notes/` §8.0), so until
   that closes, any new path whose allocation volume a user can steer carries a bound — capped
   count, checked length, or `ENOMEM` at the limit.
5. **Interrupt context does none of: sleep, allocate, walk an unbounded structure.** Bounded work,
   no lock a preempted task may hold, defer the rest. On one core, spinning on a preempted holder is
   a deadlock with the machine stopped on it.
6. **A performance change ships with before/after numbers.** `docs/notes/` §10 is only true if
   entries are measurements.
7. **One intent per commit.** A refactor and a behaviour change in one diff have no single *why*.
   Split: the refactor lands first, green on its own, then the change.

### The `no_std` split is the central convention

Arch-independent logic lives in the library and is host-tested; anything touching port I/O,
descriptor tables, virtio rings, firmware or a real clock is gated behind `#[cfg(target_os = "none")]`
(and `target_arch = "x86_64"` where the hardware is). Pure logic belongs host-side even when its
only caller is bare metal. The per-module rule (home: `src/lib.rs`'s header): **a module is
host-compiled when its logic touches no hardware, no port I/O, no virtio ring and no real clock —
and then its dependencies must be host-compiled too, transitively.** The transitive half is what
gets forgotten. `cargo xtask test` opens with a mechanical check for the failure the compiler
cannot see — a `#[cfg(test)]` module behind a `target_os = "none"` parent (`xtask/src/gates.rs`).

### Concurrency

- **Uniprocessor**: disabling interrupts *is* mutual exclusion.
- Shared state is `critical_section::Mutex<RefCell<T>>`. **Never `spin::Mutex`** — busy-waiting on a
  lock held by a task this core preempted is a deadlock (`spin::Once` only for one-time `gdt`/`idt`).
- **Never sleep holding a lock.** The pattern is *lock, poll, try, unlock, park* — see
  `src/net/mod.rs` and `docs/ARCHITECTURE.md` §7.
- The idle halt belongs to the scheduler's loop; a task that `hlt`s while staying runnable costs a
  whole tick on every wake (`docs/notes/` §10.2).

### Comments, errors, panics

House style is long-form prose doc comments explaining **why**, not what: `//!` headers run to
paragraphs, and a non-obvious decision names the rejected alternative and the evidence. Match that.
Don't add a comment that restates code; don't delete an existing rationale comment while editing
around it. Panic ends the machine (`panic = "abort"` + a handler that exits QEMU). Syscall handlers
return Linux errno constants through `err` and never unwrap user input.

---

## 4. Recipes

**Add/change a Linux syscall.** (1) `src/syscall/linux/nr.rs` — the number and `pub const` (the test
harness `#[path]`-includes this file). (2) `src/syscall/linux/mod.rs` — the dispatch arm. (3) the
handler in `fs/file.rs` (path/file/device calls) or `fs/ioctl.rs` (the device-`ioctl` entry point),
plus `proc.rs` (memory/process/time) or `posix.rs` (identity/capabilities/scheduling/clocks);
validation with the handler, pure ABI facts in host-compiled `abi.rs`. (4) an unimplemented call
falls to `_ => ENOSYS` (§3). (5) a conformance program + its `[[bin]]` entry (next recipe).
(6) update `docs/ROADMAP.md` (the status board + "Remaining ABI support") and `docs/uapi/`,
and `docs/notes/` when a measurement or a lesson is added — a call moving
between served / refused / wrong-shape belongs in both.

**Add a user test program.** `std`/`rustix` → `user/tests/src/bin/t-<name>.rs` (flat,
auto-discovered), because that shape runs on **both** machines and takes its per-claim printing and
exit status from the crate's own `Report` (`user/tests/src/lib.rs`) instead of a local copy. Use
`rustix` first; drop to `libc` only where the crate's rule allows it, and read that rule where it is
owned — `user/tests/Cargo.toml`'s header states the measured clauses (unbound call, wrong wrapper
syscall, shape cannot express the claim, plus the bitflags-vs-enum distinction and the
numbers-from-rustix convention), each with its measurement beside it. Print `ok:`/`FAIL:` per check
and a final `PASS`/`FAILED` (`Report::finish` does both). Keep the `t-` name (the test `/init`
globs `/bin/t-*`). Decide its group in `tests.toml` — under `[default]` it binds both machines,
under a machine's own section it is that machine's alone, and a name in a machine's `gaps` must be
a program that machine actually builds (§5's trap).

**Add a kernel unit test.** A `#[cfg(test)] mod tests` at the bottom of the file it tests, with the
parent module compiled for the host (§3). Assert on the contract (value, errno, count), not a log
line; prefer a test that would fail if the policy differed.

**Add a boot self-test.** Something checkable only on a booted machine goes in a suite beside the
module it tests: `#[cfg(all(target_os = "none", feature = "selftest"))] pub mod kunit;` there, a
`pub const SUITE` naming its phase and cases, and one line in the wiring layer's `SUITES`
(`src/board/kunit.rs`) — the only place that lists them. Being a child of the module is the point:
it reads the module's private state, so no accessor is added to `mm`/`sched`/`vfs` for a test's sake.
A case **answers** `Result<(), String>` through `case_check!` rather than asserting — `panic =
"abort"` means an `assert!` would end the machine and hide every case after it — and it declares the
[`Phase`] it needs (the boot step, not a position), because a suite that runs too early asserts
against a machine that is not ready. `cargo xtask test` is the only thing that builds them
(`--features selftest`) and the only thing that asserts the harness's own `mpos-kunit` line
(`docs/notes/` §15.19; §15.20 for a suite that already moved).

**Change the guest image.** A file that must exist → the `assets/rootfs/<machine>/` (or `assets/initramfs/<machine>/`) tree (overlays
apply *after* anything the toml writes). A package → `rootfs.toml`/`initramfs.toml` (under
`assets/configs/`), under the section of the machine it is for, with a key dotted by what it
belongs to (`rootfs.mirror`, `system.hostname`, `user.name`). An
**account a package's own post-install script would make** (dbus's `messagebus`, busybox's `klogd`,
seatd's `seat`, weston's `weston-launch`) → `SYSTEM_GROUPS`/`SYSTEM_USERS` in `xtask/src/rootfs.rs`:
a cross-arch install runs `apk --no-scripts`, so the build writes those lines itself, and the names,
ids and order are measured against the two images' own `/etc/passwd`/`/etc/group` (`cargo test -p
mpos-xtask` pins the shape, `seat`'s id included - the configured account joins that group, so an
image that never made it gets a second one at a regular id). Partition /
fs-type / image-size / hostname / DNS → `rootfs.toml`. Anything about the *machine* (memory, cpus,
`vsock`, `audio`, window backend, run mode, extra QEMU args) → `machine.toml`, **never**
`rootfs.toml`: its *shape* keys (mirror, branch, repos, filesystem, disk geometry) decide whether
the Alpine tree is laid fresh, while its package lists alone install and remove only the
difference. A machine key there would still charge the image for a memory size either way, which is
why the split stands (`xtask/src/config.rs`'s `the_machine_and_the_image_keep_their_own_keys` fails
if one drifts back). Boot wiring
(`console=`'s list) is read via `src/boot/console_list.rs`; the kernel is told which consoles it
has and nothing about how it was launched. It used to read a second word, `mpos.selftest`, to decide
whether to run its own checks — that is a *build* now, so no boot word decides it (§15.19).

**The second machine's userland is its own half** (`rootfs.toml`'s `[aarch64]` section + the
`assets/rootfs/aarch64/` overlay; `docs/notes/` §13.12): a package goes in the section, a file in the
overlay, and `cargo xtask rootfs --arch aarch64` (or `all --arch aarch64`, or a `run --arch aarch64`
that had to rebuild) installs it — the first machine's own command, the same block-layer install,
with the machine's `--arch` for apk. What is *not* shared is the half of an install that runs the
image's userland: on aarch64 `Arch::host_runs_guest_userland` is false, so apk runs `--no-scripts`,
the chroot steps are skipped, and busybox's applet farm is laid by **the build** instead
(`rootfs::lay_applet_farm`) — the measurement and why both machines then boot the same standard
Alpine chain: `docs/notes/` §15.34.

**Touch the boot handoff.** The `Handoff` record is one definition now —
`crates/bootinfo` — read by both the loader (`mpos-boot` depends on it) and the
kernel (`src/boot/handoff.rs` re-exports it). Changing a field changes the ABI
an older `mpos.elf` on a partition is read with, so bump `HANDOFF_MAGIC` unless
the change is append-only. Keep `BOOT_STACK_CANARY` in `crates/bootinfo` — the one
definition the loader and both kernels read now (§13.23) — and the loader's
"allocate everything before `exit_boot_services`" ordering intact.

---

## 5. Known traps

| Trap | What to do |
|---|---|
| A new test program silently never built | Add the `[[bin]]` block; `xtask` errors on "no binary" only for sources it finds |
| `cargo xtask test` goes red on `t-posix`'s "getpriority answers 20" and nowhere else | **Mechanism found 2026-10-28**: `t-priority` set its whole **process group's** nice (`PRIO_PGRP, 0, -2`) and restored only `PRIO_PROCESS`, so the runner shell stayed at `-2` and every later program in that lane inherited it (nice survives `fork`/`exec`); a fresh process then reads raw `22`, not `20`. Fixed by restoring by group in `t-priority`, pinned by its "group is left at the nice it started with" claim — a lane that still reddens is a *new* bug, not the old flake (`docs/notes/` §6.31, §6.18) |
| A lane times out at 900 s with `t-ns-pid`'s last line being "while a fork into the namespace whose init has exited is ENOMEM" | **An observed hang, not a diagnosis** (twice on 2026-10-23, in runs whose surrounding programs differed, while the same program passed on the runs between). The timeout costs ~15 minutes of wall clock, so re-run and quote the log's own `=== SUMMARY` line rather than the elapsed time; the section it stops in waits on a pipe from a forked child, which is where to look first (`docs/notes/` §6.22 records the two readings). **Since 2026-10-23 that read is bounded and the section marks six stages**, so the failing claim reads `the init section ran to its end (last stage N)` and N says where it stopped - a stage below 6 means the section itself stalled. Since the per-program budget landed (`tests.toml`'s `timeout`/`timeouts`), a program that hangs is killed at its own budget and named `=== FAIL <name> timeout=<n>s`; the lane's 900 s is only the outer bound now, so a lane that still reaches it has a runner problem rather than one blocking test |
| A wait's deadline is built from `hal::ticks()` (the tick *counter*) but its park answers `since_boot_nanos` | **The two clocks are one masked window apart**, and the mix is a self-sustaining spin: the park answers `Deadline` at once, the caller re-parks under the critical section, the asserted timer PPI is never delivered, the tick counter freezes and the expiry check never fires (the aarch64 sweep freeze of 2026-10-24, mechanism and closure: §6.24). The rule now enforced by shape — `block_until_deadline`'s tick multiply is **deleted**, every wait names its deadline against `since_boot_nanos` and converts at construction; a new caller reaching for `hal::ticks()` to bound a wait is writing the bug again |
| A borrow of the ext4 cache (or any `RefCell`) spans a device call | Block transfers **park their task** now (§10.5), so a borrow held across `read_at`/`write_at` is the `RefCell already borrowed` a second task dies of the moment the transfer yields. The shape that works: claim (the slot out of eviction's reach, the scratch out of the cache), release the borrow, park on the device, land the bytes under a fresh borrow — `vfs::ext4::cache`'s `FillClaim`/`RunClaim`, and `update_block`'s copy-out before the write. A claim in flight is neither a hit nor a victim, and one transfer in flight per device is the mailbox's own rule (`begin_request`/`finish_request`) |
| An aarch64 lane dies at the boot's EL0 check with `EXCEPTION: synchronous` (EC 0x25, `FAR` the page past `kernel_end`) — the lane hangs at the exception | **An observation, not a diagnosis** (twice, 2026-10-25: one lane each in two different arms, siblings green, both under a host running the whole sweep's TCG machines at once). The dump's own `ELR` points at `pl011` print code whose instruction cannot abort, so the report is not trustworthy at face value — read only its shape (§13.32). Kill the lane's QEMU so the harness concludes, re-run, quote the green one |
| A `src/` file nothing declares looks real | A `mod` declaration makes a file part of the build; missing ones left duplicate serial/RTC drivers for months (V-7). Check for the declared one before writing a new file |
| A module has a host body and a machine body | **Both move, or it does not build.** Run the bare-metal target on every pass (V-11: the host suite was green while `x86_64-unknown-none` failed on a non-exhaustive `#[cfg]` arm) |
| A flag or struct constant looks shared but the machines disagree | It is the machine's — `open(2)`'s `O_*` words and the overlap bit (§13.13's session-costing `ELOOP` bug), `struct stat`/`statx`, the signal frame. Read **the machine's** uapi and pin both machines (`t-open-flags`) |
| `cargo xtask run` prints no `mpos-kunit` lines | By design — the boot's own suites are in the *test* build alone (`--features selftest`, which only `cargo xtask test` builds). A session's wire shows the machine's own boot log and nothing else |
| `cargo xtask test` stage 1 goes red in `rand` and nowhere else | **Fixed 2026-10-28** — the generator is a value with the kernel's one instance behind the lock, so the re-key test no longer depends on what else drew from the shared stream, and the test-only `fingerprint()` is gone. A red there is a regression now, not a flake. The same shape — a module global read or written by parallel tests — is how the devpts option tests flaked 5 of 40 runs: a test that touches a module global takes that module's test lock. Measurement: `docs/notes/` §12.4a |
| `cargo xtask test` stage 1 goes red and the captured tail names no failing test | **Do not call it a transient; run the suspect alone.** The 2026-11-04 version of this red was `vfs::ext4::tests::relatime_moves_atime_only_when_something_has_happened`, which **failed 10 times out of 10 on its own** and passed only behind ~700 neighbours: the host has no real clock, so `inode::now()` is a process-global counter, and the test invented a stamp (`first + 20`) that outran every mtime a three-draw write could produce, so the rule's `atime <= mtime` clause never fired. Two days of "unexplained concurrency transient" readings were this. The test now derives both stamps from what the write produced; a test asserting a *relationship* between two clocks must never invent one of them (`docs/notes/` §3.26) |
| A stop is a record, not a sleep — and a park that only re-checks it is half a fix | **Fixed 2026-11-06.** `trace::park` (`src/sched/runtime/trace.rs`) published the stop, then slept **once while its task was still in the ready queue the picker pops from** — so the picker handed it straight back, `sleep` returned, and a `SIGSTOP`ped task ran on with the record still installed (`mpos-dbg: PARKWAKE id=6 stop_now=true`). Isolated fixtures never saw it; six concurrent TCG lanes did (`t-ptrace-attach`'s `(state "", collected true)`). The fix is **both** halves: `policy::leave_ready` takes the parking task out of every ready band, and `park` re-checks the record after each wake and re-parks while it stands, with `SIGKILL` the one wake that ends it (through `die_of_fatal_signal`, under the same table lock — a loop without that check hangs `t-waitid`, which kills a stopped child then blocks in `waitid(WEXITED)`). A stop-state copy cannot pin this: `set_trace_regs` only asks whether `trace.stop` is `Some`, which stays set either way — pin the task **running** (`waitpid(WNOHANG)` after the tracee's own 50 ms timer), `docs/notes/` §6.88 |
| A record read off the task table is a read of a **different subject**, not a stale one | **Fixed 2026-11-06.** A pending exit record (`Retired`, `src/sched/runtime/task.rs`) outlives the slot its task had, and four readers answered "is this mine?" by asking the *slot*: `let free = t.tasks[id.0].is_none(); free` then `waitable(..)` reads an empty slot as "the record must be mine" — which is exactly backwards, because a slot is empty **because** the record outlived its task. A process therefore collected a stranger's death (`MY waitpid(7, WNOHANG) = 7 errno=0` where Linux answers `ECHILD`) and the real parent got `ECHILD` for its own child. The record now carries its owner (`Retired::parent`, the **group**, retargeted when the reparenting walk moves the corpse); every reader asks the record, and `handle_wait` asks `is_child` before it takes anything. The tell for this class: a lookup keyed on an **id that can be recycled** must find its ownership in the record, never in the structure the id indexes — `docs/notes/` §6.90, `t-steal` |
| A pid number handed to a syscall is used as a task-table slot | **Fixed 2026-11-06.** A number is only meaningful **in a namespace**: inside a pid namespace the group is `1` and its child `2`, while their slots are global numbers far above both. `handle_prlimit64`'s cross-process half, `live_thread` (seven scheduling calls) and `setpriority`'s `PRIO_PROCESS` arm indexed the table with the raw number, so `prlimit64(2)` answered `0` with a stranger's table and `setpriority(2, 7)` answered `0` after writing a stranger - a set and a get through one number were two different tasks. Its siblings `getpriority` and `/proc/<n>/limits` already resolved first, and that disagreement was the tell. Every one of these doors now goes through `resolve_pid`/`pid_task` first; on a machine that never unshares the two spellings are the same integer, so **no existing reading moves and only a namespace fixture can pin it** (`t-nsnumbers`). Rule: a door that takes a pid **resolves** it - never indexes with it - `docs/notes/` §6.91, item 204 |
| A pid-taking handler binds `let (_pid, ...)` | **Fixed 2026-11-06.** `sched_getaffinity`/`sched_setaffinity` were the last two that never read the number, so a collected child, a stranger's number and `-1` all answered `0` where Linux answers `ESRCH` - and a request about somebody was answered about the caller. The underscore **is** the bug: it says the argument was parsed only to be discarded, while the call still acts on a task. Two more shapes hide beside it and a one-CPU machine cannot see either: this setter refused every length below a whole word where Linux honours one byte, the getter accepted a length that is not a whole number of words, and the getter zeroed the caller's tail - a write nobody asked for. The refusal **order** is not derivable: measured, the getter judges length before number and the setter number before length (`build/scratch/aff/q2.c`). Rule: resolve the number first and refuse `ESRCH`, and when several arguments are bad, use Linux's own order rather than a convenient one (`t-affinity-rules`, `docs/notes/` §6.92, item 205) |
| A guard is written from the source instead of measured | **Settled 2026-11-06, and it had been written down backwards.** `sched_setaffinity` wants `CAP_SYS_NICE` only when the target belongs to **another user** - a same-uid stranger's child is served, so blood relation is not the rule - and `sched_getaffinity` asks for **nothing at all**: as uid 1000 with an empty `CapEff`, reading pid 1 answered 16 CPUs. The earlier note here asserted a getter-side `ptrace_may_access` check, which Linux does not have; a guard written from that sentence would have refused reads the kernel must serve, which is the failure mode AGENTS.md §3 rule 2 exists to prevent. Authority also sits **before** the mask judgement (`set(<pid 1>, 0, NULL) => EPERM` against `set(<no such pid>, 8, NULL) => EFAULT`), so a caller without the capability cannot probe the CPU count one refusal shape at a time. And "the payload runs as root, so it cannot be pinned" was false too: a child may `setgid`/`setuid` to `nobody` and then ask about its root parent - `t-proc-access` had done exactly that for months. Rule: measure the guard, not the comment (`build/scratch/affperm/v.c`, `docs/notes/` §6.92) |
| A handler that changes another task answers `0` while writing it | **Fixed 2026-11-06.** No arm of `handle_setpriority` had an authority check, so a program running as one user lowered **another** user's task's nice, got `0`, and had it written - and a later `getpriority` on that task read the intruder's number. Linux asks **two** questions with **two** errnos, and one code for both is the tell that a handler is guessing: `EPERM` is ownership (not the caller's task, refused in *either* direction - a lowering of a stranger's nice is refused as surely as a raising), `EACCES` is direction (the task is the caller's but the value raises its priority, which wants `CAP_SYS_NICE` or `RLIMIT_NICE`). The ordering is measured too: an empty set is `ESRCH` before either, and ownership precedes direction. Do not derive it from the name - "may not retarget" sounds like blood relation, and a scan of 484 same-uid targets on this host answered `0` on every one of them. And read the fetched `errno` carefully: `EACCES` is 13 and `EPERM` is 1, and an early reading here called the former the latter and made the rule look like direction alone (`build/scratch/niceperm/y.c`, `docs/notes/` §6.93, item 206, `t-nice-authority`) |
| A table's infinity is a default, and a default is a promise | **Fixed 2026-11-07.** `RLIMIT_NICE` (13) and `RLIMIT_RTPRIO` (14) sit past the rows `default_rlimits()` fills, so they kept the table's `RLIM_INFINITY` starting value while Linux answers `0/0` for both - and that zero is load-bearing: it is the row `can_nice` reads to refuse a priority raise to a caller without `CAP_SYS_NICE`, so a program that read `infinity` there was told it might raise as it pleased and had no fallback left to take. The other half of the trap is the sentence that hid it: a doc comment said "this kernel does not carry `RLIMIT_NICE`", which was inherited rather than measured - `getrlimit` on that very machine answered `infinity`. Rule: walk the whole table before writing either "we have this row" or "we do not", and treat a row still holding the array's starting value as a decision nobody made (`build/scratch/rlim/r.c`, `docs/notes/` §6.94, item 207, `t-prlimit64`) |
| A resolution is a promise about the readings, and the two doors must agree | **Fixed 2026-11-07.** `clock_getres` answered one tick (4 ms) for **every** clock under a comment saying the tick counter was the only time source there was - while the `clock_gettime` beside it interpolates the free-running counter, so the measured `CLOCK_MONOTONIC` step was `616 ns` and `CLOCK_MONOTONIC_COARSE` stepped `352 ns`. A caller reads a resolution to size its own loop: told 4 ms, it gives up on anything finer, and told "coarse", it expects the cheap reading and got the full one. Measured on the host: counter clocks resolve to `1 ns`, the coarse pair to the tick, and 5000 back-to-back coarse reads move **0** times. The same walk found the id set three shorter than `clock_gettime`'s (`clock_getres(CLOCK_TAI, …)` answered `EINVAL` for a clock the sibling serves - one argument, two verdicts) and `CLOCK_BOOTTIME_ALARM` reading as wall time where the host has it `752 ns` from `CLOCK_BOOTTIME`. `Clock::resolution_nanos` answers the counter's quantum, the coarse pair reads whole ticks; pin `t-clock-getres` (`build/scratch/batch/g.c`, `k.c`, `m.c`, `docs/notes/` §6.97, item 210) |
| A field a reader parses is a promise, and "we do not count that" ages | **Fixed 2026-11-07.** `/proc/<pid>/stat`'s fields 14-17 (`utime`, `stime`, `cutime`, `cstime`) were a hardcoded `0` under a comment that listed page faults, CPU time *and* `wchan` as things this kernel did not account - true when written, and overtaken twice: `wchan` reads `park_site` now and the timer interrupt has charged CPU since `times()` landed. `ps`/`top` showed `0:00` for a busy process. Measured on a current host (`build/scratch/batch/s8.c`): field 14 tracks the **group's** CPU (`66` `USER_HZ` ticks = 0.667 s) while the reading thread's own `RUSAGE_THREAD` was 0.030 s - Linux's `whole` flag, set for `/proc/<pid>/stat` and a thread's `/proc/<tid>/stat`, clear only for `/proc/<pid>/task/<tid>/stat`; the unit is `USER_HZ` (100, ABI), not the machine's `TICK_HZ` (250). Rule: re-read the code a comment describes before trusting the "we do not do that" it states, and cross-check a procfs field against the syscall that reports the same number (`docs/notes/` §6.96, item 209, `t-times`) |
| A `who` Linux serves answers `EINVAL`, beside a sibling that resolves it | **Fixed 2026-11-07.** `getrusage`'s `who` has three values and this kernel's guard tested two (`SELF`, `CHILDREN`), so `RUSAGE_THREAD` (1) - a valid constant - came back as the answer for a malformed argument. The refusal was not recorded anywhere; it was the shape of a two-valued world left over from when "there is no per-thread split because there are no threads here" was true, and that sentence outlived `clone(CLONE_THREAD)`. The tell is the neighbouring arm that resolves the same concept correctly: here `SELF` read the group's counters and `CHILDREN` its own, so the third had a home to go to - a per-thread pair on the live `Task`, charged beside the group's in interrupt context. Serving it with the group's numbers would have been the approximating fix (a "succeeds" claim passes, the body is another thread's CPU). Rule: when `_ => EINVAL` sits next to an arm that resolves the same argument, check whether Linux serves the discarded value; and read a doc comment claiming the feature is absent against the code that now has it (`build/scratch/batch/a.c`, `docs/notes/` §6.95, item 208, `t-times`) |
| A test claim is built on a poll window and reddens under load | **If the property is a state the kernel reports, assert the state; let the poll only prove nothing was *collected*.** `t-ptrace-attach`'s "a detach leaves a stop the user asked for alone" failed a 20x10 ms `waitpid(WNOHANG)` window on busy lanes three times in one day and re-ran green every time: what a debugger needs is the `T` in `/proc/<pid>/stat`, and `R`/`S`/gone is the failure - a poll alone cannot tell "still parked" from "running but not collected yet". Now `!gone && state == "T"`, 8/8 green in one boot, both arms green (`docs/notes/` §6.78a) |
| `cargo xtask test` goes red on `t-cgroupfs`'s `mkdir grp: Os { code: 17, kind: AlreadyExists }` (`=== FAIL t-cgroupfs exit=134`) and nowhere else | **Diagnosed and fixed 2026-11-04** - the `EEXIST` is a *second* symptom: `members_of` in `src/vfs/cgroupfs.rs` tested a membership entry's generation with `&&` inside the arm instead of as a guard, so a stale entry (a slot reused after a cgrouped task exited) put the new tenant in **no** group at all and hid it from the root's `cgroup.procs`. The tree outlives every mount, so it took a *second* cgrouped program in one boot to reuse a slot; the gate's two cgroup programs sharing a lane is what made it occasional. Fixed in the arm, pinned by two claims in `t-cgroupfs`; the measurement and the counterfactual: `docs/notes/` §3.27. **A second cause of the same `EEXIST` landed 2026-11-04**: two cgrouped programs sharing a lane each make a group called `grp`, and the hierarchy is the machine's one tree that every mount is a view of, so the first program's group is still there for the second. Linux keeps it too (the `cgroup2` root is only freed when its last mount is gone *and* it has no children), so the fix is the program's own: `t-ns-cgroup` now `remove_dir`s what it made and asserts the tree's state - `docs/notes/` §3.28. A red here is a regression now, not a flake |
| Stage 2 goes red on `t-clock-gettime`'s "two reads never go backwards" | A **flake** of the same kind, in the wall clock rather than in a service — two successive `CLOCK_REALTIME` reads can step back. Re-run before believing it; the mechanism and measurement: `docs/notes/` §13.27 |
| A recorded measurement is about the machine that made it | A number already written down is not a measurement of *this* question. The tell is a claim that made a refusal permanent without saying which register, program or run — re-measure before building on it (§13.15) |
| A driver holds the transport *inside* its device struct | The crate's `Transport` is **not object-safe** (`begin_init<F: Flags>` is generic; `VirtQueue::new`/`add_notify_wait_pop` take `&mut impl Transport`), so `Box<dyn Transport>` cannot exist and such a device cannot be one `static`-held type shared by two machines. Two shapes work, and the error count does not tell them apart: open the device inside the driver module and hand the rest only a contract (`NetDevice`, `ConsolePort`, `InputDevice`), or make the device generic and let its *consumer* hold a `Box<dyn YourDevice>` (`Gpu`/`GpuDevice`, §13.26 — 3 errors either way). |
| A machine declares a capability and still has no device behind it | A capability flag makes a port cheap **only if everything behind it is gated on the capability, not the architecture** — an `#[cfg(target_arch)]` module gate hiding behind the flag multiplied the error cost (§13.23, re-measured as §13.24's lesson that a stale figure sits in the plan). And opening a device, proving the transport and dropping it is a milestone, not a driver (§13.24) |
| A boot check reads a counter two comparators can move | **Fixed 2026-11-07.** The aarch64 boot's sub-tick check armed the virtual timer half a period out, then required the **tick count** not to move - but the periodic clock and the one-shot are two comparators over one counter, so an arm landing near the periodic deadline lets the tick answer the park first. That is a confound of the measurement, not a violation, and it **halted a whole gate lane** (900 s outer bound, `the virtual timer's one-shot did not wake idle() two milliseconds out (ticks moved to 73, ctl 0x0001 = enable 1 imask 0 istatus 0 …)` in `build/gate-threadcpu.log`). The dump was the diagnosis: `istatus 0` says the one-shot had *not* reached its compare, i.e. something else woke `idle`. The verdict reads the hardware now - the `CNTV` handler disarms the one-shot before the EOI, so `CNTV_CTL_EL0.enable` clear means it fired - and retries a confounded arm from a period boundary; a check that assigns and asserts in the same register is a coin toss under load (`arch/src/arch/aarch64/checks.rs`, `docs/notes/` §13.33) |
| A check on an aarch64 device runs in front of the machine's own `init` | The device checks (`check_blk`, `check_fs`, …) run on **every** aarch64 boot, a session included, and halt the machine on failure (§13.14: a wall-clock check left the year 36812 and killed `syslogd`). A check may assert only what the **kernel itself** produces from the configured devices — never a file or byte pattern the build packed, or an image merely missing one becomes a machine that cannot boot (`docs/notes/` §15.34). Two consequences bite: the boot context has **no task identity** (`sched::current_owner()` panics — `there is no task 63`), so it cannot create a file or take a lock naming an owner; and a check that does change shared state must put it back |
| A `static` with a zero initializer and no writer lands in `.rodata` | Read-only, on a machine whose page tables enforce it — and it bites whatever intended to write it (stage 1d's arena, stage 2's task stack). The fix is frames from the allocator, not a section attribute; `-zmax-page-size` will not save you (`docs/notes/` §9.1 row 1d) |
| An assembly block that switches section switches it for every later block | Every `global_asm!` in a crate is one assembly unit, so an unrestored section change moves everything assembled after it — the symptom is a faulting fetch on a prologue that reads as "code in `.rodata`". The three defences and the measurement: `docs/notes/` §13.16 |
| An aarch64 exception entry that can switch tasks does not save `ELR_EL1`/`SPSR_EL1` | AArch64 keeps the interrupted `PC`/`PSTATE` in those registers (one pair per core, plus `SP_EL0` for an EL0 trap), not on the interrupted stack — and the IRQ stub preempts by construction, so any other task's trap overwrites them. Both stubs must save their resume state on their own stack; forgetting reads as the kernel resuming into another context's `PC`. The measurement: `docs/notes/` §13.21/§13.22 |
| No cross-architecture build | aarch64 boots the way x86_64 does — firmware, ESP, higher half, its own tables, EL0 address spaces — and the two arms share nothing but source layout. What compiles where, the three measured boot choices (`acpi=off`, `force-legacy=false`, device RAM cost) and the port's current reach: `docs/notes/` §9.1/§13 |
| `fuzz/` won't build from the root | It is its own workspace; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test never runs | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; a host test inside it is unreachable |
| Two `cargo xtask test` runs in one tree report nonsense | They share `build/test-*` artefacts and one reads a log the other is writing. Trust the log's own summary + `=== END`, re-run alone |
| A lane's sweep refuses every program after a while with `timeout: fork: Resource temporarily unavailable`, or dies with `all 64 task slots are in use` | **Diagnosed and fixed 2026-11-04** — two separate faults, both closed. The panic was `alloc_slot` ending the machine where Linux answers `EAGAIN` (now `Option` + `EAGAIN` at every spawn door, §6.81/item 198); the refusal that followed was the table being smaller than the machine's own runner, because `busybox timeout -s KILL <budget>` forks a **watchdog child that lives out the program's whole budget**, so a lane of ~50 programs held ~50 slots for two minutes at a time — `MAX_TASKS` is 256 now, at 8 MiB of stacks (§6.84, item 200). A red in this shape is a real fault again, not a flake: read `docs/notes/` §6.84's watchdog ladder before suspecting a leak |
| The two sweeps do not build the same programs | They do now — §15.11's last batch finished the migration and deleted `user/abitest`, so both arms build the same std suite — but the counts moved repeatedly, so read them off a run, never from here. "Every name `tests.toml` declares must be a program this arm built" is still two questions: the first machine's sweep asks it of the whole file, the second of its own `gaps`. A `[default]` group's reason is the same on both machines, so a name in one is a program both arms build — the narrower question is not wrong, only smaller (`xtask/src/sweep.rs`'s `select` records why) |
| An in-place `debugfs` edit to the image "goes away" on the next build | It does **not**: `cargo xtask rootfs`/`all` write boot files and the package difference and leave every other inode alone, so a file injected with `debugfs -w -R write` survives every refresh. A probe written into the image is removed in the same session, and a reading taken on an image names the files it was taken with (this is how §7.9f's "the 120 s does not reproduce" was wrong - the image was still running the §7.9c probe; §7.9h) |
| The test machine ≠ the run machine | `xtask test` boots an initramfs on its own QEMU wiring; `xtask run` boots the disk image on `machine.toml`'s. A device only one has is a shape no test sees — name a driver's "controller with nothing on it" behaviour on the machine that has one before calling it measured (§7.3's 28 s empty-controller boot) |
| A device class needs a *host* backend, not just a QEMU device | vsock is the one virtio device QEMU does not emulate in userspace — `vhost-vsock-*` is a host kernel module, so a machine naming it without `/dev/vhost-vsock` refuses to start. Attach only where the host can back it (`qemu::virt_machine_args` asks), and print the absence rather than dying of it. Give each guest a distinct CID (the two machines take `3` and `4`): a second guest claiming a live CID aborts QEMU before the guest starts (§13.28) |
| A driver compiles, is host-tested, and **nothing calls it** | The shape that hid the 9p share for months: `fs9p::init_global` filled the registry every mount reads and was called from nowhere, while every boot printed "no share on the bus". A complete driver plus green host tests is what hides it — they pin the parser, not the wiring. The tell is a boot log line that never changes: grep for it and check whether any commit changed it (§13.29) |
| A wire client that has never run against a real server | The 9p client implemented plain 9P2000, which this host's QEMU 11 answers `"unknown"` to — its hand-built fixtures could not know. A protocol client's own fixtures are not a measurement of the server it talks to: ask the real one, and fix toward the dialect it serves rather than a negotiation loop that hides which one you got (§13.29) |
| A booted measurement was taken with a kernel the loader does not carry | The UEFI loader embeds the kernel as a deflate payload (`boot/src/lib.rs:176`, `PACKED_KERNEL`), so **rebuilding `-p mpos` changes nothing a boot can see** — `.workbuddy/one-test-boot.sh` only copies `mpos-boot.efi`. A whole counterfactual here measured the fixed kernel twice and "passed" both times. Rebuild both: `cargo build --target x86_64-unknown-none --release` then `MPOS_KERNEL_ELF=$PWD/target/x86_64-unknown-none/release/mpos cargo build -p mpos-boot --release --target x86_64-unknown-uefi` (a bare `cargo build --target …-uefi` is not this; it fails with 57 errors). Put a marker `println!` in the kernel and look for it in the log before believing anything else — and `MEM=3072` for `one-test-boot.sh`, whose 201 MB initramfs panics `NoSpace` at 1024 (§6.77b) |
| A full task table takes the machine with it | `alloc_slot` panicked where Linux answers `EAGAIN` at its own limit (measured `build/scratch/forklimit/p.c`: `forked 196 children before it refused: errno=11`, and a fork after reaping them all works again). It answers `Option` now and every caller answers the errno; `t-fork-limit` is the pin and the panic's counterfactual kills the machine before the first claim prints (§6.81, item 198). What made the limit reachable inside one lane's sweep was the table's width, not a leak - `busybox timeout`'s watchdog child lives out each program's budget (§6.84, item 200) |
| A fixed-size store with an eviction path silently drops what it holds | The store of uncollected deaths held 32 records while the table has 64 slots, and dropped the oldest — so `wait` answered `ECHILD` for ever and a caller parked on a missing record was never released (measured: `heard 32 of 40 deaths`, Linux `40 of 40`; `docs/notes/` §6.82). A cap must be the table's width when one record per slot is the most that can be owed. And the pin for it must not fork all its subjects at once: space them so each frees its slot, or a lane crowded by the leak above reddens the *precondition* instead |
| `cargo xtask test` reports **"the kernel's host unit tests failed"** with no failing test named | **Diagnosed and fixed 2026-11-06** — stage 1 runs in a thread beside the first arm's sweep and `run_captured` keeps only the last 40 lines, so what reached the gate was warnings plus `error: test failed, to rerun pass -p mpos --lib`. The red was `sched::futex::tests`: one module-global `WAITERS` map, two claims about the *same* two words (`0xa000`/`0xb000`), so two test threads saw each other's waiters (`13/20` green without the lock, `20/20` with it). The module now takes `crate::sched::test_lock::take()` per test — and run a suite **more than once** before calling its green stable (`docs/notes/` §6.87) |
| A sweep's lane count is **discovered**, and on a pinned container it is one | `xtask` asks `available_parallelism()` (`xtask/src/selftest.rs:1303 fn lane_count`), so a container restricted to a single CPU (`Cpus_allowed_list: 0`) runs **every** program in one lane in one boot: `busybox timeout`'s watchdog child holds a task slot for the program's whole budget, 262 programs fill the table, and eighteen of them redden with `the machine has room for more than a hundred tasks at once (71 made)` - a clean-looking kernel red with no kernel cause. Six lanes on that same one core is the other end: the scheduler's own timing cases fail (`mpos-kunit 32 passed, 3 failed`, `sched.alternating` read `[0, 1, 0, 0, 0, 1, 0, 1, 1, 1]`), because six TCG guests share one core. Neither is a reading. Run `nproc` first and pass `MPOS_TEST_LANES=<3..6>` explicitly, and treat a red whose claims are about *room* or *timing* as a statement about the host until the lane count is known (`docs/notes/` §12.4a, 2026-11-06) |
| `/tmp` and `/var/tmp` are not writable here | A log written to `/tmp` silently does not exist afterwards, and QEMU's `-drive …,snapshot=on` needs a temp dir: every aarch64 lane died on `Could not open temporary file '/var/tmp/vl.…': Read-only file system`. Write logs under `build/` and run the gate as `mkdir -p build/tmp && TMPDIR=$PWD/build/tmp cargo xtask test` (§6.77b) |

---

## 6. Git conventions

Subjects follow the log: `type(scope): what changed` — types `feat`/`fix`/`docs`/`refactor`; scopes
are subsystems (`vfs`, `sched`, `net`, `drm`, `ext4`, `syscall`, `xtask`, `console`, `input`);
multi-subsystem commits list them. The **body carries the why**: what was observed, measured,
rejected — this history *is* the design record, so a commit that only restates the diff is out of
place. `git log` the file before you change it. The review question every change gets: *is that
Linux's answer, or a shape that looks like it?* Something that cannot be Linux's today says so in
its errno and takes a named `docs/ROADMAP.md` gap (its measurement in `docs/notes/`) in the same commit (§3). Never commit `build/`,
`target/`, `build-test/` or anything under `.workbuddy/`, `.qoder/`, `.codely-cli/` (all
gitignored). Never push to a remote unless explicitly asked.

---

## 7. Where the plans live

- `docs/ROADMAP.md` — the plan: the phase-by-phase roadmap, the status board at the top,
  "Suggested next moves", what is open and in which order. What to do next lives here.
- `docs/notes/` — the evidence: the ledger of each gate run, performance change and lesson
  *with its measurement*, and the reasoning behind every divergence the roadmap names.
- `docs/uapi/` — the userspace surface as an inventory (syscalls, ioctl families, layouts, flag
  vocabularies), each row carrying Linux's answer, this kernel's, the owning module and the pin —
  the working list of *served* / *honestly refused* / *wrong shape* (only the third is work).
- `docs/ARCHITECTURE.md` §9 — the debt audit, mirrored in `docs/notes/` Phase 11 with an ordered
  remediation plan.
- The same-commit rule for the board, the uapi row and the notes is §4's syscall recipe step (6) and
  §6; it binds every change, not only syscalls. The board is only useful if it is true.
