# AGENTS.md

Guidance for AI agents (and humans) working in this repository. Read this before your first edit.

`README.md` says what MPOS *is*; `ARCHITECTURE.md` says how it is put together and where the layering
is currently violated; `PLAN.md` is the feature roadmap, the measured status of every phase, and the
home of any new plan; `UAPI.md` is the inventory of the userspace-visible surface — syscalls, device
ioctls, layouts and flag vocabularies — one row per promise, so the whole ABI can be reviewed at once.
This file is about **how to work here**.

---

## 1. What this is, in three sentences

MPOS is a uniprocessor x86_64 kernel in Rust (`no_std`, edition 2024) that is binary-compatible with
the Linux x86_64 ABI, so musl binaries — busybox, Alpine's `apk`, weston — run on it unmodified. It
boots on UEFI through its own loader (`mpos-boot`), and is built around an unusual split: the kernel
is a **library** that compiles for the host so its arch-independent logic is unit-testable with plain
`cargo test`, plus a thin binary that holds only what must run on the machine. Everything is driven
by `cargo xtask`, and every behaviour is meant to be pinned by a test — host unit test, program
booted in QEMU, or both.

---

## 2. Repository map

| Path | What lives there |
|---|---|
| `src/` | The kernel crate `mpos`: `lib.rs` (host-testable) + `main.rs` (boot entry, milestone checks, panic handler) |
| `boot/` | `mpos-boot`, the UEFI loader. Separate target (`x86_64-unknown-uefi`), no dependency on the kernel — they agree on a hand-written handoff record |
| `xtask/` | `mpos-xtask`, the build/run/test front-end. `xtask/README.md` documents every command (bilingual) |
| `user/` | `mpos-user`: ordinary `std` programs for the image (`rustix`, static musl) |
| `user/abitest/` | `mpos-user-abi`: the `no_std` ABI conformance suite — `src/bin/syscall/` (one syscall family), `src/bin/integration/` (a device, the network, a subsystem end to end) |
| `user/tests/` | `mpos-user-tests`: `std`-side tests for what is easiest to pin from a hosted language |
| `user/staging/` | Scratch sources not wired into any build. Do not assume they are dead — but do not build them either |
| `fuzz/` | `mpos-fuzz`, **its own workspace**. libFuzzer targets over the kernel's parsers |
| `src/arch/x86_64/linker.ld` | Kernel linker script, referenced from `.cargo/config.toml` |
| `rootfs/`, `rootfs.toml` | The guest image's contents: the overlay tree, and the packages, hostname and disk geometry it is built from |
| `machine.toml` | What that image is *booted* on — memory, cpus, devices, the standing run mode. Host-side only, so editing it never reinstalls the image |
| `tests.toml` | Which user test programs the self-test sweep runs, and which groups it leaves out by default |
| `initramfs/`, `initramfs.toml` | The early userspace's contents and package list |
| `build/`, `target/`, `build-test/` | Build output and scratch. Never edit, never commit |

The root `Cargo.toml` declares `members = ["user", "user/tests", "user/abitest", "boot", "xtask"]` and
`default-members = ["."]`. That last line is load-bearing: without it a plain `cargo build` would build
the musl user programs for the host.

---

## 3. Build, run, verify

### Prerequisites

```bash
rustup toolchain install nightly
rustup component add rust-src rustfmt clippy --toolchain nightly
rustup target add x86_64-unknown-none x86_64-unknown-uefi \
                   x86_64-unknown-linux-musl x86_64-unknown-linux-gnu
```

Plus, for `xtask run`/`disk`/`rootfs`: `qemu-system-x86_64`, OVMF (`/usr/share/OVMF/x64/OVMF_CODE.4m.fd`),
`qemu-img`, `qemu-nbd`, `sfdisk`, `partx`, `blkid`, `mkfs.ext4`, `mkfs.fat`, `tar`, `sudo`.
`rootfs`/`initramfs` fetch Alpine packages over the network.

