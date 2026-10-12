# MPOS Notes and Evidence — index

The record behind the roadmap: every measurement, gate run, lesson and trap this
project has paid for, in the words it was learned in. Nothing here is a plan - the
plan is [`ROADMAP.md`](../ROADMAP.md) - and nothing here is decoration either:
a claim in the code or in the roadmap that reads arbitrary should have a number here
beside it, and section numbers (`§13.29`, `§7.4`) are what the source comments cite.

This library used to be one file, `docs/NOTES.md`. It is now split per phase; the
`§N.M` numbering is **unchanged**, so an old pointer still names its exact place.

## Routing: a `§N.M` pointer lives in file N

| Prefix | Phase / subject | File |
|---|---|---|
| §1 | Syscall completion | [01-syscall-completion.md](01-syscall-completion.md) |
| §2 | Signal delivery | [02-signal-delivery.md](02-signal-delivery.md) |
| §3 | Filesystem enhancements | [03-filesystem.md](03-filesystem.md) |
| §4 | Networking enhancements | [04-networking.md](04-networking.md) |
| §5 | Memory management | [05-memory-management.md](05-memory-management.md) |
| §6 | Process management | [06-process-management.md](06-process-management.md) |
| §7 | Device drivers (display, input, AHCI, compositor analysis) | [07-device-drivers.md](07-device-drivers.md) |
| §8 | Security hardening | [08-security-hardening.md](08-security-hardening.md) |
| §9 | Architecture expansion (aarch64 boot, RISC-V) | [09-architecture-expansion.md](09-architecture-expansion.md) |
| §10 | Performance | [10-performance.md](10-performance.md) |
| §11 | Architectural debt | [11-architectural-debt.md](11-architectural-debt.md) |
| §12 | ABI truthfulness | [12-abi-truthfulness.md](12-abi-truthfulness.md) |
| §13 | Machine-seam purity and aarch64 parity | [13-machine-seam.md](13-machine-seam.md) |
| §14 | `arch` becomes a crate | [14-arch-crate.md](14-arch-crate.md) |
| §15 | One boot flow | [15-boot-flow.md](15-boot-flow.md) |
| appendix | The evidence behind the status board | [appendix-status-board.md](appendix-status-board.md) |

A bare `§N` (no `.M`) means the whole phase — file N's top section. A `(§N)` pointer on a
`ROADMAP.md` line works the same way.

## Exceptions worth knowing before you grep

- **§13.1–§13.31**: the numbered *entries* are checklist items in
  [appendix-status-board.md](appendix-status-board.md); [13-machine-seam.md](13-machine-seam.md) holds
  the phase's plan and measured starting state (§13.x as table rows). Search both.
- **§15.10–§15.38**: these entries are table rows inside [15-boot-flow.md](15-boot-flow.md), not
  headings. §15.8/§15.9/§15.12/§15.13 sit at the *end* of that file (they were appended after the
  appendix when the notes grew, and the split moved them back).
- **§12.4a** appears twice by history (the step-1 diagnosis, and the two-reds closure); keep both.
  The gate's latest measured summary line lives in [12-abi-truthfulness.md](12-abi-truthfulness.md)'s
  §12.4a entries — `AGENTS.md`'s "reporting the gate" rule points here.
- **R.\<n\>** headings at the end of a file are evidence moved out of `ROADMAP.md`'s status board when
  it was compressed; they keep the roadmap item's subject but carry an `R.` prefix so they cannot
  collide with a real `§N.M`.
- Source comments across the tree cite `` `docs/notes/` §X.Y `` — the old single-file name. Treat every
  such citation as pointing into this library; route through the table above.
