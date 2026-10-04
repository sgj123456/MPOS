# AGENTS.md

How to work in this repository. Read this before your first edit. The other docs split the rest:
`README.md` = what MPOS *is*; `ARCHITECTURE.md` = how it is put together and where the layering is
violated; `PLAN.md` = the roadmap, the measured status of every phase, and the home of any new plan;
`UAPI.md` = the userspace-visible surface (syscalls, ioctls, layouts, flag vocabularies) as one row
per promise. This file is only **how to work here**, and points at those rather than repeating them.

---

## 1. What this is

A uniprocessor x86_64 kernel in Rust (`no_std`, edition 2024), binary-compatible with the Linux
x86_64 ABI, so unmodified musl binaries (busybox, `apk`, weston) run on it. It boots on UEFI via its
own loader (`mpos-boot`). The central split: the kernel is a **library** that also compiles for the
host (so arch-independent logic is unit-testable with `cargo test`), plus a thin binary holding only
what must run on the machine. Everything is driven by `cargo xtask`, and every behaviour is meant to
be pinned — host unit test, a program booted in QEMU, or both.

---

## 2. Repository map

| Path | What lives there |
|---|---|
| `src/` | kernel crate `mpos`: `lib.rs` (host-testable) + `main.rs`, a thin arch dispatcher over `src/main/<arch>.rs`, which holds that machine's boot entry, milestone checks and panic handler |
| `crates/` | downward-only leaf crates (see §4): `err`/`abi`/`pci`/`date`/`binfmt`/`cred`/`uts`/`seccomp`/`term`/`ahci`/`gpu`/`evdev`/`uinput`/`blk` |
| `boot/` | `mpos-boot`, the UEFI loader. Separate target (`x86_64-unknown-uefi` or `aarch64-unknown-uefi`), no kernel dep — both agree on a hand-written handoff record. One file per concern and one arm per machine: the file reading, the memory map and the handoff are shared, while the serial wire (`serial`), the page-table descriptor (`vm/x86_64.rs` vs `vm/aarch64.rs`) and the last instruction (`enter`) are the machine's |
| `xtask/` | `mpos-xtask`, the build/run/test front-end; `xtask/README.md` documents every command |
| `user/` | `mpos-user`: ordinary `std` programs for the image (`rustix`, static musl) |
| `user/abitest/` | `mpos-user-abi`: the `no_std` conformance suite — `src/bin/syscall/` (one family), `src/bin/integration/` (a device, the net, a subsystem end to end) |
| `user/tests/` | `mpos-user-tests`: `std`-side tests for what is easiest to pin from a hosted language |
| `user/staging/` | Scratch sources, wired into no build. Don't assume dead — but don't build them |
| `fuzz/` | `mpos-fuzz`, **its own workspace**: libFuzzer targets over the kernel's parsers |
| `rootfs/`, `rootfs.toml`, `initramfs/`, `initramfs.toml` | the guest image and early-userspace trees + the packages, hostname and disk geometry they are built from |
| `machine.toml` | what the image is *booted* on — memory, cpus, devices, run mode. Host-side only, so editing it never reinstalls |
| `tests.toml` | which test programs the sweep runs by default, and its groups |
| `build/`, `target/`, `build-test/` | build output and scratch. Never edit, never commit |

Root `Cargo.toml`: `default-members = ["."]` is load-bearing — without it a plain `cargo build`
would build the musl user programs for the host.

---

## 3. Build, run, verify

Prereqs: nightly toolchain with `rust-src rustfmt clippy`; targets `x86_64-unknown-none`,
`-uefi`, `-linux-musl`, `-linux-gnu`, and for the aarch64 side `aarch64-unknown-none`,
`-uefi` (both named in `rust-toolchain.toml`, so rustup installs them on first use). For
`run`/`disk`/`rootfs` also: `qemu-system-x86_64`, OVMF
(`/usr/share/OVMF/x64/OVMF_CODE.4m.fd`), `qemu-img`/`qemu-nbd`, `sfdisk`, `partx`, `blkid`,
`mkfs.ext4`, `mkfs.fat`, `tar`, `sudo`. `rootfs`/`initramfs` fetch Alpine packages over the network.
For `boot-aarch64`: `qemu-system-aarch64` and AAVMF (`/usr/share/AAVMF/AAVMF_CODE.fd` +
`AAVMF_VARS.fd`, or the `QEMU_EFI.fd`/`QEMU_VARS.fd` names under `/usr/share/edk2/aarch64`), and
the `aarch64-unknown-linux-musl` target (`rustup target add aarch64-unknown-linux-musl`) so the
verdict can build and `execve` an unmodified static-musl helper (`/musl`) alongside its
hand-encoded images — symmetric with the x86 test path's musl prereq.

