# AGENTS.md

Guidance for AI agents (and humans) working in this repository.

Read this before your first edit. `README.md` says what MPOS *is*; `ARCHITECTURE.md` says how
it is put together and where the layering is currently violated; `PLAN.md` is the feature
roadmap and the home of any new plan. This file is about **how to work here**.

---

## 1. What this is, in three sentences

MPOS is a uniprocessor x86_64 operating system kernel written in Rust (`no_std`, edition 2024)
that is binary-compatible with the Linux x86_64 ABI, so musl-based binaries — busybox, the
Alpine `apk` tool, weston — run on it unmodified. It boots on UEFI through its own loader
(`mpos-boot`) and is developed with an unusual split: the kernel is a **library** that
compiles for the host so its arch-independent logic can be unit-tested with plain `cargo
test`, plus a thin binary that only holds what must run on the machine. Everything is driven
by `cargo xtask`, and every behaviour is meant to be pinned by a test — a host unit test, a
program booted in QEMU, or both.

---

## 2. Repository map

| Path | What lives there |
|---|---|
| `src/` | The kernel crate `mpos`: `lib.rs` (host-testable) + `main.rs` (boot entry, milestone checks, panic handler) |
| `boot/` | `mpos-boot`, the UEFI loader. Separate target (`x86_64-unknown-uefi`), no dependency on the kernel — they agree on a hand-written handoff record |
| `xtask/` | `mpos-xtask`, the build/run/test front-end. `xtask/README.md` documents every command (bilingual) |
| `user/` | `mpos-user`: ordinary `std` programs for the image (`rustix`, static musl) |
| `user/abitest/` | `mpos-user-abi`: the `no_std` ABI conformance suite. `src/bin/syscall/` (one syscall family) and `src/bin/integration/` (a device, the network, a subsystem end to end) |
| `user/tests/` | `mpos-user-tests`: `std`-side tests for what is easiest to pin from a hosted language |
| `user/staging/` | Scratch sources not wired into any build. Do not assume they are dead — but do not build them either |
| `fuzz/` | `mpos-fuzz`, **its own workspace**. 18 libFuzzer targets over the kernel's parsers |
| `arch/x86_64/linker.ld` | Kernel linker script, referenced from `.cargo/config.toml` |
| `rootfs/`, `rootfs.toml` | The guest image's contents: the overlay tree and the packages, hostname, disk geometry it is built from |
| `machine.toml` | What that image is *booted* on — memory, cpus, devices, the standing run mode. Host-side only, so editing it never reinstalls the image |
| `tests.toml` | Which user test programs the self-test sweep runs, and which groups it leaves out by default |
| `initramfs/`, `initramfs.toml` | The early userspace's contents and package list |
| `build/`, `target/`, `build-test/` | Build output and scratch. Never edit, never commit |

The workspace is declared in the root `Cargo.toml` with
`members = ["user", "user/tests", "user/abitest", "boot", "xtask"]` and
`default-members = ["."]`. That last line is load-bearing: without it a plain `cargo build`
would build the musl user programs for the host.

---

## 3. Build, run, verify

### Prerequisites

```bash
rustup toolchain install nightly
rustup component add rust-src rustfmt clippy --toolchain nightly
rustup target add x86_64-unknown-none x86_64-unknown-uefi \
                   x86_64-unknown-linux-musl x86_64-unknown-linux-gnu
```

Plus, for `xtask run`/`disk`/`rootfs`: `qemu-system-x86_64`, OVMF
(`/usr/share/OVMF/x64/OVMF_CODE.4m.fd`), `qemu-img`, `qemu-nbd`, `sfdisk`, `partx`, `blkid`,
`mkfs.ext4`, `mkfs.fat`, `tar`, and `sudo`. `rootfs`/`initramfs` fetch Alpine packages over
the network.

### The commands that matter