### The commands that matter

| Command | What it does | Root? |
|---|---|---|
| `cargo xtask build` | Kernel + UEFI loader | no |
| `cargo xtask all` | Kernel, initramfs, disk image, root filesystem | yes |
| `cargo xtask run` | Boot in QEMU, rebuilding if anything is stale | when needed |
| `cargo xtask run --mode terminal\|gui\|mirror` | Same, with the QEMU wiring and the guest's `console=` list chosen together — the window, which consoles the guest counts, and where the launcher reads the serial line (stdio, or `build/serial.log` in `gui`, whose text console shows in the window) are separate questions | when needed |
| `cargo xtask test` | **The gate.** Host unit tests, then every user test program booted in QEMU | no |
| `cargo xtask depgraph` | Measure the kernel's module graph and check it against the Phase-11 ratchet — it fails when an edge appears or a count grows | no |
| `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` | Just the host unit tests | no |
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | xtask's own unit tests | no |
| `cargo xtask clean` | Remove `build/` and `target/` | when needed |

```bash
cargo fmt --check
cargo clippy --target x86_64-unknown-none            # the kernel, bare metal
cargo clippy --target x86_64-unknown-linux-gnu       # the kernel, host
cargo clippy -p mpos-xtask --target x86_64-unknown-linux-gnu
```

### Two traps in the build configuration

1. **`--target` is mandatory for anything host-side.** `.cargo/config.toml` sets
   `[build] target = "x86_64-unknown-none"`, so a bare `cargo test` builds the tests for bare metal and
   cannot run them. Always pass `--target x86_64-unknown-linux-gnu` for host work.
2. **`user/abitest` is built with overridden flags.** Its `x86_64-unknown-none` profile uses the
   *kernel's* linker script, so `cargo xtask test` builds it with `RUSTFLAGS=-C relocation-model=static`
   from inside `user/abitest/`. Building it by hand without that produces binaries the loader rejects.

### The self-test protocol (`cargo xtask test`)

Stage 1 runs the host unit tests. Stage 2 builds `mpos-user-tests` (musl) and `mpos-user-abi` (bare
metal), packs them into a throwaway initramfs with a generated `/init`, and boots it headless with
`mpos.selftest`. That `/init` runs every `/bin/t-*`, and the serial log is parsed for
`=== RUN t-name` / `=== PASS t-name` / `=== FAIL t-name exit=N` / `=== SUMMARY pass=N fail=N` / `=== END`.
The run must end with a summary **and** QEMU exit code 33 (the `isa-debug-exit` clean exit). A test that
hangs, crashes or never prints `=== PASS` counts as failed, and there is no `#[ignore]`-style escape
hatch — a test that cannot work must be deleted or made to pass.