| Command | What it does | Root? |
|---|---|---|
| `cargo xtask build` | kernel + UEFI loader | no |
| `cargo xtask all` | kernel, initramfs, disk image, root filesystem | yes |
| `cargo xtask run` [`--mode terminal\|gui\|mirror`] [`--arch x86_64\|aarch64`] | boot in QEMU, rebuilding stale bits; `--mode` picks the window backend and the guest's `console=` list together; `--arch aarch64` boots the second machine to a persistent interactive read/echo `/init` on the serial console — a REPL stand-in that reads a typed line at EL0 through `syscall::dispatch` and echoes it back prefixed `R:` (stage 7e; the default machine is `x86_64`) | when needed |
| `cargo xtask test` | **the gate**: host unit tests, then every user test program booted in QEMU. `--arch aarch64` is the second machine's own arm — the musl programs and a runner are packed onto its ESP/disk and booted under AAVMF, read with the same `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol; it is currently **red** with the reason located (the runner's `posix_spawn` faults at EL0, `PLAN.md` §13.5). Neither arm can silently run the other machine | no |
| `cargo xtask depgraph` | measure the module graph against the Phase-11 ratchet; fails when an edge appears or a count grows | no |
| `cargo xtask boot-aarch64` | **the second machine**: build the aarch64 kernel and its UEFI loader, lay out an ESP, boot it under AAVMF, and read the verdict off the PL011 log (`M4_CONSOLE_OK`, `M4_DEVICES_OK`, `M3_TABLES_OK`, `M5_VIRTIO_OK`, `M7_RNG_OK`, `M7_BLK_OK`, `M7_FS_OK`, `M7_DEV_OK`, `M8_NET_OK`, `M8_INPUT_OK`, `M1_TIMER_OK`, `M2_CONTEXT_OK`, `M6_AS_OK`, `M6_PAGING_OK`, `M6_WALL_OK`, `M6_CPU_OK`, `M6_SERIAL_RX_OK`, `M6_TLS_OK`, `M6_EL0_OK`, `M6_CONSOLE_RX_OK`, `M6_INIT_OK`). Through stage 4a/4b it was the machine layer, `hal`'s trait impls and the console service and nothing above; since stage 7's MMIO commit it also drives its first memory-mapped virtio device — `check_rng` opens the tree's virtio-rng window over `MmioTransport` and `M7_RNG_OK` asserts 32 host-generated bytes came back — and `check_blk` drives a virtio-blk the same way, reading and verifying a known sector from a raw MMIO disk image **and writing a sector back and reading it again** (`mmio::write_readback`, `M7_BLK_OK`), `check_fs` mounts an ext4 over the block to prove the block-driver -> filesystem path (the disk root, `M7_FS_OK`), and stage 8's `check_net` opens a virtio-net, reads its MAC, **and hands a frame to its TX virtqueue requiring the device to accept it** (`mmio::tx_probe`, `M8_NET_OK`) — proving the memory-mapped transport, queue setup and DMA move buffers on this machine through the same crate x86 uses (the data path, not just config space; not yet the smoltcp stack over it); `check_input` opens a fourth MMIO device class - a `virtio-input` device whose queues the transport negotiates **and whose config-space identity it reads** (`mmio::probe_input`, `M8_INPUT_OK`) - proving the path reaches input without claiming events (the evdev nodes and pump stay x86_64's, so `/dev/input` is still absent on aarch64); `check_dev` (in the init handoff, once the console tty is up) builds the `mount -t devtmpfs`/`devfs` device set the kernel now serves on aarch64, mounts it at `/dev` (the kernel-side mirror of Linux's `CONFIG_DEVTMPFS_MOUNT`), and reads `/dev/urandom` and the MMIO-rng-backed `/dev/hwrng` back *through the live mount table* (`resolve` -> `read_at`), printing `M7_DEV_OK` — the `/dev` tree a real init mounts exists on this machine, its x86-only virtio-PCI/`uinput`/DRM nodes `#[cfg]`-gated out to leave the honest generic set; since stage 6E it also starts the scheduler, unpacks the loader's initramfs into a ramfs, loads `/init` and runs it — since stage 7c the boot task actually yields a turn to the init (`sched::is_running`, not the always-false `has_live_child`), so the init truly executes at EL0: it first proves aarch64 EL0 demand paging (it pushes `sp` past the mapped stack and stores there; `MPOS_FAULT_OK` prints only if `el0_sync`'s page-fault handler took the page), then performs a `read(0)`/`write(1,...)` round trip through `syscall::dispatch` (reads the line the handoff queued on the tty, then writes the bytes it read back prefixed `R:`, so the wire shows `R:echo-test` — stage 7d), then `execve`s a second hand-encoded image (`/t`) that writes `MPOS_EXEC_OK` and then `clone`s — the child writes `MPOS_FORK_OK`, the parent `wait4`s so it lands before the halt — proving aarch64 `execve` **and** `clone`/`wait4` run programs at EL0 (stage 6D) — and the parent then `execve`s `/musl`, an *unmodified* statically-linked `aarch64-unknown-linux-musl` binary `xtask` builds (its own `println!` prints `MPOS_MUSL_OK`; the demand-paging handler services its larger stack/anon pages), whose `exit_group` reaches `sched::exit` and prints `INIT_EXITED` — a scheduler-run program that demand-paged, read, echoed, execve'd, cloned, waited, and ran a real static-musl ELF at EL0 (stage 8) which then opened the mounted `/dev` through libc file I/O (`open`/`read` `/dev/urandom` → `MPOS_DEV_URANDOM_OK`, `open`/`write` `/dev/console` → `MPOS_DEV_CONSOLE_OK`), traversed it (`read_dir` → `MPOS_DEV_GETDENTS_OK`, `metadata` → `MPOS_DEV_STAT_OK`), and ran a full regular-file lifecycle on the mounted root (`write`/`read`/`metadata`/`remove_file` → `MPOS_FILE_OPS_OK`) and a getcwd/chdir round-trip (`MPOS_CWD_OK`) and a symlink/readlink round-trip (`MPOS_SYMLINK_OK`) and a mkdir/nested-file/rmdir round-trip (`MPOS_MKDIR_OK`) and a rename/hard-link round-trip (`MPOS_RENAME_LINK_OK`) and an ftruncate round-trip (`MPOS_TRUNCATE_OK`) and a 1 MiB mmap-backed heap buffer (`MPOS_MEMORY_OK`) and fd duplication via fcntl (`MPOS_DUP_OK`) and a struct-stat ABI check (`MPOS_STAT_OK`), all asserted) — not just the `check_el0` probe | no |
| `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` | just the host unit tests | no |
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | xtask's own tests | no |
| `cargo xtask clean` | remove `build/` and `target/` | when needed |

