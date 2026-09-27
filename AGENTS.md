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
| `rootfs/`, `rootfs.toml` | The guest image's contents and boot configuration |
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
| `cargo xtask run --mode terminal\|gui\|mirror\|weston` | Same, with the window/serial/console policy chosen together | when needed |
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

Stage 3 is a non-event by design: the test initramfs is a separate archive under `build/`, and
the boot disk is a directory QEMU synthesizes as virtual FAT, so nothing from a test run can
leak into the image `cargo xtask run` boots.

### Current verification state (as of this commit)

The tree is green: `cargo xtask test` ends with `498 passed` host unit tests, `29` from xtask,
`=== SUMMARY pass=120 fail=0` from the guest suite and QEMU exit `33`. The in-flight refactor
this section used to warn about — the `user/abitest` programs moving into `syscall/` and
`integration/`, with edits across `vfs/ext4`, `vfs/tty.rs` and the syscall layer — is finished
and committed, and `PLAN.md` §11.1.12 records the resolution.

The standing rule is unchanged, and it is about the *run* rather than the tree: never report
"tests pass" from a stale run. Re-run the gate, and quote the summary line and the exit code it
printed.

---

## 4. Code conventions

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
4. A syscall with no implementation must fall through to the `_ => ENOSYS` arm. Do not invent a
   silent success.
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
- A partition, filesystem type, image size, hostname, DNS, or QEMU memory/display/console →
  `rootfs.toml`. The parser is deliberately small: `key = value` plus arrays.
- Anything about how the machine boots → the `mpos.*` words read in `src/main.rs` and the
  `--mode` that sets them in `xtask/src/qemu.rs`.

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
| `arch/x86_64/serial.rs` and `arch/x86_64/qemu.rs` look real | They are **orphans**: not declared in `arch/x86_64/mod.rs`, so not compiled at all. `qemu.rs` is byte-identical to `drivers/qemu.rs`. `arch/x86_64/pci.rs` is a stale earlier copy of `drivers/pci.rs`. Edit `drivers/`, not these |
| `cargo xtask run` does not print the milestone markers | By design: the `M*_OK` checks only run with `mpos.selftest`, which `xtask test` sets and a normal boot does not. The markers that exist are M0–M4, M6, M7, M8_IPC and M8 — there is no M5 and nothing above M8 |
| `README.md` promises aarch64 | `.cargo/config.toml` names `arch/aarch64/linker.ld`, which does not exist. The target is aspirational and will not link |
| `fuzz/` refuses to build from the root workspace | It is its own workspace on purpose; run `cargo fuzz` from inside `fuzz/` |
| A `net/*` host test does not run | `src/lib.rs` gates the whole `net` module on `target_os = "none"`; `net/socket.rs`'s test module is annotated for the host and is consequently unreachable |
| Something in `user/staging/` looks like a missing test | It is deliberate scratch space; check `user/abitest` and `user/tests` for the live version first |
| `build-test/` contains scripts and logs from past debugging sessions | Scratch. Do not build on them, do not commit them (they are gitignored as `/build*`) |

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