| Command | What it does | Root? |
|---|---|---|
| `cargo xtask build` | Kernel + UEFI loader | no |
| `cargo xtask all` | Kernel, initramfs, disk image, root filesystem | yes |
| `cargo xtask run` | Boot in QEMU, rebuilding if anything is stale | when needed |
| `cargo xtask run --mode terminal\|gui\|mirror\|weston` | Same, with the QEMU wiring and the guest's `console=` list chosen together | when needed |
| `cargo xtask test` | **The gate.** Host unit tests, then every user test program booted in QEMU | no |
| `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` | Just the host unit tests | no |
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | xtask's own unit tests | no |
| `cargo xtask clean` | Remove `build/` and `target/` | when needed |

### Two traps in the build configuration

1. **`--target` is mandatory for anything host-side.** The root `.cargo/config.toml` sets
   `[build] target = "x86_64-unknown-none"`, so a bare `cargo test` tries to build the tests
   for bare metal and cannot run them. Always pass
   `--target x86_64-unknown-linux-gnu` for host work.
2. **`user/abitest` is built with overridden flags.** Its `x86_64-unknown-none` profile uses
   the *kernel's* linker script, so `cargo xtask test` builds it with
   `RUSTFLAGS=-C relocation-model=static` from inside `user/abitest/`. Building it by hand
   without that will produce binaries the loader will not accept.

### Formatting and linting

```bash
cargo fmt --check
cargo clippy --target x86_64-unknown-none            # the kernel, bare metal
cargo clippy --target x86_64-unknown-linux-gnu       # the kernel, host
cargo clippy -p mpos-xtask --target x86_64-unknown-linux-gnu
```

### The self-test protocol (`cargo xtask test`)

Stage 1 runs the host unit tests. Stage 2 builds `mpos-user-tests` (musl) and `mpos-user-abi`
(bare metal), packs them into a throwaway initramfs with a generated `/init`, and boots it
headless with `mpos.selftest`. That `/init` runs every `/bin/t-*`, and the serial log is
parsed for:

```
=== RUN  t-name
=== PASS t-name
=== FAIL t-name exit=N
=== SUMMARY pass=N fail=N
=== END
```

The run must end with a summary **and** QEMU exit code 33 (the `isa-debug-exit` clean exit).
A test that hangs, crashes or never prints `=== PASS` counts as failed. There is no
`#[ignore]`-style escape hatch here — a test that cannot work must be deleted or made to pass.

**Which programs stage 2 boots is `tests.toml` at the repository root.** Every test is built;
the ones in a group are left out of the default sweep, and the *staging* is the selection — the
guest runs whatever `/bin/t-*` it was given. A test in no group always runs, so the default
sweep is "everything that is a check of this machine", and a red one means the kernel rather
than the host's network or a busy host. The shipped groups are `external` (needs the host's
network or a listener on it), `slow` (measurement, or seconds of work) and `diagnostic` (not a
check at all); `cargo xtask test --groups <a,b>` runs an explicit list in place of the file's
`default`, and `--all-groups` runs everything. A group name that the file does not define — on
the command line, or in the file naming a program that does not exist — is an error, because a
sweep that quietly runs fewer tests than it was asked to is the one failure this must not have.
A new test therefore lands with a decision about its group, and its silence is the default.

Stage 3 is a non-event by design: the test initramfs is a separate archive under `build/`, and
the boot disk is a directory QEMU synthesizes as virtual FAT, so nothing from a test run can
leak into the image `cargo xtask run` boots.

### Current verification state (as of this commit)

**Nothing is red. The state below is the measured run, quoted from its own summary line.**

- Host unit tests: `582 passed` (`cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`),
  plus `36 passed` from `mpos-xtask`. Measured on this tree.