**Which programs stage 2 boots is `tests.toml`.** Every test is built; the ones in a group are left out of
the default sweep, and the *staging* is the selection — the guest runs whatever `/bin/t-*` it was given. A
test in no group always runs, so the default sweep means "everything that is a check of this machine", and
a red one implicates the kernel rather than the host's network or a busy host. The shipped groups are
`external` (needs the host's network or a listener on it), `slow` (measurement, or seconds of work) and
`diagnostic` (not a check at all); `cargo xtask test --groups <a,b>` replaces the file's `default` with an
explicit list, and `--all-groups` runs everything. A group name the file does not define — on the command
line, or in the file naming a program that does not exist — is an error, because a sweep that quietly runs
fewer tests than it was asked to is the one failure this must not have. A new test therefore lands with a
decision about its group, and its silence is the default.

Stage 3 is a non-event by design: the test initramfs is a separate archive under `build/`, and the boot disk
is a directory QEMU synthesizes as virtual FAT, so nothing from a test run can leak into the image
`cargo xtask run` boots.

### Reporting the gate

This file deliberately carries **no test counts**: a snapshot here goes stale within a commit or two, and a
stale green is exactly what its own standing rule forbids. The measured state of the tree lives in
`PLAN.md` — the latest gate run is quoted from its own summary line in `PLAN.md` §12.4a, and every phase
section records what it measured when it landed.

The rule itself is unchanged: never report "tests pass" from a stale run. Re-run the gate and quote the
summary line and the exit code it printed — or say plainly that it did not get there.

---

## 4. Code conventions

### Every design here is Linux's behaviour — never a toy version of it

This governs every other convention here, so it comes first. MPOS exists so that **unmodified Linux
binaries run on it**, which makes the failure modes asymmetric: a question answered with the wrong *shape*
sends the caller down a path it cannot recover from, while one answered with the honest errno sends it down
its own fallback. **Nearly-right is worse than absent**, and the rejected alternative — "implement the
common case now and tighten it later" — has been paid for here several times, always in debugging rather
than in writing: udevd untagging every device, libinput dropping every node, a compositor logging a device
and then stopping with no pointer. Concretely:

1. **Measure before designing.** Behaviour a real program depends on is observed on a real Linux first, and
   the observation is written down with the number beside the claim (`PLAN.md` §4.1's zero-byte `recvmsg`,
   §7.2's `EVIOCGPHYS` errno, §8.2's kill actions all started as a measurement). A design argued from a man
   page alone has repeatedly been wrong, because the man page does not say which half of the rule a caller
   actually branches on.
2. **Answer truthfully, or refuse.** When this kernel cannot do what Linux does it says so in the only
   language a caller reads: `ENOSYS` for a call that does not exist here, `EOPNOTSUPP` for one that exists
   but cannot do that, `EINVAL` for an argument it refuses, `EACCES`/`EPERM` for one it will not grant. The
   `_ => ENOSYS` arm in `src/syscall/linux/mod.rs` is an honest answer and should stay crowded. **Returning
   success for something not implemented is the single worst thing this codebase can do** — it is how a
   program comes to believe something false and stops asking.
3. **No private dialects.** A flag, struct or protocol gets its Linux shape, not a shape this kernel's
   implementation finds convenient. Two designs that are behaviourally the same today split the moment a
   third program reads them.
4. **Refuse rather than approximate.** When the honest alternative to doing it properly is doing it *nearly*,
   refuse: an action this machine does not implement is `EOPNOTSUPP` from its availability probe rather than
   quietly allowed (`PLAN.md` §8.2), and a namespace that isolates nothing is `EOPNOTSUPP` rather than a
   namespace (`PLAN.md` §6.3).
5. **A gap gets a name, not a stub.** Whatever cannot be built today is recorded in `PLAN.md` as a named gap
   with its measurement and its missing prerequisite beside it — the board's `[~]` entries hold exactly
   those. A `TODO` in shipped code, a handler that does nothing and returns 0, and a shape that looks
   finished while silently doing less are the same violation.
6. **A gap gets a pin, or it is not done.** A landed behaviour is a behaviour: host unit test, `t-*` program,
   or both (§5 says where each kind goes). "Works, verified by hand once" is how this repository lost the
   same input bug four times.

### The five standing checklist rules

Checked before anything lands. The first governs *what* may go in, the next two *how* it must be shaped, the
last two *what* it must carry with it.

1. **Kernel-space discipline.** Follow the Linux standard for what belongs in a kernel: policy Linux runs in
   userspace (device management, filesystem utilities, networking daemons) is not this kernel's job — it
   provides the ABI userspace expects and nothing more.
2. **High cohesion, low coupling.** Before adding a feature, locate the module that owns the concept. If the
   change would reach into another module's internals, the feature is being placed in the wrong module.
3. **No toy or temporary designs.** Governing rule 3 and its item 4 as a checklist item, so it is caught on
   every pass; never waived for convenience.
4. **Every behaviour ships with a pin that would fail without it.** Governing rule 6, with the counterfactual
   as the criterion: a test that passes with *and* without the change pins nothing, so it does not count.
5. **Respect the designed dependency graph.** Modules depend on each other only along the edges the design
   allows; update the graph in `ARCHITECTURE.md` before adding an edge, not after. A new upward or back edge
   (a VFS module calling into a driver's private internals) is a design violation even when it compiles.

### Module boundaries

Rule 5 says to respect the graph; this is the graph. `ARCHITECTURE.md` §2 holds the full mermaid diagram with
every real `crate::` reference, §3 the ownership table, §9 the measured violations — this table is the working
copy and the two files must move together. A module may call **downward only**, into what "may depend on"
lists; everything else is a boundary violation. The graph is measured, not just described: `cargo xtask
depgraph` (a host test too, so `cargo xtask test` runs it) re-extracts every `crate::` reference, reports the
edges and the mutual module pairs, and fails when one appears that was not there the last time somebody
recorded the number (`PLAN.md` §11.2 Step G). Adding an edge along a violation is therefore a red gate, and
removing one is a commit that lowers a constant. For the seven purest leaves — `err`, `pci`, `date`,
`binfmt`, `cred`, `uts`, `seccomp` — that rule is no longer a convention: they are separate crates under
`crates/` (re-exported from `src/lib.rs` as `crate::err` etc.), so a leaf physically cannot name the kernel
and an upward edge from them is a compile error. See `ARCHITECTURE.md` §1.

| Module | Owns | May depend on | Must never |
|---|---|---|---|
| `main.rs` | Boot sequence, milestone checks, panic handler | anything (it is the wiring) | hold reusable logic — that lives in the lib |
| `syscall/` (ABI) | Numbers, `abi.rs` facts, dispatch, handlers | `vfs`, `sched`, `mm`, `drivers`, `net`, services | be a per-device ioctl switchboard (V-2) |
| `vfs/` | `Inode`/`FileSystem`, mounts, fd table, every filesystem, pipes/epoll/timerfd/signalfd | `sched`, `mm`, `blk`, `pci` | downcast to concrete devices through the `Inode` trait (V-1); own the terminal emulator — the emulator is the `term` crate now, the console mux and the line discipline are still here (V-4 half closed) |
| `sched/` | Task table, priority bands, sleep/block, signals, futex | `mm`, `arch`, `binfmt` | host another subsystem's per-process cleanup (V-3) |
| `mm/` | Buddy frames, slab heap, mapping plans, ELF loader | `boot` | know files, tasks or devices |
| `arch/` | GDT/IDT, PIC/LAPIC, paging, context switch, syscall gate, timer, ports | `mm` (via its own re-exports only) | be named from outside `arch::` — `arch/mod.rs` re-exports the uniform surface |
| `console/` | Log ring, serial, framebuffer switch, the six virtual consoles | `arch`, `boot` (the boot-supplied console list), the `term` crate | reach into the VFS for the grid: it names the `term` crate, never `vfs::term` (V-4 half closed) |
| `net/` | smoltcp, socket registry, AF_INET/UNIX/NETLINK/VSOCK | `drivers`, `sched`, `uevent` | validate user pointers outside the `uaccess` leaf (V-9, closed by B3) |
| `drivers/` | virtio, DRM/KMS, render task, evdev/uinput, PCI config, serial, RTC | `mm`, `pci`, `sched` | call back into `syscall` (V-5) or parse ioctl ABI (V-2) |
| Services — leaf crates `err`/`pci`/`date`/`binfmt`/`cred`/`uts`/`seccomp`/`term`/`ahci`/`gpu` (see `crates/`), plus `blk`, `rand`, `uevent`, `logging`, `sync`, `evdev`, `uinput`, `uaccess` | One concern each, pure logic, host-compiled where possible. The leaf crates are downward-only and cannot name the kernel. `err` is the errno table: the leaf every layer may answer with, so nothing below `syscall` names `syscall` for a minus sign | each other and `arch` as drawn | accumulate state that makes them a layer |

Two rules make the table usable rather than decorative:

1. **An existing violation is not precedent.** `ARCHITECTURE.md` §9's V-1…V-5, V-9 and V-13 are edges that
   are real today and are being removed in Phase 11. Code may not add a *new* caller along a violation edge,
   and may not widen one; working around a violation is how it stays removable.
2. **A needed edge changes the design record first.** If a legitimate dependency is genuinely missing from
   §2's graph, update `ARCHITECTURE.md` (§2 diagram and §3 table) in the same commit, with the reason — the
   graph is the contract, so a diff that contradicts it is the diff that is wrong.

### The extending standing rules

The five above govern placement and shape; these seven govern what a change carries with it and what it may
not touch. Each exists because of a mode of failure this repository has actually paid for.

1. **A test is a contract, not an obstacle.** When the gate goes red, the code changes, not the test.
   Deleting or weakening a test to make it pass is the same violation as returning success for something
   unimplemented. Where the test's *expectation* is genuinely wrong — as `t-tty-eintr` was, comparing against
   a two-byte line and the wrong `EINTR` — the commit body carries the measurement proving the kernel's answer
   is Linux's answer.
2. **Every new parser gets a fuzz target.** Any place the kernel reads user- or wire-supplied bytes — an ioctl
   argument, an ELF section, a network packet — joins the targets in `fuzz/` in the same commit, or the change
   says in `PLAN.md` why it does not. The parsers are where the crashes live; that is why the directory exists.
3. **A published ABI is append-only.** Syscall numbers, struct layouts and ioctl encodings are contracts once
   they ship, because a running musl binary reads them at its own address. Changing a shipped shape is the
   wrong-shape answer §4 calls unrecoverable. Renumber, repack or reinterpret something already landed only
   with an explicit migration written down in `PLAN.md`.
4. **A user-driven allocation must answer "what is the worst case".** The global allocator still panics on
   failure (`PLAN.md` §8.0), so unbounded user-controlled growth can end the machine. Until that gap closes,
   any new path whose allocation volume a user program can steer carries a bound — a capped count, a checked
   length, or an honest `ENOMEM` at the limit. This is the **worst-case rule**, and drivers cite it by that name.
5. **Interrupt context does none of: sleep, allocate, or walk an unbounded structure.** An IRQ handler follows
   the hard-interrupt version of the lock-poll-try-unlock-park pattern: bounded work, no lock a preempted task
   may hold, deferral to a lower half or a parked task when the work is not bounded. The uniprocessor rule
   makes this cheap to get right and cheap to lose — a spin on a preempted holder is a deadlock with the
   machine stopped on it.
6. **A performance change ships with before/after numbers.** `PLAN.md` §10 is the ledger and it is only true if
   entries are measurements. An optimisation without a measured cost is how the idle `hlt` bug stayed
   invisible until a tick was being burned on every wake.
7. **One intent per commit.** A refactor and a behaviour change in the same diff cannot carry the §7 body —
   there is no single *why* to write down. Split them: the refactor lands first, green on its own, then the
   change.

### The `no_std` split is the central convention

Everything arch-independent lives in the library and is unit-tested on the host; anything touching port I/O,
descriptor tables, virtio rings, firmware or a real clock is gated behind `#[cfg(target_os = "none")]` (and
`target_arch = "x86_64"` where the hardware is x86-specific). When you add code, ask which half it belongs in,
and gate it. Pure logic (arithmetic, encodings, parsers, policies) belongs on the host side even when its only
caller is bare metal — `src/evdev.rs` and `src/uinput.rs` exist precisely to hold input policy where a host
test can reach it.

The gate is applied per module rather than by a rule, and the rule it is applied *by* is this
(`src/lib.rs`'s header is its home): **a module is host-compiled when its logic touches no
hardware, no port I/O, no virtio ring and no real clock — and then its dependencies must be
host-compiled too, transitively.** The transitive half is the one that gets forgotten. When you
gate a module, check what gates its tests; when you ungated one, check that everything it names is
ungated too. `cargo xtask test` opens with a mechanical check for the one failure the compiler
cannot see — a `#[cfg(test)]` module sitting behind a `target_os = "none"` parent, which is dead
code that reads as coverage (`xtask/src/gates.rs`, closing `ARCHITECTURE.md` V-11).

### Concurrency

- The kernel is **uniprocessor**. Disabling interrupts *is* mutual exclusion; a critical section is the whole
  mechanism.
- Shared state is `critical_section::Mutex<RefCell<T>>`. Never `spin::Mutex` — busy-waiting on a lock held by
  a task this core just preempted is a deadlock. `spin::Once` is used only for one-time hardware init in
  `gdt`/`idt`.
- **Never sleep while holding a lock.** The pattern for a blocking operation is *lock, poll, try, unlock,
  park*; see `src/net/mod.rs` for the canonical example and `ARCHITECTURE.md` §7 for the full rules.
- The idle halt belongs to the scheduler's own loop. A task that `hlt`s while staying runnable costs a whole
  tick on every wake — a real bug with a measured cost (`PLAN.md` §10.2).

### Comments, errors and panics

The house style is long-form prose doc comments that explain **why**, not what: `//!` module headers run to
several paragraphs, and a non-obvious decision names the alternative that was rejected and the evidence for
the choice. Match that. Do not add a comment that restates the code; do not delete an existing rationale
comment while editing around it.

Panicking in the kernel ends the machine (`panic = "abort"` plus a handler that exits QEMU). Syscall handlers
return Linux errno constants through the `err` module and never unwrap user input. Known gap: the global
allocator's failure path is still `panic!`, so unbounded user-driven growth can kill the kernel
(`PLAN.md` §8.0).

---

## 5. Recipes

### Add or change a Linux syscall

1. `src/syscall/linux/nr.rs` — the number and its `pub const`. This file is `#[path]`-included by the test
   harness, so naming it here is what makes it addressable from a test; a host test in that file asserts the
   numbers against their documented values.
2. `src/syscall/linux/mod.rs` — add the arm to the dispatch `match`. Grouped arms (`nr::STAT | nr::LSTAT =>`)
   are normal.
3. The handler goes in `fs.rs` (path/file/device calls), `proc.rs` (memory, process, time) or `posix.rs`
   (identity, capabilities, scheduling, clocks). Argument validation and comment contracts belong with the
   handler; pure ABI facts (flag values, struct layouts, encodings) belong in `abi.rs`, which is host-compiled
   so they can be tested.
4. A syscall with no implementation must fall through to the `_ => ENOSYS` arm — §4 says why a silent success
   is the wrong direction to be wrong in, and what to write in `PLAN.md` instead.
5. Add a conformance program under `user/abitest/src/bin/syscall/t-<name>.rs` **and** a matching `[[bin]]`
   entry in `user/abitest/Cargo.toml` — see the next recipe.
6. Update `PLAN.md`: the "Remaining ABI support" list and the TODO board are the inventory of what is and is
   not answered. Update `UAPI.md` too: its syscall table and its work list are the same inventory kept in one
   place, and a call that moves between served / refused / wrong-answer belongs in both.

### Add a user test program

- `no_std`, raw-syscall test → `user/abitest/src/bin/syscall/t-<name>.rs`, or
  `user/abitest/src/bin/integration/t-<name>.rs` if it needs a device, the network, another process image, or
  is a timing instrument. **You must also add an explicit `[[bin]]` block to `user/abitest/Cargo.toml`** —
  Cargo auto-discovers `src/bin/*.rs` but *not* `src/bin/<subdir>/*.rs`, and `xtask/src/selftest.rs` bails if
  a source produced no binary.
- `std`/`rustix` test → `user/tests/src/bin/t-<name>.rs`. This one is flat, so discovery is automatic.
- Print `ok:`/`FAIL:` per check and a final `PASS`/`FAILED`, exiting 0 on success and non-zero on failure.
  `user/abitest/lib/lin.rs` provides the harness and the syscall stubs; include it with
  `#[path = "../../../lib/lin.rs"]`.
- Keep the name `t-<something>`: the test `/init` globs `/bin/t-*` and the verdict line is keyed on the name.
- **Decide its group in `tests.toml`** — the silence is the default, and the default means "a check of this
  machine and of nothing else". A test that reaches outside (the host's network, a listener on it), measures
  rather than asserts, or takes seconds is named `external`, `slow` or `diagnostic` instead, and is then run
  with `--groups`/`--all-groups`. A diagnostic rather than a check belongs there too, and needs no `PASS` line
  to be worth keeping — `t-sh` is one.

### Add a kernel unit test

Put it in a `#[cfg(test)] mod tests` at the bottom of the file it tests, and make sure the file's parent module
is compiled for the host (see §4). Assert on the *contract* (a value, an errno, a count) rather than on a log
line, and prefer a test that would fail if the policy were implemented differently — the ext4 cache tests, the
evdev window tests and the locking tests are the models.

### Change what the guest image contains

- A file that must exist in the image → the `rootfs/` tree (or `initramfs/` for the early userspace). The
  trees are overlays applied *after* anything written from the toml, so a file in the tree wins.
- A package → `rootfs.toml` (`base_packages` / `extra_packages`) or `initramfs.toml`.
- A partition, filesystem type, image size, hostname or DNS → `rootfs.toml`. The parser is deliberately small:
  `key = value` plus arrays.
- Anything about the *machine* the image boots on — memory, cpus, `vsock`, `audio`, the window backend, the
  standing run mode, extra QEMU arguments → `machine.toml`, **not** `rootfs.toml`. This is a cost rule, not a
  tidy one: `rootfs.toml`'s mtime decides whether the Alpine tree is reinstalled
  (`rootfs::needs_full_install`), so a key unrelated to the image's contents would charge a full package
  install for a memory size. `xtask/src/config.rs`'s `the_machine_and_the_image_keep_their_own_keys` fails if
  a machine key drifts back, and `image_size`/`esp_size` stay in `rootfs.toml` because changing either
  re-makes the partitions.
- Anything about how the machine boots → `mpos.selftest` and the `console=` list, both read in `src/main.rs`
  through `src/boot/console_list.rs`, and the `--mode` that writes the list in `xtask/src/config.rs` /
  `xtask/src/rootfs.rs`. The kernel is told which consoles it has and nothing about how it was launched.

### Touch the boot handoff

The `Handoff` record is written twice: `boot/src/handoff.rs` (UEFI loader) and `src/boot/handoff.rs` (kernel),
plus the `BOOT_STACK_CANARY` constant in `src/main.rs`. There is no shared crate between them by design.
Change all three, and keep the loader's "allocate everything before `exit_boot_services`" ordering intact.

---

## 6. Known traps

| Trap | What to do |
|---|---|
| Bare `cargo test` fails to link/run | Always add `--target x86_64-unknown-linux-gnu` |
| A new test program is silently never built | Add the `[[bin]]` block; `xtask` treats "no binary" as a hard error, but only for sources it finds |
| A file under `src/` that nothing declares looks real | A `mod` declaration is what makes a file part of the build, and a duplicate driver is what happens when it is missing — undeclared copies of the serial and RTC drivers survived here for months (Phase 11 V-7). Check for the declared one before writing a new file |
| A module has a host body and a machine body | **Both move, or the change does not build.** `cargo test` was green while `cargo build --target x86_64-unknown-none` failed: `VfsError::Revoked` was added for `EVIOCREVOKE` and mapped in the host `err_of`, and the `#[cfg(target_os = "none")]` one — the one the machine builds — was left non-exhaustive (`ARCHITECTURE.md` V-11 in its plainest form). Run the bare-metal target on every pass, not only the host suite |
| `cargo xtask run` does not print the milestone markers | By design: the `M*_OK` checks only run with `mpos.selftest`, which `xtask test` sets and a normal boot does not. The markers that exist are M0–M4, M6, M7, M8_IPC and M8 — there is no M5 and nothing above M8 |
| There is no cross-architecture build | x86_64 only; `PLAN.md` §9.1 lists what a port needs |
| `fuzz/` refuses to build from the root workspace | It is its own workspace on purpose; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test does not run | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; a test module annotated for the host inside it is consequently unreachable |
| Something in `user/staging/` looks like a missing test | It is deliberate scratch space; check `user/abitest` and `user/tests` for the live version first |
| `build-test/` holds scripts and logs from past debugging sessions | Scratch. Do not build on them, do not commit them (gitignored as `/build*`) |
| Two `cargo xtask test` runs in one tree report nonsense | They share `build/test-initramfs.tar`, `build/test-esp` and `build/test-serial.log`, so one run reads a log the other is still writing and counts the programs it cannot see as failed. The symptom is a harness line like "115 of 136 did not pass" beside a serial log that ends `=== SUMMARY pass=134 fail=2`. Trust the log's own summary line and `=== END`, and re-run alone; the same collision can flash a transient compile error in a file nobody touched |
| The test machine is not the run machine | `xtask test` boots an initramfs on its own QEMU wiring and `xtask run` boots the disk image on `machine.toml`'s, so a device either one has and the other lacks is a shape no test sees. This has hidden a real bug once: a `q35` machine carries an empty SATA controller that `xtask run` probes and `xtask test` never reaches, and the boot it cost was 28 s (`PLAN.md` §7.3). A driver's behaviour on "a controller with nothing on it" therefore needs naming on the machine that has one — `cargo xtask run`, or a copy of its image with the serial line timestamped — before it is called measured |

---

## 7. Git conventions

Commit subjects follow the existing log: `type(scope): what changed`, with `feat`, `fix`, `docs`, `refactor`
as the common types and subsystem names (`vfs`, `sched`, `net`, `drm`, `ext4`, `syscall`, `xtask`, `console`,
`input`) as the scope; multi-subsystem commits list them (`feat(input,perf,vfs,net): ...`).

The **body carries the why**: what was observed, what was measured, what was rejected. This repository's
history is the project's design record — a commit that only restates the diff is out of place. Use `git log`
on the file you are about to change before you change it; the rationale for most of the structure is in there.

The review question every change gets asked is *is that Linux's answer, or a shape that looks like it?*
Something that cannot be Linux's today says so in the errno it returns and takes a named gap in `PLAN.md` in
the same commit — see §4. A silent approximation does not get to wait for someone to notice it.

Never commit `build/`, `target/`, `build-test/` or anything under `.workbuddy/`, `.qoder/`, `.codely-cli/` —
all gitignored. Never push to a remote unless explicitly asked.

---

## 8. Where the plans live

- `PLAN.md` — the feature roadmap, phase by phase, with a TODO board at the top and a "Suggested next moves"
  list. It is also where each gate run and each performance change is recorded *with its measurement*, and
  where a new plan goes.
- `UAPI.md` — the userspace-visible surface as an inventory: syscalls, device `ioctl` families, structure
  layouts and flag vocabularies, each row carrying Linux's answer, this kernel's, the owning module and the
  pin. It is the working list of what is *served*, *honestly refused*, or a *wrong shape* — the third is the
  only kind that is work, and landing one moves its row.
- `ARCHITECTURE.md` §9 — the architectural debt audit, mirrored in `PLAN.md` Phase 11 with an ordered
  remediation plan.
- If you implement something, update `PLAN.md`'s status — and `UAPI.md`'s row, when the change moves a call or
  a request between the three verdicts — in the same commit. The board is only useful if it is true.
