# AGENTS.md

How to work in this repository. Read this before your first edit. The other docs split the rest:
`README.md` = what MPOS *is*; `docs/ARCHITECTURE.md` = how it is put together, the module graph and
where the layering is violated; `docs/PLAN.md` = the roadmap, the measured status of every phase,
the ledger of every gate run and measurement, and the home of any new plan; `docs/UAPI.md` = the
userspace-visible surface as one row per promise. This file is only **how to work here**, and points
at those rather than repeating them.

---

## 1. Repository map

| Path | What lives there |
|---|---|
| `src/` | kernel crate `mpos`: `lib.rs` (host-testable) + `main.rs`, the one architecture-independent entry and panic handler, over `src/board/`, which holds that machine's boot wiring, its devices and its milestone checks — one body per machine (`src/board/<arch>.rs`, chosen in one place) plus the shared half both bodies call: `src/board/steps.rs` (the five steps every body's `init` runs: scheduler, ramfs root, unpack, console arming, pid-1 handoff, plus the one exit-hook registry and the net bring-up), `src/board/report.rs` (the boot banner, the boot-stack canary check, `park`, the stack-use report and the panic walk) and `src/board/checks/` (the neutral milestone checks, `PLAN.md` §13.21/§13.23). The bodies live here and not in `mpos-arch` because what is left in them names `vfs`/`blk`/`net`/`drivers`, and the shared half lives here rather than in the library because arming the console and reading `kernel_sections` name `crate::arch::`, which only the wiring may do |
| `arch/` | `mpos-arch`, the machine layer: `hal/` (the capability traits, their forwards, and `hal::kernel`'s table of kernel services), `arch/{x86_64,aarch64}/` (the two ports, their linker scripts, and everything that *is* the machine), and `early/` (the same two machines as the *bootloader* sees them — descriptors, wire, jump — the one tree compiled for `-unknown-uefi` rather than `-unknown-none`, `PLAN.md` §14.6). A crate so that the machine layer cannot name the kernel at all (§3); `src/lib.rs` re-exports `{arch, hal}` so the kernel's paths are unchanged. Deliberately not under `crates/` — that directory means "host-testable leaf" and this one cannot be compiled for the host |
| `crates/` | downward-only leaf crates (§3): `err`/`abi`/`pci`/`date`/`binfmt`/`cred`/`uts`/`seccomp`/`term`/`ahci`/`gpu`/`evdev`/`uinput`/`blk`/`fdt`/`bootinfo`/`vm` |
| `boot/` | `mpos-boot`, the UEFI loader — its own target (`-unknown-uefi`), no kernel dep, no architecture in it at all (the three machine things it needs are `mpos-arch`'s `early/`). Since 2026-10-06 it is a **library plus a thin `#[entry]` binary**: `src/lib.rs` (`mpos_boot::run`) holds the whole sequence and `src/main.rs` is the firmware entry that calls it, so the kernel can become the second caller (`PLAN.md` §14.7). The handoff record is one definition in the `crates/bootinfo` leaf, read by both machines and both sides of the seam |
| `xtask/` | `mpos-xtask`, the build/run/test front-end; `xtask/README.md` documents every command |
| `user/` | `mpos-user` (ordinary `std` programs); `user/abitest/` = the `no_std` conformance suite (`src/bin/syscall/` one family, `src/bin/integration/` end-to-end); `user/tests/` = `std`-side tests; `user/staging/` = scratch, wired into no build |
| `fuzz/` | `mpos-fuzz`, **its own workspace**: libFuzzer targets over the kernel's parsers |
| `rootfs/`, `rootfs.toml` | the x86_64 guest image tree and its packages/disk geometry; `rootfs-aarch64.toml` + `rootfs-aarch64/` are the second machine's pair — same parser, different userland and its own `/init` overlay |
| `initramfs/`, `initramfs.toml` | the early-userspace tree the x86_64 boot unpacks as its first root, and its packages; `initramfs-aarch64.toml` + `initramfs-aarch64/` are the second machine's pair, built by the **same pipeline** (`xtask/src/initramfs.rs`) — the machine picks the toml, the overlay and the applet farm, and nothing else |
| `machine.toml` | what the image is *booted* on — memory, cpus, devices, run mode, and the second machine's own values under an `aarch64_` prefix in the same file. Host-side only, so editing it never reinstalls |
| `tests.toml` | which test programs the sweep runs by default, and its groups |
| `build/`, `target/`, `build-test/` | build output and scratch. Never edit, never commit |

Root `Cargo.toml`: `default-members = ["."]` is load-bearing — without it a plain `cargo build`
would build the musl user programs for the host.

---

## 2. Build, run, verify

Prereqs: nightly with `rust-src rustfmt clippy`; targets `x86_64-unknown-none`, `-uefi`,
`-linux-musl`, `-linux-gnu`, plus `aarch64-unknown-none` and `-uefi` (both named in
`rust-toolchain.toml`). For `run`/`disk`/`rootfs`: `qemu-system-x86_64`, OVMF, `qemu-img`/`qemu-nbd`,
`sfdisk`, `partx`, `blkid`, `mkfs.ext4`, `mkfs.fat`, `tar`, `sudo`. For the aarch64 side:
`qemu-system-aarch64`, AAVMF, and `aarch64-unknown-linux-musl` so the verdict can build and `execve`
an unmodified static-musl helper. `rootfs`/`initramfs`/`alpine-aarch64` fetch Alpine packages over
the network; `alpine-aarch64` also needs `fakeroot`.

| Command | What it does | Root? |
|---|---|---|
| `cargo xtask build` [`--arch x86_64\|aarch64`] | kernel + UEFI loader, **for the machine `--arch` names** — the switch drives both `--target`s (`x86_64\|aarch64-unknown-none` and `-unknown-uefi`), not just which image gets booted | no |
| `cargo xtask all` | kernel, initramfs, disk image, root filesystem. **x86_64 only**: these assemble *that machine's* GPT disk, so `--arch aarch64` is refused by name rather than built as the other machine; the second machine's disk is laid out by `boot-aarch64`/`alpine-aarch64`/`run --arch aarch64` | yes |
| `cargo xtask run` [`--mode terminal\|gui\|mirror`] [`--arch x86_64\|aarch64`] | boot in QEMU, rebuilding stale bits. `--mode` picks the window backend and the guest's `console=` list together. `--arch aarch64` boots the second machine to its **own shell** on the serial console — pid 1 is an in-tree `/init` that makes `/tmp` and `execve`s `/bin/sh` (the multi-call `/bin` image `xtask/src/aarch64_bin.rs` builds) | when needed |
| `cargo xtask test` | **the gate**: host unit tests, then every user test program booted in QEMU. `--arch aarch64` is the second machine's own arm, read with the same `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol; its program set is `tests.toml`'s `aarch64` group's answer, and the arm prints every one by group and name — a green summary means "every program this machine was supposed to run passed", never a silent skip. Neither arm can silently run the other machine | no |
| `cargo xtask depgraph` | measure the module graph against the Phase-11 ratchet; fails when an edge appears or a count grows | no |
| `cargo xtask boot-aarch64` | **the second machine's boot verdict**: build the aarch64 kernel and loader, lay out an ESP, boot under AAVMF, read the marker chain off the PL011 (`M3_TABLES_OK` … `INIT_EXITED`). The chain's meaning, every stage's measurement and the terminator rule live in `docs/PLAN.md` §9.1 and §13 | no |
| `cargo xtask alpine-aarch64` | **the second machine's real userland**: install a genuine Alpine `aarch64` tree under `fakeroot` onto its bare ext4 and boot it headless — a green run ends with Alpine's own `getty` printing `localhost login:`. No initramfs; the disk root is the whole userland. `docs/PLAN.md` §13.12 | no |
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

**Three build traps.** (1) **`--target` is mandatory for host work**: `.cargo/config.toml` pins
`[build] target = "x86_64-unknown-none"`, so a bare `cargo test` builds for bare metal and cannot
run. (2) **`user/abitest` uses overridden flags**: its profile uses the *kernel's* linker script, so
the gate builds it with `RUSTFLAGS=-C relocation-model=static` from inside `user/abitest/`; by hand
without that yields binaries the loader rejects. (3) **The aarch64 target links in `--release`
only**: a debug build fails with `relocation R_AARCH64_ABS64 …` from the *sysroot's* `liballoc`, and
no gate notices — anything hand-verifying an aarch64 change must pass `--release`.

**The self-test protocol.** Stage 1 = host unit tests. Stage 2 = builds both test suites, packs
them into a throwaway initramfs with a generated `/init`, boots it headless with `mpos.selftest`,
and parses the serial log for `=== RUN/PASS/FAIL t-name` / `=== SUMMARY pass=N fail=N` / `=== END`.
The run must end with a summary **and** QEMU exit code 33 (`isa-debug-exit`). A test that hangs,
crashes or never prints `=== PASS` counts as failed — no `#[ignore]` escape hatch; a test that
cannot work is deleted or made to pass.

**`tests.toml` picks which programs run.** All are built; grouped ones are left of the default
sweep. Groups: `external` (needs the host network or a listener), `slow` (measurement/seconds),
`diagnostic` (not a check at all). `--groups <a,b>` replaces the file's default; `--all-groups` runs
everything; an undefined group name is an error — a sweep that quietly runs fewer tests than asked
is the one failure this must not have. A new test lands with a group decision; silence is the
default. Nothing a test runs can leak into the image `cargo xtask run` boots (test artefacts live
under `build/`).

**Reporting the gate — never from a stale run.** This file carries **no test counts**: a snapshot
here goes stale within a commit or two. The measured state lives in `docs/PLAN.md` (its latest §12.4a
summary line; every phase records what it measured). Re-run the gate and quote the summary line +
exit code — or say plainly it did not get there.

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
   `docs/PLAN.md` with its measurement and missing prerequisite. A `TODO` in shipped code, a handler
   that returns 0, and a shape that looks finished while doing less are the same violation.
6. **A gap gets a pin, or it is not done.** Host test, `t-*` program, or both. "Works, verified by
   hand once" is how this repo lost the same bug four times.

### Five standing checklist rules (before anything lands)

1. **Kernel-space discipline.** What Linux runs in userspace is not this kernel's job — it provides
   the ABI userspace expects and nothing more.
2. **High cohesion, low coupling.** Before adding a feature, locate the module that owns the
   concept. A change reaching into another module's internals is in the wrong module.
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
   joins the targets in `fuzz/` in the same commit, or `docs/PLAN.md` says why not.
3. **A published ABI is append-only.** Syscall numbers, struct layouts and ioctl encodings are
   contracts once shipped. Renumber/repack/reinterpret only with an explicit migration in `docs/PLAN.md`.
4. **Worst-case rule.** The global allocator still panics on failure (`docs/PLAN.md` §8.0), so until
   that closes, any new path whose allocation volume a user can steer carries a bound — capped
   count, checked length, or `ENOMEM` at the limit.
5. **Interrupt context does none of: sleep, allocate, walk an unbounded structure.** Bounded work,
   no lock a preempted task may hold, defer the rest. On one core, spinning on a preempted holder is
   a deadlock with the machine stopped on it.
6. **A performance change ships with before/after numbers.** `docs/PLAN.md` §10 is only true if
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
  whole tick on every wake (`docs/PLAN.md` §10.2).

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
(6) update `docs/PLAN.md` ("Remaining ABI support" + TODO board) and `docs/UAPI.md` — a call moving
between served / refused / wrong-shape belongs in both.

**Add a user test program.** `std`/`rustix` → `user/tests/src/bin/t-<name>.rs` (flat,
auto-discovered) — the shape a new test belongs in, because it runs on **both** machines and takes
its per-claim printing and exit status from the crate's own `Report` (`user/tests/src/lib.rs`)
instead of a local copy. `no_std` raw-syscall → `user/abitest/src/bin/{syscall,integration}/t-<name>.rs`,
which is where the *unmigrated* half of the ABI suite lives: `PLAN.md` §15.11 moves it to the std side
a batch at a time, so a new test does not belong there. **Add an explicit `[[bin]]` block to
`user/abitest/Cargo.toml`** — Cargo does not auto-discover `src/bin/<subdir>/*.rs`; the std side needs
none. Print `ok:`/`FAIL:` per check and a final `PASS`/`FAILED` (`Report::finish` does both). The
no_std harness is `user/abitest/lib/lin.rs`. Keep the `t-` name (the test `/init` globs `/bin/t-*`).
Decide its group in `tests.toml` — and a name in a machine's own group must be a program that machine
actually builds (§5's trap).

**Add a kernel unit test.** A `#[cfg(test)] mod tests` at the bottom of the file it tests, with the
parent module compiled for the host (§3). Assert on the contract (value, errno, count), not a log
line; prefer a test that would fail if the policy differed.

**Change the guest image.** A file that must exist → the `rootfs/` (or `initramfs/`) tree (overlays
apply *after* anything the toml writes). A package → `rootfs.toml`/`initramfs.toml`. Partition /
fs-type / image-size / hostname / DNS → `rootfs.toml`. Anything about the *machine* (memory, cpus,
`vsock`, `audio`, window backend, run mode, extra QEMU args) → `machine.toml`, **never**
`rootfs.toml` — `rootfs.toml`'s mtime decides whether the Alpine tree is reinstalled, so a machine
key there charges a full package install for a memory size (`xtask/src/config.rs`'s
`the_machine_and_the_image_keep_their_own_keys` fails if one drifts back). Boot wiring
(`mpos.selftest`, the `console=` list) is read via `src/boot/console_list.rs`; the kernel is told
which consoles it has and nothing about how it was launched.

**The second machine's userland is its own pair** (`rootfs-aarch64.toml` + `rootfs-aarch64/`;
`docs/PLAN.md` §13.12): a package goes in the toml, a file in the overlay, both installed by
`cargo xtask alpine-aarch64`. One caveat worth repeating: `fakeroot` must wrap **`mkfs.ext4` too**,
not only `apk` — `mke2fs -d` stats each file, so an unwrapped `mkfs` writes a root filesystem owned
by the build user.

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
| Bare `cargo test` won't link/run | Always `--target x86_64-unknown-linux-gnu` |
| A new test program silently never built | Add the `[[bin]]` block; `xtask` errors on "no binary" only for sources it finds |
| A `src/` file nothing declares looks real | A `mod` declaration makes a file part of the build; missing ones left duplicate serial/RTC drivers for months (V-7). Check for the declared one before writing a new file |
| A module has a host body and a machine body | **Both move, or it does not build.** Run the bare-metal target on every pass (V-11: the host suite was green while `x86_64-unknown-none` failed on a non-exhaustive `#[cfg]` arm) |
| A flag or struct constant looks shared but the machines disagree | It is the machine's. `open(2)`'s `O_*` words differ on arm64 and the overlap bit the session-costing `ELOOP` bug (§13.13); `struct stat`/`statx` and the signal frame were the same class. Read **the machine's** uapi and pin both machines (`t-open-flags`) |
| `cargo xtask run` prints no milestone markers | By design — `M*_OK` checks run only under `mpos.selftest`. The aarch64 machine's own marker chain is its boot's verdict and comes from `boot-aarch64`; its full meaning is `docs/PLAN.md` §9.1 |
| `cargo xtask test` stage 1 goes red in `rand` and nowhere else | A **flake** — re-run before believing it and quote the green run. Measurement, mechanism and fix directions: `docs/PLAN.md` §12.4a |
| Stage 2 goes red on `t-clock-gettime`'s "two reads never go backwards" | A **flake** of the same kind, in the wall clock rather than in a service: `CLOCK_REALTIME` is the CMOS second plus a tick-derived fraction sampled around it, so two successive reads can step back. Re-run before believing it; the measurement and why it is not this class's subject are in `docs/PLAN.md` §13.27 |
| A recorded measurement is about the machine that made it | A number already written down is not a measurement of *this* question. The tell is a claim that made a refusal permanent without saying which register, program or run — re-measure before building on it (§13.15) |
| A driver holds the transport *inside* its device struct | The crate's `Transport` is **not object-safe** (`begin_init<F: Flags>` is a generic method and `VirtQueue::new`/`add_notify_wait_pop` take `&mut impl Transport`), so `Box<dyn Transport>` does not exist and such a device cannot be one concrete `static`-held type shared by two machines. Two shapes work, and the error count does not tell them apart: open the device inside the driver module and hand the rest only a contract (`NetDevice`, `ConsolePort`, `InputDevice`), or make the device generic and let its *consumer* hold a `Box<dyn YourDevice>` (`Gpu`/`GpuDevice`, §13.26 — 3 errors either way). |
| A machine declares a capability and still has no device behind it | A capability flag makes a port cheap to complete **only if everything behind it is gated on the capability and not on the architecture**. Declaring `mpos_input_registry` for aarch64 cost **1** error once the modules, the `/dev` block and the exit hooks asked it (§13.23); it cost **4** while a `#[cfg(target_arch = "x86_64")]` module gate hid behind it, and the stale figure sat in the plan until it was re-measured (§13.24). The other half of the same trap: opening a device, proving the transport and dropping it is a milestone, not a driver — such a port has no device to read, only a proof that one answered (§13.24) |
| An aarch64 `M*_OK` check runs in front of the machine's own `init` | The checks run on every aarch64 boot, including `alpine-aarch64`. A check that changes shared state must put it back or it damages the machine it is checking (§13.14: a wall-clock check left the year 36812 and killed `syslogd`) |
| `TERMINATORS` in `xtask/src/aarch64.rs` is a list of *last* markers | It holds the last check's success marker and every check's failure marker — never the success marker of a check another check comes after. A wrong one kills the boot seconds in, indistinguishable from a hang. Full history and the elapsed-time tell: `docs/PLAN.md` §9.1 |
| A `static` with a zero initializer and no writer lands in `.rodata` | Read-only, on a machine whose page tables enforce it — and it bites whatever intended to write it (stage 1d's arena, stage 2's task stack). The fix is frames from the allocator, not a section attribute; `-zmax-page-size` will not save you (`docs/PLAN.md` §9.1 row 1d) |
| An assembly block that switches section switches it for every later block | Every `global_asm!` in a crate is one assembly unit; a block that changes section without restoring moves everything assembled after it, visibly only in the linker map. The symptom is a faulting fetch on a prologue that reads as "code in `.rodata`". Three defences (`.pushsection`/`.popsection`, naming the section in every code block, the linker `ASSERT`s) and the measurement: `docs/PLAN.md` §13.16 |
| An aarch64 exception entry that can switch tasks does not save `ELR_EL1`/`SPSR_EL1` | AArch64 does not push the interrupted `PC`/`PSTATE` on the interrupted stack the way x86 pushes its return frame: they go to `ELR_EL1`/`SPSR_EL1`, one pair per core, and any other task's trap overwrites them. The kernel's IRQ stub calls `preempt`, so it switches by construction; a trap from EL0 needs `SP_EL0` as well. Both stubs save their resume state on their own stack because of this, and the symptom of forgetting is the kernel resuming into another context's `PC` — measured as an instruction abort on a data address: `docs/PLAN.md` §13.21/§13.22 |
| No cross-architecture build | aarch64 boots the way x86_64 does — firmware, ESP, higher half, its own tables, EL0 address spaces — and the two arms share nothing but source layout. What compiles where, the three measured boot choices (`acpi=off`, `force-legacy=false`, device RAM cost) and the port's current reach: `docs/PLAN.md` §9.1/§13 |
| `fuzz/` won't build from the root | It is its own workspace; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test never runs | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; a host test inside it is unreachable |
| `user/staging/` looks like a missing test | Deliberate scratch — check `user/abitest` and `user/tests` for the live version first |
| Two `cargo xtask test` runs in one tree report nonsense | They share `build/test-*` artefacts and one reads a log the other is writing. Trust the log's own summary + `=== END`, re-run alone |
| The two sweeps do not build the same programs | x86_64's arm builds **both** suites (169: the bare-metal `user/abitest` plus `user/tests`) and aarch64's builds **only** the std suite (30). So "every name `tests.toml` declares must be a program this arm built" has two scopes, and `tests.toml`'s `external`/`slow`/`diagnostic` groups name programs only the first machine carries. Sharing the check refuses every aarch64 sweep - measured, on exactly `t-sh, t-dns, t-rawicmp, t-tcp-close, t-fill-ramfs, t-forkcost, t-netlat, t-pipecost`. Each arm asks its own question (`xtask/src/sweep.rs`'s `select` records why) |
| The test machine ≠ the run machine | `xtask test` boots an initramfs on its own QEMU wiring; `xtask run` boots the disk image on `machine.toml`'s. A device only one has is a shape no test sees — name a driver's "controller with nothing on it" behaviour on the machine that has one before calling it measured (§7.3's 28 s empty-controller boot) |
| A device class needs a *host* backend, not just a QEMU device | vsock is the one virtio device QEMU does not emulate in userspace: `vhost-vsock-*` is a host kernel module, and a machine naming it without `/dev/vhost-vsock` refuses to start. So such a device is attached only where the host can back it, its success marker is required only there, and the skip is **printed** - a verdict that quietly ran one check fewer is the failure `tests.toml`'s groups exist to prevent. Give the device a CID distinct from every other machine's (the aarch64 boot takes `4`, the run machine `3`): a second guest claiming a live CID aborts QEMU before the guest starts (§13.28) |
| An `M*_OK` marker's own summary can outlive the class's condition | the prose `xtask` prints when every required marker was seen is a *claim*, and a conditional marker makes it a conditional claim: §13.28's summary line is built from the same `vsock_available()` decision that picks the marker, so it says "seven devices" and omits `M8_VSOCK_OK` on a host that could not back the device. A summary hard-coded to the host that wrote it is a lie on the host that reads it |
| A driver compiles, is host-tested, and **nothing calls it** | the shape that hid the 9p share for months: `fs9p::init_global` filled the registry every mount reads and was called from nowhere, so `devices()` was always empty and every boot printed "no share on the bus" while QEMU exported one. A probe nothing invokes is not a working feature, and a *complete* driver plus a green host test is exactly what hides it - the tests pin the parser, not the wiring. The tell is a boot log line that never changes: grep for the line a device is supposed to print and check whether any commit ever changed it (§13.29) |
| A wire client that has never run against a real server | the 9p client implemented plain 9P2000, which this host's QEMU 11 answers `"unknown"` to - so even with the probe wired it could never attach, and its host tests (against hand-built records) could not know. A protocol client's *own* fixtures are not a measurement of the server it talks to: ask the real one. The measurement is one boot printing the dialect each side agrees on, and the fix is a dialect the server serves, not a negotiation loop that hides which one it got (§13.29) |

---

## 6. Git conventions

Subjects follow the log: `type(scope): what changed` — types `feat`/`fix`/`docs`/`refactor`; scopes
are subsystems (`vfs`, `sched`, `net`, `drm`, `ext4`, `syscall`, `xtask`, `console`, `input`);
multi-subsystem commits list them. The **body carries the why**: what was observed, measured,
rejected — this history *is* the design record, so a commit that only restates the diff is out of
place. `git log` the file before you change it. The review question every change gets: *is that
Linux's answer, or a shape that looks like it?* Something that cannot be Linux's today says so in
its errno and takes a named `docs/PLAN.md` gap in the same commit (§3). Never commit `build/`,
`target/`, `build-test/` or anything under `.workbuddy/`, `.qoder/`, `.codely-cli/` (all
gitignored). Never push to a remote unless explicitly asked.

---

## 7. Where the plans live

- `docs/PLAN.md` — roadmap phase by phase, TODO board at the top, "Suggested next moves". Also the
  ledger of each gate run and performance change *with its measurement*, and the home of any new plan.
- `docs/UAPI.md` — the userspace surface as an inventory (syscalls, ioctl families, layouts, flag
  vocabularies), each row carrying Linux's answer, this kernel's, the owning module and the pin —
  the working list of *served* / *honestly refused* / *wrong shape* (only the third is work).
- `docs/ARCHITECTURE.md` §9 — the debt audit, mirrored in `docs/PLAN.md` Phase 11 with an ordered
  remediation plan.
- If you implement something, update `docs/PLAN.md`'s status — and `docs/UAPI.md`'s row, when a call
  or request moves between the three verdicts — **in the same commit**. The board is only useful if
  it is true.
