# user

**Home:** `user/` · **Owns:** the programs that run on the machine — the shipped `std` program and the one test suite that pins the Linux ABI.

## What it owns

`user/` is where a caller of this kernel lives. It holds two things, and the line
between them is *what a program needs to be a program*:

| Path | What it is | Built for |
|---|---|---|
| `src/` (`mpos-user`) | ordinary `std` programs that ship in the system image; `src/bin/hello.rs` is the one there today | `x86_64-unknown-linux-musl`, static |
| `tests/` (`mpos-user-tests`) | the test suite: flat, auto-discovered `src/bin/t-*.rs`, each a `fn main` that takes its verdict from the crate's own `Report` | `x86_64-unknown-linux-musl`, static; runs on **both** machines |

One suite, and its shape is the end of a migration rather than a preference. The
suite used to be two: a `no_std` raw-syscall conformance crate (`user/abitest`,
built for `x86_64-unknown-none` with the kernel's own numbers `#[path]`-included
from `nr.rs`) beside this `std` one, and `docs/notes/` §15.11 moved it across a
batch at a time — mechanical per program, `#![no_std]` + `lin::raw_sys` becoming
`fn main()` + `rustix::`, `lin::check_true`/`done` becoming
`Report::check`/`finish`. The last batch deleted the `no_std` crate with the
program that finished it, so `user/tests` is the whole suite now and the gate
builds one target for it. What the old crate pinned at the *raw door* — the
instruction, the number, the register shape — this one pins through the surface a
musl program actually speaks: `user/tests/Cargo.toml`'s header states the library
rule that keeps it honest, **`rustix` first** and `libc` only where one of the
measured clauses applies (a call `rustix` does not bind at all; a wrapper that is
not the syscall the ABI promises or that discards its result; a wrapper whose
*shape* cannot express the claim). The clauses are not preferences — each names
the program that measured it (`t-faccessat`, `t-dup2`, `t-dup3`, `t-getcwd`,
`t-openat2`, `t-fcntl`, `t-filelock`, `t-fstat`, …), and the
bitflags-versus-plain-enum distinction the rule turns on is spelled out there too.

What deliberately does not live here: the test *runner* (`xtask/src/selftest.rs`
builds, stages and reads the verdicts), the kernel-side pins (those are host
`#[cfg(test)]` modules), and the fuzz targets (their own workspace, see
[`fuzz`](fuzz.md)).

## Key files

| File | What lives there |
|---|---|
| `Cargo.toml` | `mpos-user`, the shipped `std` program crate; the test crate lives beside it, not here |
| `src/bin/hello.rs` | the one shipped program |
| `tests/Cargo.toml` | the test crate and the **`rustix`-first / `libc`-clauses rule** in its header |
| `tests/src/lib.rs` | the shared verdict protocol: `Report::check`/`Report::finish`, and `host_run()` (the `MPOS_ON_HOST` gate) |
| `tests/src/bin/t-*.rs` | one flat, auto-discovered file per claim family; the name keeps its `t-` prefix because the test `/init` globs `/bin/t-*` |
| `../tests.toml` | which programs the default sweep runs, and its groups — written in the three sections every config file here uses, so `[default]` binds both machines and a machine's own `gaps` is that machine's alone |
| `../xtask/src/selftest.rs` | the runner: builds the crate, stages the selection, writes the test `/init` (`TEST_INIT`) and reads `=== RUN/PASS/FAIL/SUMMARY/END` |

## Contracts

- **The verdict protocol is one contract.** A program prints `ok:`/`FAIL:` per
  claim and a final `PASS`/`FAILED`, and exits 0 on pass, 1 on failure, 101 from
  the panic path, through `Report::check`/`Report::finish`
  (`tests/src/lib.rs`). The `=== RUN/PASS/FAIL` lines *around* a program are the
  runner's, written from the exit status — a division the `Report` header
  states. The `no_std` suite this used to share the protocol with finished
  migrating and was deleted (§15.11's last batch), so there is one
  implementation now.
- **The numbers come from one source.** The `no_std` suite `#[path]`-included the
  kernel's `src/syscall/linux/nr.rs` so kernel and test could not disagree about
  a number; with it gone, the same rule reads its numbers from `rustix`/`libc` —
  the maintained tables — rather than from a second copy in the tree, which is
  the clause `user/tests/Cargo.toml`'s header states and each measured program
  names.
- **`tests.toml` decides the default sweep, not the crate.** A test named in a group
  is left out of the plain run: `external` needs the host network or a listener,
  `slow` measures, `diagnostic` is not a check — all three bind **both** machines,
  because the host's resolver and a public address are equally outside either. A
  machine's own `gaps` is different in kind: it is that machine's list of what it
  cannot serve *yet*, invisible to the other machine's sweep, refused by name on
  its own, and the way to shrink it is to make the call work.
- **A migrated program's claims are the contract, not its fixture.** The `std` tests
  own their fixtures (`/tmp` files, their own `PROT_NONE` pages, their own pty pairs)
  precisely because the no-std versions depended on what a particular root happened
  to carry; `docs/notes/` §15.11 records each time a fixture had to move and why.

## What userspace sees

These modules *are* userspace; they own no kernel promise, they **pin** the ones
`docs/uapi/` records. Every row in that inventory names the program that would
fail if the promise changed — a syscall's shape and errno, an ioctl family, a
struct layout, a flag vocabulary — and this suite is how those pins reach the
machine rather than a comment. The `t-` naming, the `/bin/t-*` glob and the verdict
lines are what make a claim traceable from the UAPI row, through the log, to one
`ok:` line.

## Pins

| Command | What it pins |
|---|---|
| `cargo xtask test` | builds the suite, stages the default sweep and boots it on x86_64; the reader requires a `=== PASS` from every staged program |
| `cargo xtask test --arch aarch64` | the same sweep and the same selection rule on the second machine; the only group neither arm may ask for is a machine's own `gaps` |
| `cargo xtask test --groups external,slow` / `--all-groups` | the left-out programs, on purpose — on either machine; an undefined group name is an error |
| `MPOS_ON_HOST=1 t-<name>` (run by hand) | a program's claims against the host's real Linux; the divergences this kernel keeps by design are accepted *only* under this flag and are named in each program's comment |
| `cargo fmt --check` | house format for the test sources |

## Evidence and open work

- `docs/notes/` §15.11 is the migration ledger: every batch, the fixture that moved,
  the rule clause it measured, the kernel bugs it found, and the gate runs. The
  migration is finished — the `no_std` crate was deleted with its last program —
  and the page is what to read before adding or moving a `t-` program.
- `docs/ROADMAP.md`'s status board and its Phase-15 items are the plan; what is
  left of the second machine's gaps is `tests.toml`'s own `gaps` list, one name
  long (`t-arch-prctl`, an architecture rather than a missing handler).
- `AGENTS.md` §4 ("Add a user test program") is the recipe: `std`/`rustix` in
  `user/tests/src/bin/t-<name>.rs`, a `tests.toml` group decision, and the `t-`
  name. A new parser elsewhere in the tree gets a fuzz target in the same commit
  (`AGENTS.md` §3), which is where [`fuzz`](fuzz.md) picks up.