```bash
cargo fmt --check
cargo clippy --target x86_64-unknown-none            # kernel, bare metal
cargo clippy --target x86_64-unknown-linux-gnu       # kernel, host
cargo clippy -p mpos-xtask --target x86_64-unknown-linux-gnu
# The second machine, and the reason these two are here: everything under
# `src/arch/aarch64/` is `#[cfg]`-ed out of every x86_64 target, so nothing above
# lints it. A `--deny`-level lint in aarch64-only code fails there and nowhere else.
cargo clippy --target aarch64-unknown-none
(cd boot && cargo clippy --target aarch64-unknown-uefi)   # the loader's aarch64 arm
```

**Three build traps.** (1) **`--target` is mandatory for host work**: `.cargo/config.toml` pins
`[build] target = "x86_64-unknown-none"`, so a bare `cargo test` builds for bare metal and cannot run.
(2) **`user/abitest` uses overridden flags**: its `x86_64-unknown-none` profile uses the *kernel's*
linker script, so the gate builds it with `RUSTFLAGS=-C relocation-model=static` from inside
`user/abitest/`. Building it by hand without that yields binaries the loader rejects.
(3) **The aarch64 target links in `--release` only**: `cargo build --target aarch64-unknown-none`
fails with `relocation R_AARCH64_ABS64 cannot be used against local symbol` from the *sysroot's*
`liballoc`, which is not built for a PIE — and no gate notices, because `xtask` builds this machine
in release. Anything hand-verifying an aarch64 change must pass `--release` (or the failure reads as
a broken change rather than a broken profile).

**The self-test protocol (`cargo xtask test`).** Stage 1 = host unit tests. Stage 2 = builds
`mpos-user-tests` (musl) and `mpos-user-abi` (bare metal), packs them into a throwaway initramfs with
a generated `/init`, and boots it headless with `mpos.selftest`. `/init` runs every `/bin/t-*`; the
serial log is parsed for `=== RUN/PASS/FAIL t-name` / `=== SUMMARY pass=N fail=N` / `=== END`. The run
must end with a summary **and** QEMU exit code 33 (`isa-debug-exit`). A test that hangs, crashes or
never prints `=== PASS` counts as failed — no `#[ignore]` escape hatch; a test that cannot work is
deleted or made to pass.

**`tests.toml` picks which programs run.** All are built; grouped ones are left of the default sweep.
A test in no group always runs, so the default = "everything that is a check of *this machine*", and a
red one implicates the kernel, not the host. Groups: `external` (needs the host network or a listener),
`slow` (measurement/seconds), `diagnostic` (not a check at all — `t-sh`). `--groups <a,b>` replaces the
file's default; `--all-groups` runs everything. An undefined group name (on the CLI, or in the file
naming a nonexistent program) is an error — a sweep that quietly runs fewer tests than asked is the one
failure this must not have. So a new test lands with a group decision, and silence is the default.
Stage 3 is a non-event: the test initramfs and the FAT-synthesized boot dir live under `build/`, so
nothing a test runs can leak into the image `cargo xtask run` boots.

**Reporting the gate — never from a stale run.** This file carries **no test counts**: a snapshot here
goes stale within a commit or two, and a stale green breaks its own rule. The measured state lives in
`PLAN.md` (latest run quoted from its §12.4a summary line; every phase records what it measured). The
rule: re-run the gate and quote the summary line + exit code — or say plainly it did not get there.

---

## 4. Code conventions

### Every design here is Linux's behaviour — never a toy of it

This governs everything below. MPOS exists so **unmodified Linux binaries run**, which makes failure
asymmetric: a wrong *shape* sends the caller down a path it cannot recover from; an honest errno sends
it down its own fallback. **Nearly-right is worse than absent** — "implement the common case, tighten
later" has been paid for repeatedly, always in debugging (udevd untagging every device; a compositor
stopping with no pointer). Concretely:

1. **Measure before designing.** A behaviour a real program depends on is observed on real Linux first,
   and the observation is written down with the number beside the claim. A design argued from a man page
   alone is often wrong — the man page does not say which half of the rule the caller branches on.
2. **Answer truthfully, or refuse.** When this kernel cannot do what Linux does it says so in the only
   language a caller reads: `ENOSYS` (call absent here), `EOPNOTSUPP` (present, cannot do that),
   `EINVAL` (argument refused), `EACCES`/`EPERM` (will not grant). The `_ => ENOSYS` arm in
   `src/syscall/linux/mod.rs` is an honest answer and should stay crowded. **Returning success for
   something not implemented is the single worst thing this codebase can do** — the caller believes
   something false and stops asking.
3. **No private dialects.** A flag, struct or protocol gets its Linux shape, not a shape this
   implementation finds convenient. Two behaviourally-equal designs split the moment a third program reads them.