- The guest suite reached `=== SUMMARY pass=129 fail=0` (129 built) with QEMU exit code 33, with
  `9 of 138 user tests left out by tests.toml` — the `external`, `slow` and `diagnostic` groups,
  which is what makes the sweep's verdict a statement about the kernel. `--groups diagnostic`
  was run on its own to prove the flag reaches the guest (`128 staged`, `t-sh` in the count).
  `/proc/<pid>/maps` (plan §7.4 and §Remaining) is green end to end: `t-maps` printed
  `=== PASS` over 29 checks, and the ones that matter are the lines a crash reporter reads —
  the program's own image reported under its own path with the code segment `r-xp` (which is
  *not* in the VMA table, since `execve` fills those pages eagerly), a private anonymous mapping
  as `rw-p` with no name and no object, a shared one as `rw-s` named `/memfd: (deleted)`, a
  `memfd` mapping named `/memfd:t-maps-buf (deleted)` with its name starting in Linux's column
  73, and `[stack]`/`[heap]` covering the frame and the break.
  The UTS namespace (plan §6.3, step 1) is green end to end: `t-ns-uts` prints `=== PASS`,
  holding both halves of the property that makes a namespace one — a child that unshares reports
  `inside` through `uname` *and* through both `/proc/sys/kernel` files while the parent that did
  not unshare keeps the names it started with, and a child forked *before* a refused
  `CLONE_NEWUTS|CLONE_NEWNS` word still shares the parent's namespace, which is what pins "a
  refusal unshares nothing" rather than merely asserting the errno.
  The ABI-honesty batch is green in the booted machine: `t-prctl`, `t-times`, `t-priority`,
  `t-fadvise`, `t-evdev`, `t-ioctl`, `t-openat`, `t-statx`, `t-truncate`, `t-rseq` and
  `t-posix` all print `=== PASS`, and so do the two programs that were the gate's standing red
  baseline for the whole of Phase 12:
  - `t-dsp` — was red because the sound driver looked for the wrong PCI ID (`0x1055`; the card is
    `0x1059`), so `/dev/dsp` was never registered, and then panicked the kernel on its first
    write because the driver handed a **user** address to the DMA mapping. Both fixed
    (`PLAN.md` §12.4a). The boot line is now
    `virtio-sound: 44100 Hz, 2 channels at /dev/dsp`.
  - `t-dhcp` — was red because the *test* built a malformed DISCOVER (IPv4 destination written at
    the UDP port offset, BOOTP magic cookie 28 bytes early), so slirp correctly ignored it. The
    kernel's `AF_PACKET` receive path was never at fault, and no assertion was relaxed to get
    here.
- One thing this tree got wrong and the host suite could not see: `VfsError::Revoked` was
  added for `EVIOCREVOKE` and mapped in one `err_of`, but the `err_of` the machine builds is
  the *other*, `#[cfg(target_os = "none")]` one, and it was left non-exhaustive — so
  `cargo build --target x86_64-unknown-none` failed while `cargo test` was green. Fixed by
  mapping `Revoked` to `ENODEV` in the machine arm. This is `ARCHITECTURE.md` V-11's trap in
  its plainest form: when a module has a host body and a machine body, both move or the
  change does not build. Run the bare-metal target on every pass, not only the host suite.

The standing rule is unchanged: never report "tests pass" from a stale run. Re-run the gate,
and quote the summary line and the exit code it printed — or say plainly that it did not get
there.

---

## 4. Code conventions

### Every design here is Linux's behaviour — never a toy version of it

This is the rule that governs every other convention in this file, so it comes
first among them.

MPOS exists so that **unmodified Linux binaries run on it**. That means the value
of anything added here is exactly how well it answers the question a real program
actually asks, and it also means the failure modes are asymmetric: a question
answered with the wrong *shape* sends the caller down a path it cannot recover
from, while a question answered with the honest errno sends it down its own
fallback. Nearly-right is worse than absent.

So there are no toy implementations, no "temporary" designs, and nothing meant to
be filled in later. Concretely:

1. **Measure before designing.** Behaviour a real program depends on is observed
   on a real Linux first, and the observation is written down with the number
   next to the claim (§4.1's zero-byte `recvmsg`, §7.2's `EVIOCGPHYS` errno and
   §8.2's kill actions all started as a measurement). A design argued from a man
   page alone has repeatedly been wrong, because the man page does not say which
   half of the rule a caller actually branches on.
2. **Answer truthfully, or refuse.** If this kernel cannot do what Linux does,
   it says so in the only language a caller reads: `ENOSYS` for a call that does
   not exist here, `EOPNOTSUPP` for one that exists but cannot do that,
   `EINVAL` for an argument it refuses, `EACCES`/`EPERM` for one it will not
   grant. The `_ => ENOSYS` arm in `src/syscall/linux/mod.rs` is an honest
   answer and should stay crowded. **Returning success for something not
   implemented is the single worst thing this codebase can do** — it is how a
   program comes to believe something false and stops asking.
3. **No private dialects.** A flag, struct or protocol gets its Linux shape, not
   a shape this kernel's implementation finds convenient. Two designs that are
   behaviourally the same today split the moment a third program reads them.
4. **Refuse rather than approximate.** When the honest alternative to doing it
   properly is doing it *nearly*, refuse: an action this machine does not
   implement is `EOPNOTSUPP` from its availability probe instead of quietly
   allowed (§8.2), and a namespace that isolates nothing is `EOPNOTSUPP` rather
   than a namespace (§6.3).
5. **A gap gets a name, not a stub.** Whatever cannot be built today is recorded
   in `PLAN.md` as a named gap with its measurement and its missing prerequisite
   beside it — the board's `[~]` entries exist to hold exactly those. A `TODO`
   in shipped code, a handler that does nothing and returns 0, or a shape that
   looks finished while silently doing less, are all the same violation.
6. **A gap gets a pin, or it is not done.** A behaviour landed is a behaviour: host unit test, `t-*` program, or both (§5 says where
   each kind goes). "Works, verified by hand once" is how this repository lost
   the same input bug four times.

The rejected alternative is "implement the common case now and tighten it later".
Its cost has been paid here several times, and it is always paid in debugging
rather than in writing: udevd untagging every device on the machine, libinput
dropping every node, a compositor logging a device and then stopping with no
pointer — each came from an answer that was *nearly* right, and each took longer
to find than silence would have.

### The five standing checklist rules

Every change is checked against these five before it lands. They are a checklist,
not decoration — the first governs *what* may go in, the next two *how* it must
be shaped, and the last two *what* it must carry with it.

1. **Kernel-space discipline.** Follow the Linux standard for what belongs in a
   kernel: a non-kernel feature stays out of the kernel. Policy that Linux runs
   in userspace (device management, filesystem utilities, networking daemons) is
   not this kernel's job; the kernel provides the ABI userspace expects and
   nothing more.
2. **High cohesion, low coupling.** Before adding a feature, locate the module
   that owns the concept, and keep the call graph between modules narrow. If the
   change would reach into another module's internals, that is a signal the
   feature is being placed in the wrong module.
3. **No toy or temporary designs.** Every feature is implemented the Linux way —
   the same flags, structs, errnos and semantics, measured on real Linux where a
   program branches on them. This is the governing rule above stated as a
   checklist item; it is repeated here so it is caught on every pass, and it is
   never waived for convenience.
4. **Every behaviour ships with a pin that would fail without it.** A bug fix or
   a new feature is landed only when the same commit carries a test that would
   have failed before the change and passes after it — a new unit test, a case
   added to the existing test module, or a `t-*` program that covers the
   area. The judgement criterion is exactly that counterfactual: a test that
   passes with and without the change pins nothing, so it does not count as the
   pin. A fix without a pin is how the same input bug was lost four times; a
   feature without a pin is how a gap gets reported as done.
5. **Respect the designed dependency graph.** Modules depend on each other only
   along the edges the design allows. Before adding a cross-module dependency,
   draw (or update) the dependency graph first — `ARCHITECTURE.md` is where the
   layering facts live — and make the change fit it, not the other way round. A
   new upward or back edge (e.g. a VFS module calling into a driver's private
   internals) is a design violation even when it compiles.

### Module boundaries

Rule 5 says to respect the graph; this is the graph. `ARCHITECTURE.md` §2 holds
the full mermaid diagram with every real `crate::` reference, §3 the ownership
table and §9 the measured violations — this table is the working copy, and the
two files must move together.

The intended shape is a one-way stack. A module may call **downward only**
into what the table's "may depend on" column lists; everything else is a
boundary violation.

| Module | Owns | May depend on | Must never |
|---|---|---|---|
| `main.rs` | Boot sequence, milestone checks, panic handler | anything (it is the wiring) | hold reusable logic — that lives in the lib |
| `syscall/` (ABI) | Numbers, `abi.rs` facts, dispatch, handlers | `vfs`, `sched`, `mm`, `drivers`, `net`, services | be a per-device ioctl switchboard (V-2) |
| `vfs/` | `Inode`/`FileSystem`, mounts, fd table, every filesystem, pipes/epoll/timerfd/signalfd | `sched`, `mm`, `blk`, `pci` | downcast to concrete devices through the `Inode` trait (V-1); own the terminal emulator (V-4) |
| `sched/` | Task table, priority bands, sleep/block, signals, futex | `mm`, `arch`, `binfmt` | host another subsystem's per-process cleanup (V-3) |
| `mm/` | Buddy frames, slab heap, mapping plans, ELF loader | `boot` | know files, tasks or devices |
| `arch/` | GDT/IDT, PIC/LAPIC, paging, context switch, syscall gate, timer, ports | `mm` (via its own re-exports only) | be named from outside `arch::` — `arch/mod.rs` re-exports the uniform surface |
| `console/` | Log ring, serial, framebuffer switch, the six virtual consoles | `arch`, `boot` (the boot-supplied console list) | reach into `vfs::term` for the grid (V-4) |
| `net/` | smoltcp, socket registry, AF_INET/UNIX/NETLINK/VSOCK | `drivers`, `sched`, `uevent` | validate user pointers with `syscall::buffer_ok` (V-9) |
| `drivers/` | virtio, DRM/KMS, render task, evdev/uinput, PCI config, serial, RTC | `mm`, `pci`, `sched` | call back into `syscall` (V-5) or parse ioctl ABI (V-2) |
| Services — `blk`, `pci`, `rand`, `uevent`, `binfmt`, `date`, `logging`, `sync`, `evdev`, `uinput`, `seccomp`, `err` | One concern each, pure logic, host-compiled where possible. `err` is the errno table: the leaf every layer may answer with, so nothing below `syscall` names `syscall` for a minus sign | each other and `arch` as drawn | accumulate state that makes them a layer |

Two rules make the table usable rather than decorative:

1. **An existing violation is not precedent.** §9's V-1…V-5, V-9 and V-13 are edges
   that are real today and are being removed in Phase 11. Code may not add a
   *new* caller along a violation edge, and may not widen one; working around
   a violation is how it stays removable.
2. **A needed edge changes the design record first.** If a legitimate new
   dependency is genuinely missing from §2's graph, update `ARCHITECTURE.md`
   (§2 diagram and §3 table) in the same commit, with the reason — the graph is
   the contract, so a diff that contradicts it is the diff that is wrong.

### The extending standing rules

The five above govern placement and shape; these seven govern what a change
carries with it and what it may not touch. Each exists because of a mode of
failure this repository has actually paid for.

1. **A test is a contract, not an obstacle.** When the gate goes red, the code
   changes, not the test. Deleting or weakening a test to make it pass is the
   same violation as returning success for something unimplemented. Where the
   test's *expectation* is genuinely wrong — as `t-tty-eintr` was, comparing
   against `-lin::EINTR` and a two-byte line — the commit body carries the
   measurement that proves the kernel's answer is Linux's answer.
2. **Every new parser gets a fuzz target.** Any place the kernel reads
   user- or wire-supplied bytes — an ioctl argument, an ELF section, a network
   packet — joins the 18 targets in `fuzz/` in the same commit, or the change
   says in `PLAN.md` why it does not. The parsers are where the crashes live;
   that is the whole reason the directory exists.
3. **A published ABI is append-only.** Syscall numbers, struct layouts and ioctl
   encodings are contracts once they ship, because a running musl binary reads
   them at its own address. Changing a shipped shape is exactly the wrong-shape
   answer §4 calls unrecoverable: it sends every caller down a path it cannot
   detect. Renumber, repack or reinterpret something already landed only with an
   explicit migration written down in `PLAN.md`.
4. **A user-driven allocation must answer "what is the worst case".** The
   global allocator still panics on failure (`PLAN.md` §8.0), so unbounded
   user-controlled growth can end the machine. Until that gap is closed, any
   new path whose allocation volume a user program can steer carries a bound —
   a capped count, a checked length, or an honest `ENOMEM` at the limit.
5. **Interrupt context does none of: sleep, allocate, or walk an unbounded
   structure.** An IRQ handler follows the hard-interrupt version of the
   lock-poll-try-unlock-park pattern: bounded work, no lock that a preempted
   task may hold, deferral to a lower half or a parked task when the work is
   not bounded. The uniprocessor rule makes this cheap to get right and cheap
   to lose — a spin on a preempted holder is the §4 deadlock, with the machine
   stopped on it.
6. **A performance change ships with before/after numbers.** `PLAN.md` §10 is
   the ledger and it is only true if entries are measurements. An optimisation
   without a measured cost is how the idle `hlt` bug stayed invisible until a
   tick was being burned on every wake — if there is no number, it is not a
   performance change, it is a guess, and guesses go through the normal review.
7. **One intent per commit.** A refactor and a behaviour change in the same
   diff cannot carry the §7 body — there is no single *why* to write down, and
   the review question ("is that Linux's answer?") cannot be asked of a diff
   that is answering two questions. Split them: the refactor lands first, green
   on its own, then the change.

### The `no_std` split is the central convention

Everything arch-independent lives in the library and is unit-tested on the host; anything
touching port I/O, descriptor tables, virtio rings, firmware or a real clock is gated behind
`#[cfg(target_os = "none")]` (and `target_arch = "x86_64"` where the hardware is x86-specific).
When you add code, ask which half it belongs in, and gate it accordingly. Pure logic
(arithmetic, encodings, parsers, policies) belongs on the host side even when its only caller
is bare metal — `src/evdev.rs` and `src/uinput.rs` exist precisely to hold input policy where
a host test can reach it.

The gate is applied per module rather than by a rule, and it has already drifted (see
`ARCHITECTURE.md` V-11: a `#[cfg(all(test, not(target_os = "none")))]` module behind a
`cfg(target_os = "none")` parent, so the tests never run). When you gate a module, check what
gates its tests.

### Concurrency

- The kernel is **uniprocessor**. Disabling interrupts *is* mutual exclusion; a critical
  section is the whole mechanism.
- Shared state is `critical_section::Mutex<RefCell<T>>`. Never `spin::Mutex` — busy-waiting
  on a lock held by a task this core just preempted is a deadlock. `spin::Once` is used only
  for one-time hardware init in `gdt`/`idt`.
- **Never sleep while holding a lock.** The pattern for a blocking operation is *lock, poll,
  try, unlock, park*; see `src/net/mod.rs` for the canonical example.
- The idle halt belongs to the scheduler's own loop. A task that `hlt`s while staying runnable
  costs a whole tick on every wake — that was a real bug with a measured cost (`PLAN.md` §10.2).

### Comments and documentation

The house style is long-form prose doc comments that explain **why**, not what:
`//!` module headers run to several paragraphs, and a non-obvious decision gets a comment that
names the alternative that was rejected and the evidence for the choice. Match that. Do not
add a comment that restates the code; do not delete an existing rationale comment while
editing around it.

### Errors, no panics in the kernel

Panicking in the kernel ends the machine (`panic = "abort"` plus a panic handler that exits
QEMU). Syscall handlers return Linux errno constants through the `err` module and never
unwrap user input. Note the known gap: the global allocator's failure path is still `panic!`,
so unbounded user-driven growth can kill the kernel (`PLAN.md` §8.0).

---

## 5. Recipes

### Add or change a Linux syscall

1. `src/syscall/linux/nr.rs` — the number and its `pub const`. This file is `#[path]`-included
   by the test harness, so naming it here is what makes it addressable from a test. A host
   test in that file asserts numbers against their documented values.
2. `src/syscall/linux/mod.rs` — add the arm to the dispatch `match`. Grouped arms
   (`nr::STAT | nr::LSTAT =>`) are normal.
3. The handler goes in `fs.rs` (path/file/device calls), `proc.rs` (memory, process, time) or
   `posix.rs` (identity, capabilities, scheduling, clocks). Argument validation and comment
   contracts belong with the handler; pure ABI facts (flag values, struct layouts, encodings)
   belong in `abi.rs`, which is host-compiled so they can be tested.
4. A syscall with no implementation must fall through to the `_ => ENOSYS` arm.
   Do not invent a silent success, and do not ship a handler that answers 0 while
   doing less than Linux does — see the rule above for why that is the wrong
   direction to be wrong in, and for what to write in `PLAN.md` instead.
5. Add a conformance program under `user/abitest/src/bin/syscall/t-<name>.rs` **and** a
   matching `[[bin]]` entry in `user/abitest/Cargo.toml` — see the next recipe.
6. Update `PLAN.md`: the "Remaining ABI support" list and the TODO board are the project's
   inventory of what is and is not answered.

### Add a user test program

- `no_std`, raw-syscall test → `user/abitest/src/bin/syscall/t-<name>.rs`, or
  `user/abitest/src/bin/integration/t-<name>.rs` if it needs a device, the network, another
  process image or is a timing instrument.
  **You must also add an explicit `[[bin]]` block to `user/abitest/Cargo.toml`.** Cargo
  auto-discovers `src/bin/*.rs` but *not* `src/bin/<subdir>/*.rs`, and
  `xtask/src/selftest.rs` bails if a source produced no binary.
- `std`/`rustix` test → `user/tests/src/bin/t-<name>.rs`. This one is flat, so discovery is
  automatic.
- The program must print `ok:`/`FAIL:` per check and a final `PASS`/`FAILED`, exiting 0 on
  success and non-zero on failure. `user/abitest/lib/lin.rs` provides the harness and the
  syscall stubs; include it with `#[path = "../../../lib/lin.rs"]`.
- Keep the name `t-<something>`: the test `/init` globs `/bin/t-*` and the verdict line is
  keyed on the name.
- **Decide its group in `tests.toml`** — the silence is the default, and the default means "it
  is a check of this machine and of nothing else". A test that reaches outside (the host's
  network, a listener on the host), or measures rather than asserts, or takes seconds, is named
  in `external`, `slow` or `diagnostic` instead, and is then run with `--groups`/`--all-groups`.
  A program that is a diagnostic rather than a check belongs there too, and needs no `PASS`
  line to be worth keeping — `t-sh` is one.

