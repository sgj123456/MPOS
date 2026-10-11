# user

**Home:** `user/` · **Owns:** the programs that run on the machine — the shipped `std` program, the two test suites that pin the Linux ABI, and the scratch tree that is deliberately wired into no build.

## What it owns

`user/` is where a caller of this kernel lives. It holds four things, and the line
between them is *what a program needs to be a program*:

| Path | What it is | Built for |
|---|---|---|
| `src/` (`mpos-user`) | ordinary `std` programs that ship in the system image; `src/bin/hello.rs` is the one there today | `x86_64-unknown-linux-musl`, static |
| `tests/` (`mpos-user-tests`) | the `std`-side test suite: flat, auto-discovered `src/bin/t-*.rs`, each a `fn main` that takes its verdict from the crate's own `Report` | `x86_64-unknown-linux-musl`, static; runs on **both** machines |
| `abitest/` (`mpos-user-abi`) | the `no_std` ABI conformance suite: raw `syscall` instructions with the numbers taken from the kernel's own `nr.rs` | `x86_64-unknown-none`, static ET_EXEC; the first machine only |
| `staging/` | deliberate scratch — probes and programs under active measurement, wired into no build | — |

Two suites exist because they answer different questions. The `no_std` suite pins
a syscall's contract at the *raw door*: the instruction, the number, the register
shape, the errno, with no libc between the claim and the kernel. The `std` suite
exercises the surface a musl program actually speaks, through `rustix` and `std`,
and — because it builds for both machines — is what lets one program's per-claim
`ok:` lines be compared across the two arms. `user/tests/Cargo.toml`'s header states
the library rule that keeps the second suite honest: **`rustix` first**, and `libc`
only where one of the measured clauses applies (a call `rustix` does not bind at all;
a wrapper that is not the syscall the ABI promises or that discards its result; a
wrapper whose *shape* cannot express the claim). The clauses are not preferences —
each names the program that measured it (`t-faccessat`, `t-dup2`, `t-dup3`,
`t-getcwd`, `t-openat2`, `t-fcntl`, `t-filelock`, `t-fstat`, …), and the
bitflags-versus-plain-enum distinction the rule turns on is spelled out there too.

The suite tree is also mid-migration, and that is a fact about the current tree
rather than a plan. `docs/notes/` §15.11 moves `user/abitest` to `user/tests` a
batch at a time (mechanical per program: `#![no_std]` + `lin::raw_sys` becomes
`fn main()` + `rustix::`, `lin::check_true`/`done` becomes `Report::check`/`finish`);
the syscall family is fully migrated, so `user/abitest/src/bin/` holds only the
`integration/` half now. `xtask/src/selftest.rs`'s doc comment still describes the
older `src/bin/syscall/` split; the tree is the current state.

What deliberately does not live here: the test *runner* (`xtask/src/selftest.rs`
builds, stages and reads the verdicts), the kernel-side pins (those are host
`#[cfg(test)]` modules), and the fuzz targets (their own workspace, see
[`fuzz`](fuzz.md)).

## Key files

| File | What lives there |
|---|---|
| `Cargo.toml` | `mpos-user`, the shipped `std` program crate; the test crates live beside it, not here |
| `src/bin/hello.rs` | the one shipped program |
| `tests/Cargo.toml` | the `std` test crate and the **`rustix`-first / `libc`-clauses rule** in its header |
| `tests/src/lib.rs` | the shared verdict protocol: `Report::check`/`Report::finish`, and `host_run()` (the `MPOS_ON_HOST` gate) |
| `tests/src/bin/t-*.rs` | one flat, auto-discovered file per claim family; the name keeps its `t-` prefix because the test `/init` globs `/bin/t-*` |
| `abitest/Cargo.toml` | the `no_std` crate; each program needs an explicit `[[bin]]` block because Cargo does not auto-discover `src/bin/<subdir>/*.rs` |
| `abitest/lib/lin.rs` | the `no_std` harness: the `syscall` stubs, the ABI structs, the per-check `check_true`/`check_eq`/`done` protocol |
| `abitest/build.rs` | links each binary as a static ET_EXEC at `--image-base=0x400000` with `--gc-sections`, the only image shape the kernel's ELF loader accepts |
| `abitest/src/bin/integration/` | the end-to-end programs — a device, the network, another process image, or a subsystem run whole (including the `t-forkcost`/`t-pipecost`/`t-netlat` instruments) |
| `staging/` | deliberate scratch; look in `abitest/` and `tests/` for the live version first |
| `../tests.toml` | which programs the default sweep runs, and the `external`/`slow`/`diagnostic`/`aarch64` groups |
| `../xtask/src/selftest.rs` | the runner: builds both crates, stages them, writes the test `/init` (`TEST_INIT`) and reads `=== RUN/PASS/FAIL/SUMMARY/END` |