4. **Refuse rather than approximate.** When the honest alternative to doing it properly is doing it
   *nearly*, refuse — an unimplemented action is `EOPNOTSUPP` from its availability probe, a namespace
   that isolates nothing is `EOPNOTSUPP` rather than a namespace.
5. **A gap gets a name, not a stub.** Whatever cannot be built today is a named gap in `PLAN.md` with
   its measurement and missing prerequisite. A `TODO` in shipped code, a handler that does nothing and
   returns 0, and a shape that looks finished while doing less are the same violation.
6. **A gap gets a pin, or it is not done.** A landed behaviour is a behaviour: host test, `t-*` program,
   or both. "Works, verified by hand once" is how this repo lost the same bug four times.

### Five standing checklist rules (before anything lands)

1. **Kernel-space discipline.** What Linux runs in userspace (device management, fs utilities, network
   daemons) is not this kernel's job — it provides the ABI userspace expects and nothing more.
2. **High cohesion, low coupling.** Before adding a feature, locate the module that owns the concept.
   A change that reaches into another module's internals is in the wrong module.
3. **No toy or temporary designs.** Governing rule 3 + item 4, as a checklist item; never waived for convenience.
4. **Every behaviour ships with a pin that would fail without it.** Governing rule 6, counterfactual as
   the criterion: a test that passes with *and* without the change pins nothing.