### Add a kernel unit test

Put it in a `#[cfg(test)] mod tests` at the bottom of the file it tests, and make sure the
file's parent module is compiled for the host (see §4). Assert on the *contract* (a value, an
errno, a count) rather than on a log line. Prefer a test that would fail if the policy were
implemented differently — the ext4 cache tests, the evdev window tests and the locking tests
are the models to copy.

### Change what the guest image contains

- A file that must exist in the image → drop it in the `rootfs/` tree (or the `initramfs/`
  tree for the early userspace). The trees are overlays applied *after* anything written from
  the toml, so a file in the tree wins.
- A package → `rootfs.toml` (`base_packages` / `extra_packages`) or `initramfs.toml`.
- A partition, filesystem type, image size, hostname or DNS → `rootfs.toml`. The parser is
  deliberately small: `key = value` plus arrays.
- Anything about the *machine* the image boots on — memory, cpus, `vsock`, `audio`, the
  window backend, the standing run mode, extra QEMU arguments → `machine.toml`, **not**
  `rootfs.toml`. This is a cost rule and not a tidy one: `rootfs.toml`'s mtime is what decides
  whether the Alpine tree is reinstalled (`rootfs::needs_full_install`), so a key that has
  nothing to do with the image's contents would charge a full package install for a memory
  size. `xtask/src/config.rs`'s `the_machine_and_the_image_keep_their_own_keys` fails if a
  machine key drifts back, and `image_size`/`esp_size` stay in `rootfs.toml` because changing
  either re-makes the partitions.
