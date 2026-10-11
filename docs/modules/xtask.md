# xtask

**Home:** `xtask/` · **Owns:** the build, run and test front-end — one command that folds the old `scripts/mkdisk.sh` and `scripts/run.sh` into `cargo xtask <command>`.

## What it owns

`xtask` is the only supported way to build, boot and sweep the kernel, and what it
owns is the *sequence* plus the privilege model around it. The command is reached
through the alias in the root `Cargo.toml`
(`xtask = "run --quiet --package mpos-xtask --target x86_64-unknown-linux-gnu --"`),
so `cargo xtask all` first builds this crate for the host and then runs it. The
privilege model is the one the scripts had: `cargo` compiles as the invoking user;
root is taken only for the steps that genuinely need it (writing the image through
the block layer, mounting it, `chroot`); and whatever root wrote is handed back to
the invoking uid/gid afterwards (`main.rs`'s `hand_back`, `utils::repair_ownership`).
The reasoning behind every command — the flags, the artefacts, the config keys, the
three `run` modes — lives in `xtask/README.md`, which is the command reference. This
page does not duplicate its tables; it says what the modules are and where the shared
logic sits.

The second thing this crate owns is the **machine switch**. `--arch` is parsed once,
up front, into `arch::Arch` (`xtask/src/arch.rs`), and every command names the machine
by that value instead of comparing the string again. That is why `build --arch aarch64`
really compiles the aarch64 kernel and loader: the switch used to reach only `run` and
`test`, and `build` silently compiled x86_64 whatever `--arch` said. An unrecognised
machine is refused by name (`Arch::parse`), not defaulted to x86_64.

What deliberately does **not** live here: anything that belongs to the kernel's own
build (the `no_std` split, the linker scripts) or to the guest (the `/init` scripts,
the test protocol's *runner script*). `xtask` decides *what* is booted and *which*
programs are staged; the shared `TEST_INIT` shell script it writes into the test
initramfs is the guest's half of the protocol, and the verdict lines are read back
out of the serial log.

## Key files

| File | What lives there |
|---|---|
| `src/main.rs` | the command set (`Cli`/`Commands`), `dispatch`, the resolved `Invocation`, and the whole privilege dance (`build_under_sudo`, `escalate`, `hand_back`) |
| `src/arch.rs` | `Arch` — the machines this front end builds, and every per-machine fact (target triples, image names) answered once |
| `src/config.rs` | the small TOML-subset parser for `machine.toml`, `rootfs.toml`, `initramfs.toml` and the flat `tests.toml`; the sectioned files' `[default]`/`[x86_64]`/`[aarch64]` semantics |
| `src/disk.rs` | the `disk` command (the raw image) and `sweep_medium`, the rootless GPT staging disk a test sweep boots |
| `src/fat.rs` | the FAT32 writer used to put boot files on the ESP without mounting it (long-name entries included) |
| `src/initramfs.rs` | the `initramfs` command: one pipeline, two machines — repos, apk, busybox applet farm, overlay, ustar |
| `src/rootfs.rs` | the `rootfs` command: install the Alpine tree into the disk image through the block layer, refresh the boot files |
| `src/qemu.rs` | the `run` command: the QEMU wiring, `--mode`/`console=` selection, the accelerator and `machine.toml` |
| `src/selftest.rs` | the `test` command: stage 1 host unit tests, the test kernel (`utils::build_test_kernel`, `--features selftest`, copied out of the shared target directory), the two user suites, the throwaway test initramfs (`TEST_INIT`) and the verdict reader - which asserts the kernel's own `mpos-kunit: N passed, M failed` line beside the sweep's summary; `run_disk_sweep` for aarch64 |
| `src/sweep.rs` | the half of a sweep that is not the machine: what `tests.toml` means, `select`/`Selection`, the `=== RUN/PASS/FAIL/SUMMARY/END` protocol, and `kunit()` - the kernel's own `mpos-kunit: N passed, M failed` line, which is the other half of the verdict |
| `src/gates.rs` | the target-gate check run ahead of stage 1: a `#[cfg(test)]` module behind a `target_os = "none"` parent is a test that has never run |
| `src/depgraph.rs` | the module-graph measurement and ratchet behind `cargo xtask depgraph` |
| `src/utils.rs` | `build_kernel`, build-dir/profile resolution, ownership repair, `run_status` and the filesystem/process helpers every command shares |
| `README.md` | the full command, flag and artefact reference |

There is no separate `build.rs` module: "build the kernel and loader" is
`utils::build_kernel`, called from `dispatch`'s `Commands::Build` arm and from the
commands that need artefacts current before they escalate.

## Contracts

- `cargo xtask <command>` is the contract with the user; the command table and every
  flag is `xtask/README.md`'s, and the `cargo xtask` alias is the root `Cargo.toml`'s.
- `dispatch` is entered with one `Invocation` (arch, build dir, profile, whether
  artefacts are ready, whether `run` must build, the resolved boot mode) and one
  `sweep::Selection` (the `--groups`/`--all-groups` flags). Those two structs are the
  contract between `main`'s pre-dispatch steps and the per-command bodies.
- The `test` arm is where the machines genuinely differ and where the front end
  refuses to lie: `Arch::X86_64` runs `selftest::run_tests`, `Arch::Aarch64` runs
  `selftest::run_disk_sweep` and *refuses* `--groups`/`--all-groups` by name, because
  `tests.toml`'s groups name x86_64's programs.
- `sweep::select`/`sweep::Selection` own what a run boots and what it leaves out, so
  the two arms share one answer to "what does `tests.toml` mean" and keep only the
  payload location machine-specific.
- The privilege model is a contract too: cargo never runs as root (a root-owned
  `target/` is the state every escalate-then-build path used to end in), and the
  privileged half is told `MPOS_XTASK_ARTEFACTS_READY`, the mode, the arch and the
  invoking uid/gid across the `sudo` boundary.

Which kernel modules these commands may reach, and the edge rules, are
`docs/ARCHITECTURE.md` §2/§3 — xtask is a host tool outside the kernel graph.

## What userspace sees

`xtask` makes no syscall promise of its own — it is not in the kernel graph and
serves no `docs/uapi/` row. The only thing it hands the guest is the boot command
line it bakes into `mpos.cmdline` (the `console=` list, chosen with `--mode`) and the
programs it stages for a sweep; both are inputs to the machine, not a contract
userspace reads. Everything the user sees from a *boot* is the kernel's, documented
in `docs/uapi/` and the kernel's module pages.

## Pins

| Command | What it pins |
|---|---|
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | xtask's own unit tests — the selection semantics in `config.rs` (including `the_machine_and_the_image_keep_their_own_keys`), the refusal in `arch.rs`/`main.rs` (`a_machine_this_front_end_does_not_build_is_refused_by_name`), `every_leaf_crate_is_tested`, and the sweep/disk/qemu decisions |
| `cargo xtask test` | the whole pipeline end to end, and stage 1 exercises `gates::dead_host_tests` and the per-leaf `-p` list |
| `cargo xtask depgraph` | the ratchet behind `depgraph.rs` — also run as a host test by `cargo xtask test` |
| `cargo clippy -p mpos-xtask --target x86_64-unknown-linux-gnu` | the front end itself |
| `cargo fmt --check` | house format |

## Evidence and open work

- The command reference is `xtask/README.md`; where a command's behaviour belongs to
  a phase, the module header cites it: `depgraph.rs` §11.2 Step G, `gates.rs` §11.2
  Step F, `selftest.rs` the self-test protocol, `initramfs.rs` §15.6, `disk.rs` the
  sweep medium, `sweep.rs` the refactor that removed the duplicated harness.
- `docs/notes/` §15.18 records the capability gates being wired to the syscalls
  that own them, with the gate runs beside it; the `--arch` switch's own measurement
  is the header of `arch.rs` and `docs/notes/` §15.18/§13.x's per-machine sweeps.
- `docs/ROADMAP.md`'s status board is the plan for what the front end still has to
  grow (for example the aarch64 `tests.toml` group and the remaining migration in
  §15.11); `docs/notes/` holds the numbers. A new command lands with a
  `xtask/README.md` row and, if it changes what a sweep boots, a `sweep.rs` decision —
  the same one-intent-per-commit rule as the rest of the tree.