5. **Respect the designed dependency graph.** Modules depend on each other only along the edges the
   design allows; update the graph in `ARCHITECTURE.md` before adding an edge, not after. A new upward or
   back edge (a VFS module calling a driver's internals) is a violation even when it compiles.

### Module boundaries

Rule 5 says respect the graph; this is it. `ARCHITECTURE.md` §2 = the full mermaid diagram, §3 the
ownership table, §9 the measured violations — this table is the working copy and the two files move
together. A module may call **downward only**, into what "may depend on" lists; anything else is a
violation. The graph is *measured*, not just described: `cargo xtask depgraph` (a host test too)
re-extracts every `crate::` reference and fails when an edge appears that was not recorded the last time
someone set the number. Adding an edge along a violation is a red gate; removing one lowers a constant.
The purest leaves are separate crates under `crates/` (re-exported from `src/lib.rs` as `crate::err`
etc.) so an upward edge from them is a **compile error**, not a convention — see `ARCHITECTURE.md` §1.

| Module | Owns | May depend on | Must never |
|---|---|---|---|
| `main.rs` | boot, milestone checks, panic handler | anything (it is the wiring) | hold reusable logic — that lives in the lib |
| `syscall/` | numbers, `abi.rs` facts, dispatch, handlers | `vfs`, `sched`, `mm`, `drivers`, `net`, services | be a per-device ioctl switchboard (V-2) |
| `vfs/` | `Inode`/`FileSystem`, mounts, fd table, every fs, pipes/epoll/timerfd/signalfd | `sched`, `mm`, `blk`, `pci` | name a device/socket type (V-1 closed — a caller uses `dyn Inode::as_any`; the trait names none). The terminal emulator is the `term` crate; the console mux and line discipline are still here, but the shared VC vocabulary (`VC_COUNT`, KD/K mode words in `crate::term::vc`), the pure `vtmode` handover table (`crate::term::vtmode`) and the active-console number (`crate::term::vc::active`) have moved to the `term` leaf — which collapsed the `vfs ↔ console` pair (V-4 pair closed; the mux *file* move, D.2b, remains) |
| `sched/` | task table, priority bands, sleep/block, signals, futex | `mm`, `arch`, `binfmt` | host another subsystem's per-process cleanup (V-3) |
| `mm/` | buddy frames, slab heap, mapping plans, ELF loader | `boot` | know files, tasks or devices |
| `arch/` | GDT/IDT, PIC/LAPIC, paging, context switch, syscall gate, timer, ports | `mm` (via its own re-exports) | be named from outside `arch::`/`hal::` — every machine call above the machine layer goes through `crate::hal`, and the depgraph now fails on any module but `arch`/`hal`/`main`/`lib` naming it (`PLAN.md` §11.4, stages 1–4 all landed) |
| `hal/` | the capability traits a machine implements (`Clock`, `WallClock`, `Cpu`), the platform tag `Current`, and the one-line forwards everything above calls | nothing — `arch/<arch>/` implements the traits *for* `hal`'s `Current`, so `arch → hal` is the only arrow | hold machine bodies: a port adds its `impl`s in its own `arch/<arch>/`, never here |
| `console/` | log ring, serial, framebuffer switch, the six virtual consoles | `hal` (the `Console`/`SerialRing` sink), `boot` (the console list), the `term` crate | reach into the VFS for the grid: it names `crate::term` for the grid and for the active-console number, never `vfs::*` (V-4 pair closed). The serial wire is reached through `crate::hal::Console`/`SerialRing` (stage 4a), not by naming `drivers::serial`, so the framebuffer + interrupt-ring + parking arms `#[cfg]`-inert on a polled machine |
| `net/` | smoltcp, socket registry, AF_INET/UNIX/NETLINK/VSOCK/PACKET | `drivers`, `sched`, `uevent` | validate user pointers outside the `uaccess` leaf (V-9, closed by B3) |
| `drivers/` | virtio, DRM/KMS, render task, evdev/uinput, PCI config, serial, RTC | `mm`, `pci`, `sched` | call back into `syscall` (V-5) or parse ioctl ABI (V-2) |
| leaves — `crates/*` + `rand`/`uevent`/`logging`/`sync`/`uaccess` | one concern each, pure logic, host-compiled where possible; downward-only and cannot name the kernel. `err` is the errno table every layer may answer with, so nothing below `syscall` names `syscall` for a minus sign | each other and `arch` as drawn | accumulate state that makes them a layer |

Two rules make the table usable rather than decorative:

1. **An existing violation is not precedent.** `ARCHITECTURE.md` §9's V-* are real edges being removed in
   Phase 11. Code may not add a *new* caller along a violation edge or widen one.
2. **A needed edge changes the design record first.** A legitimate dependency genuinely missing from §2
   is added to `ARCHITECTURE.md` (§2 + §3) *in the same commit*, with the reason — a diff that
   contradicts the graph is the diff that is wrong.

### Seven extended rules (what a change carries, what it may not touch)

Each exists because of a failure this repo has actually paid for.

1. **A test is a contract, not an obstacle.** When the gate goes red, the code changes, not the test.
   Weakening a test is the same violation as faking success. Where a test's *expectation* is genuinely
   wrong, the commit body carries the measurement proving the kernel's answer is Linux's.
2. **Every new parser gets a fuzz target.** Any place the kernel reads user- or wire-supplied bytes
   (ioctl arg, ELF section, packet) joins the targets in `fuzz/` in the same commit, or `PLAN.md` says why not.
3. **A published ABI is append-only.** Syscall numbers, struct layouts and ioctl encodings are contracts
   once shipped — a running musl binary reads them. Renumber/repack/reinterpret a landed shape only with
   an explicit migration in `PLAN.md`.
4. **Worst-case rule.** The global allocator still panics on failure (`PLAN.md` §8.0), so user-controlled
   growth can end the machine. Until that closes, any new path whose allocation volume a user can steer
   carries a bound — capped count, checked length, or `ENOMEM` at the limit. Drivers cite it by this name.
5. **Interrupt context does none of: sleep, allocate, walk an unbounded structure.** An IRQ handler does
   bounded work, takes no lock a preempted task may hold, and defers the rest. On one core, spinning on a
   preempted holder is a deadlock with the machine stopped on it.
6. **A performance change ships with before/after numbers.** `PLAN.md` §10 is only true if entries are
   measurements; an optimisation without a cost is how the idle `hlt` bug hid until it burned a tick/wake.
7. **One intent per commit.** A refactor and a behaviour change in one diff have no single *why* for the
   §7 body. Split: the refactor lands first, green on its own, then the change.

### The `no_std` split is the central convention

Arch-independent logic lives in the library and is host-tested; anything touching port I/O, descriptor
tables, virtio rings, firmware or a real clock is gated behind `#[cfg(target_os = "none")]` (and
`target_arch = "x86_64"` where the hardware is x86-specific). Pure logic (arithmetic, encodings, parsers,
policies) belongs host-side even when its only caller is bare metal — `crates/evdev`/`crates/uinput`
exist to hold input policy a host test can reach.

The gate is applied per module, by this rule (home: `src/lib.rs`'s header): **a module is host-compiled
when its logic touches no hardware, no port I/O, no virtio ring and no real clock — and then its
dependencies must be host-compiled too, transitively.** The transitive half is what gets forgotten: when
you gate a module, check what gates its tests; when you ungate one, check everything it names is ungated
too. `cargo xtask test` opens with a mechanical check for the one failure the compiler cannot see — a
`#[cfg(test)]` module behind a `target_os = "none"` parent, dead code that reads as coverage
(`xtask/src/gates.rs`, closing `ARCHITECTURE.md` V-11).

### Concurrency

- **Uniprocessor**: disabling interrupts *is* mutual exclusion; a critical section is the whole mechanism.
- Shared state is `critical_section::Mutex<RefCell<T>>`. **Never `spin::Mutex`** — busy-waiting on a lock
  held by a task this core preempted is a deadlock (`spin::Once` only for one-time `gdt`/`idt` init).
- **Never sleep holding a lock.** The blocking pattern is *lock, poll, try, unlock, park* — see
  `src/net/mod.rs` and `ARCHITECTURE.md` §7.
- The idle halt belongs to the scheduler's loop. A task that `hlt`s while staying runnable costs a whole
  tick on every wake — a real bug with a measured cost (`PLAN.md` §10.2).

### Comments, errors, panics

House style is long-form prose doc comments explaining **why**, not what: `//!` headers run to paragraphs,
and a non-obvious decision names the rejected alternative and the evidence. Match that. Don't add a
comment that restates code; don't delete an existing rationale comment while editing around it.

Panic ends the machine (`panic = "abort"` + a handler that exits QEMU). Syscall handlers return Linux
errno constants through `err` and never unwrap user input. Known gap: the global allocator's failure path
is still `panic!` (`PLAN.md` §8.0).

---

## 5. Recipes

**Add/change a Linux syscall.** (1) `src/syscall/linux/nr.rs` — the number and `pub const` (this file is
`#[path]`-included by the test harness, so naming it there makes it addressable). (2) `src/syscall/linux/mod.rs`
— the dispatch `match` arm (grouped arms like `nr::STAT | nr::LSTAT =>` are normal). (3) handler in
`fs/file.rs` (path/file/device calls) and `fs/ioctl.rs` (the device-`ioctl` entry point) — the old
`fs.rs` split along that seam at Step E — plus `proc.rs` (memory/process/time) or `posix.rs`
(identity/capabilities/scheduling/
clocks); validation and contracts with the handler, pure ABI facts (flag values, layouts, encodings) in
host-compiled `abi.rs`. (4) an unimplemented call falls to `_ => ENOSYS` (§4). (5) add a conformance program
+ its `[[bin]]` entry (next recipe). (6) update `PLAN.md` ("Remaining ABI support" + TODO board) and
`UAPI.md` — its syscall table and work list are the same inventory; a call moving between served / refused /
wrong-shape belongs in both.

**Add a user test program.** `no_std` raw-syscall → `user/abitest/src/bin/syscall/t-<name>.rs`, or
`.../integration/t-<name>.rs` if it needs a device, the net, another process image, or is a timing
instrument. **You must add an explicit `[[bin]]` block to `user/abitest/Cargo.toml`** — Cargo does not
auto-discover `src/bin/<subdir>/*.rs`, and `xtask/src/selftest.rs` bails on a source with no binary.
`std`/`rustix` → `user/tests/src/bin/t-<name>.rs` (flat, auto-discovered). Print `ok:`/`FAIL:` per check and
a final `PASS`/`FAILED`, exit 0/nonzero. The harness + stubs are `user/abitest/lib/lin.rs`, included with
`#[path = "../../../lib/lin.rs"]`. Keep the `t-` name (the test `/init` globs `/bin/t-*` and keys the verdict
on it). **Decide its group in `tests.toml`** — a test that reaches outside (host network, a listener), measures,
or takes seconds is `external`/`slow`/`diagnostic`, not silent.

**Add a kernel unit test.** A `#[cfg(test)] mod tests` at the bottom of the file it tests, with the file's
parent module compiled for the host (§4). Assert on the contract (value, errno, count), not a log line; prefer
a test that would fail if the policy differed — the ext4 cache, evdev window and locking tests are the models.

**Change the guest image.** A file that must exist → the `rootfs/` (or `initramfs/`) tree — overlays apply
*after* anything the toml writes, so a tree file wins. A package → `rootfs.toml` (`base_packages`/
`extra_packages`) or `initramfs.toml`. Partition/fs-type/image-size/hostname/DNS → `rootfs.toml` (parser is
deliberately tiny: `key = value` + arrays). Anything about the *machine* the image boots on (memory, cpus,
`vsock`, `audio`, window backend, run mode, extra QEMU args) → `machine.toml`, **not** `rootfs.toml` — this
is a cost rule: `rootfs.toml`'s mtime decides whether the Alpine tree is reinstalled, so a machine key there
would charge a full package install for a memory size (`xtask/src/config.rs`'s
`the_machine_and_the_image_keep_their_own_keys` fails if a machine key drifts back; `image_size`/`esp_size`
stay in `rootfs.toml` because changing either re-makes the partitions). Boot wiring (`mpos.selftest`, the
`console=` list) is read in `src/main.rs` via `src/boot/console_list.rs`, and the `--mode` writes the list in
`xtask/src/config.rs`/`rootfs.rs` — the kernel is told which consoles it has and nothing about how it was launched.

**Touch the boot handoff.** The `Handoff` record is written twice — `boot/src/handoff.rs` (loader) and
`src/boot/handoff.rs` (kernel) — plus `BOOT_STACK_CANARY` in `src/main.rs`. No shared crate between them by
design. Change all three, and keep the loader's "allocate everything before `exit_boot_services`" ordering intact.

---

## 6. Known traps

| Trap | What to do |
|---|---|
| Bare `cargo test` won't link/run | Always `--target x86_64-unknown-linux-gnu` |
| A new test program silently never built | Add the `[[bin]]` block; `xtask` errors on "no binary" only for sources it finds |
| A `src/` file nothing declares looks real | A `mod` declaration makes a file part of the build; missing ones left duplicate serial/RTC drivers for months (V-7). Check for the declared one before writing a new file |
| A module has a host body and a machine body | **Both move, or it does not build.** The host suite was green while `--target x86_64-unknown-none` failed: `VfsError::Revoked` was mapped in the host `err_of` but the machine `#[cfg]` arm was left non-exhaustive (V-11). Run the bare-metal target on every pass |
| `cargo xtask run` prints no milestone markers | By design — `M*_OK` checks run only under `mpos.selftest` (set by `xtask test`). Markers that exist: M0–M4, M6, M7, M8_IPC, M8 (no M5, nothing above M8). The aarch64 machine prints eighteen of its own — `M4_CONSOLE_OK`, `M4_DEVICES_OK`, `M3_TABLES_OK`, `M5_VIRTIO_OK`, `M7_RNG_OK`, `M7_BLK_OK`, `M8_NET_OK`, `M8_INPUT_OK`, `M1_TIMER_OK`, `M2_CONTEXT_OK`, `M6_AS_OK`, `M6_PAGING_OK`, `M6_WALL_OK`, `M6_CPU_OK`, `M6_SERIAL_RX_OK`, `M6_TLS_OK`, `M6_EL0_OK`, `M6_CONSOLE_RX_OK`, `M6_INIT_OK` (in that order) — which no x86 sweep produces, and they come from `cargo xtask boot-aarch64`, not from `run`, because the second machine still has no disk image or rootfs to boot. Since stage 7c the verdict also requires the scheduler-run `/init` to actually execute, and 7d made it a full interactive round trip: the handoff queues a line on the tty, the init does `read(0)` (returns it) then writes the bytes it read back prefixed `R:` (`write(1,"R:")` then `write(1,buf,n)`) then `exit_group`, so boot-aarch64 shows `echo-test`, `R:echo-test` and `INIT_EXITED` — proof a real userspace program read console input and echoed it back at EL0 via the scheduler, not just the pre-scheduler `check_el0` probe. The `M3_`/`M4_`/`M5_`/`M6_` on the aarch64 side are that machine's *stage* checks (console 4b, devices 5a, timer/paging/virtio/context 1c-6f, address-space/EL0 6a-6g, thread pointer 6h, init handoff 6E, serial receive 7a, console read path 7b, scheduler-run init 7c), which is why several start with a stage number the x86 sweep never prints |
| `TERMINATORS` in `xtask/src/aarch64.rs` is a list of *last* markers | It holds the **last** check's success marker and **every** check's failure marker — never the success marker of a check that another check comes after. A terminator stops the run, so a success marker listed too early kills the machine mid-boot and the log simply ends, which is indistinguishable from a hang. Four directions of this have now been measured: the list once ended at `M1_TIMER_OK`, which stage 2's checks come after (killed mid-line); stage 5a added `M4_DEVICES_OK`, which the timer and context checks come after — a boot that finished in 5.7 s instead of the 60 s deadline and stopped three lines into a machine that was working; stage 6a is the first check to land *after* the last one, so `M2_CONTEXT_OK` had to come **out** when `M6_EL0_OK` went in; and stage 6b's check lands *before* 6a's, so its **success** marker is not a terminator and only its failure marker is — the same holds for stages 6c/6d/6e, each of which sits before 6a's EL0 check, so `M6_PAGING_FAILED`/`M6_WALL_FAILED`/`M6_CPU_FAILED` are terminators while their `_OK` markers are not — and stage 6h's thread-pointer check sits in the same slot, so `M6_TLS_FAILED` is a terminator and `M6_TLS_OK` is not — and stage 6E's init handoff runs *after* the EL0 probe, so `M6_INIT_OK` is now the last success terminator and `M6_EL0_OK` came **out** of the list (leaving it in would stop the poll on a machine that still has init to run) — and stage 7a's serial-receive check sits in the same pre-EL0 slot as 6h (between `check_cpu` and `check_tls`), so `M6_SERIAL_RX_FAILED` is a terminator and `M6_SERIAL_RX_OK` is not — and stage 7b's console-read check runs in the handoff *after* the EL0 probe but *before* the init wait, so `M6_CONSOLE_RX_FAILED` is a terminator and `M6_CONSOLE_RX_OK` is not — and stage 7c made the handoff actually run `/init` (wait on `is_running`, not `has_live_child`, which was false from the start because pid 1 has no parent), so the terminal proof is now the init's `INIT_EXITED` (its `exit_group` routed to `sched::exit`) and the `R:echo-test` echo it writes after its `read(0)` completes; `INIT_EXITED` is the last success terminator and `M6_INIT_OK` (now printed before the wait) came **out** of the list. Elapsed time is the tell: a stall is 60 s, a wrong terminator is seconds |
| A `static` with a zero initializer and no writer lands in `.rodata` | Read-only, on a machine whose page tables enforce it. Both times this has bitten, it was a buffer something intended to write: stage 1d's 128 KiB page-table arena (which cost 128 KiB of `.rodata` and was deleted with the `-kernel` path) and stage 2's task stack, whose first `stp` took a data abort *with the fault address inside the kernel image*. The fix is not a section attribute — it is to take frames from the allocator, which is what the code wanted anyway. Add `-C link-arg=-zmax-page-size=0x1000` to the list of things that will not save you |
| No cross-architecture build | aarch64 boots the way x86_64 does — firmware, ESP, `mpos-boot`, higher half, its own page tables out of `mm::vm::kernel_mappings`, a switch between two kernel tasks, the firmware's device tree read back and checked against the machine's own constants, the device window in the kernel half with the tree's thirty-two virtio transports read through it, and code run at EL0 in an address space of its own (`TTBR0_EL1`) that traps into the kernel and is answered — a space whose pages can then be protected, shared, marked copy-on-write, resolved and unmapped, with its tables returned when it goes, its address space
answerable as a running process would answer it, a real wall epoch read off the
PL031, user SIMD turned on and a distributor line maskable and an interrupt
waitable on, and `crate::hal`'s baseline capability concerns implemented for it —
through `cargo xtask boot-aarch64` (stages 1a-1f, 2, 5a, 5b, 6a-6f of `PLAN.md`
§9.1). Three things about that boot are measured rather than obvious, and all
three are choices the *command* has to make: AAVMF publishes **no device tree** while it also builds ACPI tables (which `-M virt` does by default on aarch64), so the command passes `acpi=off` to get the machine to describe itself the way this port reads; the `virtio-mmio` transports answer **version 1** — the legacy register map, which this kernel's driver crate cannot read — unless `-global virtio-mmio.force-legacy=false` is passed; and attaching a device costs the guest **3.66 MiB** of usable RAM (measured: `48954` → `48018` frames, with the flag above contributing none of it), which on top of stage 5b's doubled page-table pool (`66` frames) and stage 6b's check (16 KiB of kernel: text 62 → 74, rodata 16 → 20) is why the boot log's frame count is `47944` where stage 5a's was `48954`. The aarch64 **debug** profile does not link at all (see the build traps above). What compiles above `arch` on aarch64: `hal` itself (stage 6f — its seven baseline concerns `Clock`/`WallClock`/`Cpu`/`Interrupts`/`Paging`/`Contexts`/`Console` implemented, and x86's four optional `Msi`/`SerialRing`/`FpuTrap`/`Segments` capabilities left out because this machine has no such hardware), and `console` (stage 4a — the serial sink reached through `hal::Console`, its framebuffer/interrupt-ring/parking arms `#[cfg]`-inert; stage 4b exercises it at boot through a `main`-supplied `milestone` fn that emits `M4_CONSOLE_OK`, so it is runtime-pinned on the wire, not just compiling). Still gated (each a further cut into the knot): `drivers` and `net` (both `all(target_os = "none", target_arch = "x86_64")`). Now compiled and wired for aarch64: the stage-5 knot `sched`/`vfs`/`uaccess`, and `syscall` (stage 6A/6E — the whole module builds on its per-arch asm-generic number table, its gate is open to aarch64, and `el0_sync` routes a running task's `svc` into `linux_dispatch`, the deferred signal/ptrace/socket/device/`execve`-return families falling to honest `ENOSYS`/`ENOTTY`); since stage 6E the boot starts the scheduler, unpacks the initramfs and runs `/init` to completion, so the routing is exercised end-to-end and pinned by `M6_INIT_OK`. `cargo xtask run --arch aarch64` boots the second machine to a persistent interactive read/echo `/init` on the serial console — the looping image (`aarch64_init::looping_init_elf`, stage 7e) packs the same `read`/`write` echo body with `exit_group` replaced by a branch back to the top, so a typed line is echoed (`R:...`) and the next waited for; `cargo xtask test` and a *full* shell remain x86_64-only. The idle/park context switch now works (stage 7a fixed `set_kernel_stack`, which emitted an undefined `MSR SP_EL1` at `EL1h`), the PL011 receive path is pinned by `M6_SERIAL_RX_OK`, and stage 7b wired it through the tty: `hal::ConsoleInput::poll_input` (aarch64-only capability; x86_64 feeds input from its serial interrupt and is not a `ConsoleInput`), an input pump task that polls the wire into `vfs::tty::global().receive_byte`, and `M6_CONSOLE_RX_OK` proving an injected byte reads back through `tty.read`. The read/echo loop (`/init` that reads `fd 0` in a loop, not one-shot) is landed as the stage-7e REPL; what is left for a *real* shell is a shell binary (static musl `ash`) in the aarch64 initramfs, which additionally needs the aarch64 `execve`/`clone` return-frame path (the named stage-6D gap) - the `aarch64-unknown-linux-musl` target is installable and Alpine aarch64 is fetchable, so there is no environment wall, only stages to walk. The full `drivers` un-gate (virtio-blk MMIO for a disk root) and `net` remain x86_64-only. The `-kernel` bring-up those stages used is **deleted** — do not bring it back as a shortcut, because the image is a higher-half PIE now and has no fixed address for QEMU to load it at |
| `fuzz/` won't build from the root | It is its own workspace; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test never runs | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; a host test inside it is unreachable |
| `user/staging/` looks like a missing test | Deliberate scratch — check `user/abitest` and `user/tests` for the live version first |
| `build-test/` holds old scripts/logs | Scratch (gitignored as `/build*`); don't build on or commit them |
| Two `cargo xtask test` runs in one tree report nonsense | They share `build/test-initramfs.tar`, `build/test-esp`, `build/test-serial.log`; one reads a log the other is still writing. Symptom: "115 of 136 did not pass" beside a log ending `=== SUMMARY pass=134 fail=2`. Trust the log's own summary + `=== END`, re-run alone |
| The test machine ≠ the run machine | `xtask test` boots an initramfs on its own QEMU wiring; `xtask run` boots the disk image on `machine.toml`'s. A device only one has is a shape no test sees — this hid an empty-SATA-controller boot that cost 28 s (§7.3). Name a driver's "controller with nothing on it" behaviour on the machine that has one (`cargo xtask run`) before calling it measured |

---

## 7. Git conventions

Subjects follow the log: `type(scope): what changed` — types `feat`/`fix`/`docs`/`refactor`; scopes are
subsystems (`vfs`, `sched`, `net`, `drm`, `ext4`, `syscall`, `xtask`, `console`, `input`); multi-subsystem
commits list them. The **body carries the why**: what was observed, measured, rejected — this history *is*
the design record, so a commit that only restates the diff is out of place. `git log` the file before you
change it. The review question every change gets: *is that Linux's answer, or a shape that looks like it?*
Something that cannot be Linux's today says so in its errno and takes a named `PLAN.md` gap in the same
commit (§4). Never commit `build/`, `target/`, `build-test/` or anything under `.workbuddy/`, `.qoder/`,
`.codely-cli/` (all gitignored). Never push to a remote unless explicitly asked.

---

## 8. Where the plans live

- `PLAN.md` — roadmap phase by phase, TODO board at the top, "Suggested next moves". Also the ledger of
  each gate run and performance change *with its measurement*, and the home of any new plan.
- `UAPI.md` — the userspace surface as an inventory (syscalls, ioctl families, layouts, flag vocabularies),
  each row carrying Linux's answer, this kernel's, the owning module and the pin — the working list of
  *served* / *honestly refused* / *wrong shape* (only the third is work).
- `ARCHITECTURE.md` §9 — the debt audit, mirrored in `PLAN.md` Phase 11 with an ordered remediation plan.
- If you implement something, update `PLAN.md`'s status — and `UAPI.md`'s row, when a call or request moves
  between the three verdicts — **in the same commit**. The board is only useful if it is true.