## Contracts

- **The verdict protocol is one contract with two implementations.** A program
  prints `ok:`/`FAIL:` per claim and a final `PASS`/`FAILED`, and exits 0 on pass,
  1 on failure, 101 from the panic path. The `std` side does it through
  `Report::check`/`Report::finish` (`tests/src/lib.rs`); the `no_std` side through
  `lin.rs`'s `check_true`/`check_eq`/`done`. The `=== RUN/PASS/FAIL` lines *around*
  a program are the runner's, written from the exit status — a division the
  `Report` header states, and the reason the two suites can move between readers.
- **The numbers come from one source.** `lin.rs` `#[path]`-includes the kernel's
  `src/syscall/linux/nr.rs`, and `nr.rs` re-exports the `clone(2)` bits from the
  `mpos-abi` leaf (which is why `abitest/Cargo.toml` depends on it): kernel and test
  cannot disagree about a number, or about a flag word, without one of them failing
  to build. `lin.rs`'s `syscall` stubs are thin on purpose — each names the Linux
  number once and nothing else.
- **`RUSTFLAGS` is load-bearing for `abitest`.** Its profile points at the kernel's
  linker script, so `cargo xtask test` builds it with
  `RUSTFLAGS=-C relocation-model=static`; a hand build without that links binaries
  the loader rejects (`AGENTS.md` §2, trap 2).
- **`tests.toml` decides the default sweep, not the crate.** A test named in a group
  is left out of the plain run: `external` needs the host network or a listener,
  `slow` measures, `diagnostic` is not a check. The `aarch64` group is different in
  kind — it is the second machine's list of what it cannot serve *yet*, invisible to
  x86_64's sweep, and the way to shrink it is to make the call work.
- **A migrated program's claims are the contract, not its fixture.** The `std` tests
  own their fixtures (`/tmp` files, their own `PROT_NONE` pages, their own pty pairs)
  precisely because the no-std versions depended on what a particular root happened
  to carry; `docs/notes/` §15.11 records each time a fixture had to move and why.

## What userspace sees

These modules *are* userspace; they own no kernel promise, they **pin** the ones
`docs/uapi/` records. Every row in that inventory names the program that would
fail if the promise changed — a syscall's shape and errno, an ioctl family, a
struct layout, a flag vocabulary — and the two suites are how those pins reach the
machine rather than a comment. The `t-` naming, the `/bin/t-*` glob and the verdict
lines are what make a claim traceable from the UAPI row, through the log, to one
`ok:` line.

## Pins

| Command | What it pins |
|---|---|
| `cargo xtask test` | builds both suites, stages the default sweep and boots it on x86_64; the reader requires a `=== PASS` from every staged program |
| `cargo xtask test --arch aarch64` | the same sweep on the second machine, staging only `user/tests`; refuses `--groups`/`--all-groups` (the groups name x86_64's programs) |
| `cargo xtask test --groups external,slow` / `--all-groups` | the left-out programs, on purpose; an undefined group name is an error |
| `MPOS_ON_HOST=1 t-<name>` (run by hand) | a migrated program's claims against the host's real Linux; the divergences this kernel keeps by design are accepted *only* under this flag and are named in each program's comment |
| `cargo fmt --check` | house format for the test sources |

The `no_std` suite is pinned by the gate too: `cargo xtask test` builds
`mpos-user-abi` with the `RUSTFLAGS` above from inside `user/abitest/`, and a program
without its `[[bin]]` block is caught as "no binary" for a source the walk found.

## Evidence and open work

- `docs/notes/` §15.11 is the migration ledger: every batch, the fixture that moved,
  the rule clause it measured, the kernel bugs it found, and the gate runs. It is the
  page to read before adding or moving a `t-` program.
- `docs/ROADMAP.md`'s status board and its Phase-15 items are the plan; the remaining
  `user/abitest` integration programs are what is still to migrate, and the `aarch64`
  group in `tests.toml` is the second machine's open list.
- `AGENTS.md` §4 ("Add a user test program") is the recipe: `std`/`rustix` in
  `user/tests/src/bin/t-<name>.rs`, `[[bin]]` for the `no_std` half, a `tests.toml`
  group decision, and the `t-` name. A new parser elsewhere in the tree gets a fuzz
  target in the same commit (`AGENTS.md` §3), which is where [`fuzz`](fuzz.md) picks up.
