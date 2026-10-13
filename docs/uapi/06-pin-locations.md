# UAPI §6

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

## 6. Where the pins live

- **Host unit tests** — `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`,
  and the same command per leaf crate (`cargo xtask test` names them all).
  Pure encodings, flag verdicts, struct sizes (`src/syscall/linux/abi.rs`,
  `crates/gpu/src/blob.rs`, `crates/evdev`, `crates/link`).
- **Guest conformance programs** — `cargo xtask test`, the `std`/`rustix` programs in
  `user/tests/src/bin/` (one syscall family, or a device, the network, a subsystem
  end to end).
- `tests.toml` decides which of them the default sweep runs. A behaviour a machine can only
  show where a device or the host's network is present is pinned there or not at all — the
  rule is that a test which cannot work is deleted or made to work, never `#[ignore]`d
  (`AGENTS.md` §3).

The gate's latest measured state is quoted from its own summary line in `docs/notes/` §12.4a;
this file deliberately carries no counts of its own.
