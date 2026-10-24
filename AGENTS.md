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
| `cargo xtask test` [`--arch x86_64\|aarch64`] | **the gate**: host unit tests, then every user test program booted in QEMU. Both machines run the *same* sweep protocol (below); only where the payload sits differs. Neither arm can silently run the other machine | no |
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
| `cargo xtask test` goes red on `t-posix`'s "getpriority answers 20" and nowhere else | A **flake** (observed once, 2026-10-23, on a commit that touches no priority path; the re-run was green). Re-run and quote the green one — the mechanism is not established, so it is an observation rather than a diagnosis (`docs/notes/` §6.18) |
| A lane times out at 900 s with `t-ns-pid`'s last line being "while a fork into the namespace whose init has exited is ENOMEM" | **An observed hang, not a diagnosis** (twice on 2026-10-23, in runs whose surrounding programs differed, while the same program passed on the runs between). The timeout costs ~15 minutes of wall clock, so re-run and quote the log's own `=== SUMMARY` line rather than the elapsed time; the section it stops in waits on a pipe from a forked child, which is where to look first (`docs/notes/` §6.22 records the two readings). **Since 2026-10-23 that read is bounded and the section marks six stages**, so the failing claim reads `the init section ran to its end (last stage N)` and N says where it stopped - a stage below 6 means the section itself stalled |
| The aarch64 sweep freezes mid-sweep — `t-clone-exit` at its zombie poll, or `t-busybox-farm` — with QEMU at ~99 % host CPU and the serial log silent for ever | **An observed hang, not a diagnosis** (measured on 2026-10-24 on HEAD with the tree stashed as well as on working trees; the serial-silent log offsets repeat byte-for-byte per program). `git bisect run` names `4081466` (§10.7's wait-deadline landing) as the first bad commit — **but run the bisect with hang detection and a stale-QEMU kill per step**: a timeout-killed step leaves its guest holding vhost-vsock CID 4, every later boot aborts before starting (`unable to set guest cid`), and the script reads that as "bad". QMP reads at the freeze show the timer compare expired for seconds with no tick taken, and in one window the guest executing nothing at all while QEMU burned a host core; a Linux-shaped re-arm fix (re-arm the compare from the counter's current reading) was built, validated clean on both machines, and **did not stop the freeze** — reverted. Evidence, the voided-bisect lesson, and the ordered next steps: `docs/notes/` §6.23. Until it closes, an aarch64 gate claim says plainly that its sweep produced no summary |
| A `src/` file nothing declares looks real | A `mod` declaration makes a file part of the build; missing ones left duplicate serial/RTC drivers for months (V-7). Check for the declared one before writing a new file |
| A module has a host body and a machine body | **Both move, or it does not build.** Run the bare-metal target on every pass (V-11: the host suite was green while `x86_64-unknown-none` failed on a non-exhaustive `#[cfg]` arm) |
| A flag or struct constant looks shared but the machines disagree | It is the machine's — `open(2)`'s `O_*` words and the overlap bit (§13.13's session-costing `ELOOP` bug), `struct stat`/`statx`, the signal frame. Read **the machine's** uapi and pin both machines (`t-open-flags`) |
| `cargo xtask run` prints no `mpos-kunit` lines | By design — the boot's own suites are in the *test* build alone (`--features selftest`, which only `cargo xtask test` builds). A session's wire shows the machine's own boot log and nothing else |
| `cargo xtask test` stage 1 goes red in `rand` and nowhere else | A **flake** — re-run before believing it and quote the green run. Measurement, mechanism and fix directions: `docs/notes/` §12.4a |
| Stage 2 goes red on `t-clock-gettime`'s "two reads never go backwards" | A **flake** of the same kind, in the wall clock rather than in a service — two successive `CLOCK_REALTIME` reads can step back. Re-run before believing it; the mechanism and measurement: `docs/notes/` §13.27 |
| A recorded measurement is about the machine that made it | A number already written down is not a measurement of *this* question. The tell is a claim that made a refusal permanent without saying which register, program or run — re-measure before building on it (§13.15) |
| A driver holds the transport *inside* its device struct | The crate's `Transport` is **not object-safe** (`begin_init<F: Flags>` is generic; `VirtQueue::new`/`add_notify_wait_pop` take `&mut impl Transport`), so `Box<dyn Transport>` cannot exist and such a device cannot be one `static`-held type shared by two machines. Two shapes work, and the error count does not tell them apart: open the device inside the driver module and hand the rest only a contract (`NetDevice`, `ConsolePort`, `InputDevice`), or make the device generic and let its *consumer* hold a `Box<dyn YourDevice>` (`Gpu`/`GpuDevice`, §13.26 — 3 errors either way). |
| A machine declares a capability and still has no device behind it | A capability flag makes a port cheap **only if everything behind it is gated on the capability, not the architecture** — an `#[cfg(target_arch)]` module gate hiding behind the flag multiplied the error cost (§13.23, re-measured as §13.24's lesson that a stale figure sits in the plan). And opening a device, proving the transport and dropping it is a milestone, not a driver (§13.24) |
| A check on an aarch64 device runs in front of the machine's own `init` | The device checks (`check_blk`, `check_fs`, …) run on **every** aarch64 boot, a session included, and halt the machine on failure (§13.14: a wall-clock check left the year 36812 and killed `syslogd`). A check may assert only what the **kernel itself** produces from the configured devices — never a file or byte pattern the build packed, or an image merely missing one becomes a machine that cannot boot (`docs/notes/` §15.34). Two consequences bite: the boot context has **no task identity** (`sched::current_owner()` panics — `there is no task 63`), so it cannot create a file or take a lock naming an owner; and a check that does change shared state must put it back |
| A `static` with a zero initializer and no writer lands in `.rodata` | Read-only, on a machine whose page tables enforce it — and it bites whatever intended to write it (stage 1d's arena, stage 2's task stack). The fix is frames from the allocator, not a section attribute; `-zmax-page-size` will not save you (`docs/notes/` §9.1 row 1d) |
| An assembly block that switches section switches it for every later block | Every `global_asm!` in a crate is one assembly unit, so an unrestored section change moves everything assembled after it — the symptom is a faulting fetch on a prologue that reads as "code in `.rodata`". The three defences and the measurement: `docs/notes/` §13.16 |
| An aarch64 exception entry that can switch tasks does not save `ELR_EL1`/`SPSR_EL1` | AArch64 keeps the interrupted `PC`/`PSTATE` in those registers (one pair per core, plus `SP_EL0` for an EL0 trap), not on the interrupted stack — and the IRQ stub preempts by construction, so any other task's trap overwrites them. Both stubs must save their resume state on their own stack; forgetting reads as the kernel resuming into another context's `PC`. The measurement: `docs/notes/` §13.21/§13.22 |
| No cross-architecture build | aarch64 boots the way x86_64 does — firmware, ESP, higher half, its own tables, EL0 address spaces — and the two arms share nothing but source layout. What compiles where, the three measured boot choices (`acpi=off`, `force-legacy=false`, device RAM cost) and the port's current reach: `docs/notes/` §9.1/§13 |
| `fuzz/` won't build from the root | It is its own workspace; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test never runs | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; a host test inside it is unreachable |
| Two `cargo xtask test` runs in one tree report nonsense | They share `build/test-*` artefacts and one reads a log the other is writing. Trust the log's own summary + `=== END`, re-run alone |
| The two sweeps do not build the same programs | They do now — §15.11's last batch finished the migration and deleted `user/abitest`, so both arms build the same std suite — but the counts moved repeatedly, so read them off a run, never from here. "Every name `tests.toml` declares must be a program this arm built" is still two questions: the first machine's sweep asks it of the whole file, the second of its own `gaps`. A `[default]` group's reason is the same on both machines, so a name in one is a program both arms build — the narrower question is not wrong, only smaller (`xtask/src/sweep.rs`'s `select` records why) |
| The test machine ≠ the run machine | `xtask test` boots an initramfs on its own QEMU wiring; `xtask run` boots the disk image on `machine.toml`'s. A device only one has is a shape no test sees — name a driver's "controller with nothing on it" behaviour on the machine that has one before calling it measured (§7.3's 28 s empty-controller boot) |
| A device class needs a *host* backend, not just a QEMU device | vsock is the one virtio device QEMU does not emulate in userspace — `vhost-vsock-*` is a host kernel module, so a machine naming it without `/dev/vhost-vsock` refuses to start. Attach only where the host can back it (`qemu::virt_machine_args` asks), and print the absence rather than dying of it. Give each guest a distinct CID (the two machines take `3` and `4`): a second guest claiming a live CID aborts QEMU before the guest starts (§13.28) |
| A driver compiles, is host-tested, and **nothing calls it** | The shape that hid the 9p share for months: `fs9p::init_global` filled the registry every mount reads and was called from nowhere, while every boot printed "no share on the bus". A complete driver plus green host tests is what hides it — they pin the parser, not the wiring. The tell is a boot log line that never changes: grep for it and check whether any commit changed it (§13.29) |
| A wire client that has never run against a real server | The 9p client implemented plain 9P2000, which this host's QEMU 11 answers `"unknown"` to — its hand-built fixtures could not know. A protocol client's own fixtures are not a measurement of the server it talks to: ask the real one, and fix toward the dialect it serves rather than a negotiation loop that hides which one you got (§13.29) |

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
