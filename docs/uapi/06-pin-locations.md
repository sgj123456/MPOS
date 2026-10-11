# UAPI §6

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

## 6. Where the pins live

- **Host unit tests** — `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`.
  Pure encodings, flag verdicts, struct sizes (`src/syscall/linux/abi.rs`,
  `crates/gpu/src/blob.rs`, `crates/evdev`, `src/net`'s ungated halves).
- **Guest conformance programs** — `cargo xtask test`. `user/abitest/src/bin/syscall/t-*.rs`
  (one syscall family) and `user/abitest/src/bin/integration/t-*.rs` (a device, the
  network, a subsystem end to end), plus the `std`/`rustix` programs in
  `user/tests/src/bin/`.
- `tests.toml` decides which of them the default sweep runs. A behaviour a machine can only
  show where a device or the host's network is present is pinned there or not at all — the
  rule is that a test which cannot work is deleted or made to work, never `#[ignore]`d
  (`AGENTS.md` §3).

The gate's latest measured state is quoted from its own summary line in `docs/notes/` §12.4a;
this file deliberately carries no counts of its own.