- Anything about how the machine boots → `mpos.selftest` and the `console=` list, both read in
  `src/main.rs` through `src/boot/console_list.rs`, and the `--mode` that writes the list in
  `xtask/src/config.rs` / `xtask/src/rootfs.rs`. The kernel is told which consoles it has and
  nothing about how it was launched.

### Touch the boot handoff

The `Handoff` record is written twice: `boot/src/handoff.rs` (UEFI loader) and
`src/boot/handoff.rs` (kernel), plus the `BOOT_STACK_CANARY` constant in `src/main.rs`. There
is no shared crate between them by design. Change all three, and keep the loader's
"allocate everything before `exit_boot_services`" ordering intact.

---

## 6. Known traps

| Trap | What to do |
|---|---|
| Bare `cargo test` fails to link/run | Always add `--target x86_64-unknown-linux-gnu` |
| A new test program is silently never built | Add the `[[bin]]` block; `xtask` treats "no binary" as a hard error, but only for sources it finds |
| A file under `src/` that nothing declares looks real | That is how `arch/x86_64/serial.rs` and `arch/x86_64/qemu.rs` survived uncompiled for months (Phase 11 V-7). They are **deleted now**, along with the unreferenced `arch/x86_64/pci.rs`, and `drivers/rtc.rs` is `drivers/rtc_dev.rs` (`arch::x86_64::rtc` is the clock; this one is the node). The trap outlives them: a `mod` declaration is what makes a file part of the build, and a duplicate driver is what happens when it is missing |
| `cargo xtask run` does not print the milestone markers | By design: the `M*_OK` checks only run with `mpos.selftest`, which `xtask test` sets and a normal boot does not. The markers that exist are M0–M4, M6, M7, M8_IPC and M8 — there is no M5 and nothing above M8 |
| `README.md` used to promise aarch64 | Fixed in Phase 11 Step A: the `aarch64-unknown-none` profile is gone from `.cargo/config.toml` (and the target from `rust-toolchain.toml`), because it named `arch/aarch64/linker.ld` and no such script exists. There is no cross-architecture build until a port is written; `PLAN.md` §9.1 lists what one needs |
| `fuzz/` refuses to build from the root workspace | It is its own workspace on purpose; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test does not run | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; `net/socket.rs`'s test module is annotated for the host and is consequently unreachable |
| Something in `user/staging/` looks like a missing test | It is deliberate scratch space; check `user/abitest` and `user/tests` for the live version first |
| `build-test/` contains scripts and logs from past debugging sessions | Scratch. Do not build on them, do not commit them (they are gitignored as `/build*`) |
| Two `cargo xtask test` runs in one tree report nonsense | They share `build/test-initramfs.tar`, `build/test-esp` and `build/test-serial.log`, so one run reads a log the other is still writing and counts the programs it cannot see as failed. The measured symptom is a harness line like "115 of 136 did not pass" beside a serial log that ends `=== SUMMARY pass=134 fail=2`. Trust the log's own summary line and `=== END`, and re-run alone; the same collision can flash a transient compile error in a file nobody touched |

---

## 7. Git conventions

Commit subjects follow the existing log: `type(scope): what changed`, with `feat`, `fix`,
`docs`, `refactor` as the common types and subsystem names (`vfs`, `sched`, `net`, `drm`,
`ext4`, `syscall`, `xtask`, `console`, `input`) as the scope. Multi-subsystem commits list
them (`feat(input,perf,vfs,net): ...`).

The **body carries the why**: what was observed, what was measured, what was rejected. This
repository's history is the project's design record — a commit that only restates the diff is
out of place. Use `git log` on the file you are about to change before you change it; the
rationale for most of the structure is in there.

The review question every change here gets asked is *is that Linux's answer, or a shape that
looks like it?* Something that cannot be Linux's today says so in the errno it returns and
takes a named gap in `PLAN.md` in the same commit — see §4. A silent approximation does not
get to wait for someone to notice it.

Never commit `build/`, `target/`, `build-test/` or anything under `.workbuddy/`, `.qoder/`,
`.codely-cli/` — all gitignored.

Never push to a remote unless explicitly asked.

---

## 8. Where the plans live

- `PLAN.md` — the feature roadmap, phase by phase, with a TODO board at the top and a
  "Suggested next moves" list. It also records what was *measured* for each performance
  change, and it is the place to append a new plan.
- `ARCHITECTURE.md` §9 — the architectural debt audit, with the same items mirrored in
  `PLAN.md` Phase 11 and an ordered remediation plan.
- If you implement something, update `PLAN.md`'s status in the same commit. The board is only
  useful if it is true.
