# fuzz

**Home:** `fuzz/` · **Owns:** the libFuzzer targets over the kernel's parsers — the places where untrusted bytes become something the kernel then trusts.

## What it owns

`fuzz/` is a **separate cargo workspace** from the kernel (an empty `[workspace]`
table in `fuzz/Cargo.toml`; the root workspace does not list it). That is not
tidiness: without its own workspace root, `mpos-fuzz` would believe it belongs to a
workspace that never heard of it, and the separation is also what lets the fuzz build
pick `--target x86_64-unknown-linux-gnu` over the kernel's own `[build] target`
(`x86_64-unknown-none`, which cannot link a libFuzzer binary). The entry point is
therefore `cargo fuzz` run **from inside `fuzz/`**, not from the repository root
(`AGENTS.md` §5).

What the directory owns is the negative direction of the project's design rule: any
place the kernel reads user- or wire-supplied bytes joins the targets here, or
`docs/notes/` says why not (`AGENTS.md` §3, extended rule 2). Its scope is exactly
the modules the kernel's `src/lib.rs` leaves compiled for a host build — the ELF
loader, the ustar initramfs, ext2/ext4's on-disk structures, partition tables, paths,
PCI sizing, the console's escape parser, the 9P `Rstat` record, and the
`uinput`/`evdev` vocabulary that turns a user request into a device's self-description.
Machine-dependent code (`arch`, `drivers`, `console`, `net`) is out of scope here;
its pins are the conformance programs in `user/abitest` and `user/tests` that run on
the machine.

Two properties make this suite more than "it didn't crash". First, the build runs
with ASan, debug assertions and overflow checks — stricter than the kernel's own
release profile, which is the point: the arithmetic in `date` and `term` *wraps
silently* in the kernel and panics here. Second, each target asserts a property its
module already promised (the `assert!`s in `fuzz_targets/*.rs`), because a crash is
the least informative outcome. The seed corpora in `seeds/<target>/` are real
artefacts (`mke2fs` images, a static musl ELF, the head of a built initramfs), copied
into `corpus/` before the first run; `ext4_write` reuses `ext4_image`'s images.

Deliberately not here: a `cargo xtask` command, and any gate hook. This suite is run
by hand when a parser changes (or when the plan says to); it is evidence, not a
check the self-test sweep depends on.

## Key files

| File | What lives there |
|---|---|
| `Cargo.toml` | the workspace root, `cargo-fuzz` metadata, the `libfuzzer-sys`/`arbitrary` dependencies, the `mpos` path dependency (host build) and one `[[bin]]` per target |
| `README.md` | the bilingual reference: how to install and run, the target table with each module's asserted property, the seed corpora, and the LeakSanitizer exception |
| `fuzz_targets/support.rs` | the shared harness: `MemDev`, a `Vec<u8>` seen through the kernel's `BlockDev` trait (reads past the end answer zeros), and `MAX_DISK` — the cap that keeps a fuzzed *length* (the host running out of RAM) apart from a fuzzed *value* (a kernel bug). The screen-side clamp is `term::MAX_ROWS`, applied in `term_grid.rs` |
| `fuzz_targets/*.rs` | one `fuzz_target!` per entry point (table below) |
| `seeds/` | the committed seed corpora; `corpus/`, `artifacts/` and `target/` are ignored |

| Target | Entry point it drives |
|---|---|
| `elf_parse` | `mpos::mm::elf::parse` |
| `tar_unpack` | `mpos::vfs::tar::unpack_into` over a `ramfs` |
| `ext4_image` | `mpos::vfs::ext4::Ext4Fs::mount` and reads |
| `ext4_write` | the same mount, plus create/write/unlink and `statfs` |
| `partition_table` | `mpos::blk::part` |
| `binfmt_shebang` | `mpos::binfmt::shebang` |
| `vfs_path` | `mpos::vfs::path::{normalize, components}` |
| `pci_bars` | `mpos::pci::{bar_range, rom_bar}` and the attribute spelling |
| `uevent_encode` | `mpos::uevent::Event::encode_seq` |
| `inotify_record` | `mpos::vfs::inotify`'s record layout |
| `logging_cmdline` | `mpos::logging::level_from_cmdline` |
| `term_grid` | `mpos::vfs::term::Grid::feed` |
| `keymap_events` | `mpos::vfs::term::keymap::Keymap::event` |
| `signal_sets` | `mpos::sched::signal` |
| `sched_policy` | `mpos::sched::policy::Scheduler` |
| `date_civil` | `mpos::date::{days_from_civil, civil_from_epoch}` |
| `uinput_setup` | `mpos::uinput::{parse_setup, record_count, bit_request, Device::set_bit}` |
| `evdev_window` | `mpos::evdev::Records` |
| `virtgpu_capset` | `mpos::gpu::abi::resp_capset_payload` |
| `p9_stat` | `mpos::vfs::p9::parse_stat` |

`p9_stat` is newer than `README.md`'s target table — the target and its corpus exist,
the table has not caught up. `support.rs` is not a target. Some inputs are structured
(`arbitrary`-derived `Input` structs) rather than byte strings, because the thing
worth fuzzing there is the field values and not their encoding.

## Contracts

- Every target is a `fuzz_target!(|input| { … })` function; the build is
  `cargo fuzz build`, a single run is `cargo fuzz run <target>` (optionally
  `-- -max_total_time=…`), and a minimised input is `cargo fuzz tmin <target>
  artifacts/<target>/crash-<hash>`. These are `README.md`'s.
- The shared `support::MemDev` is the contract between a byte string and a
  disk-reading parser: it implements the kernel's `BlockDev` trait, and reads past
  the end of the device answer zeros (what a short read on a real disk does), so a
  truncated image behaves like a truncated image rather than like an out-of-bounds
  indexing panic in the harness.
- The clamps are part of the contract too, and they exist to keep a finding
  meaningful: `MAX_DISK` caps how much *real* memory backs a fuzzed disk (the
  declared size is still whatever the bytes say and is the kernel's problem to
  validate), and `term_grid` clamps a constructed screen to `term::MAX_ROWS`, the
  same cap the kernel applies.
- `ext4_write` and sometimes `ext4_image` trip LeakSanitizer by design — a mounted
  `Core` and each inode hold `Arc`s to one another, and a kernel that never unmounts
  never frees them — so that run turns the check off with
  `ASAN_OPTIONS=detect_leaks=0 cargo fuzz run ext4_write`. The reasoning is in
  `README.md`.

## What userspace sees

The fuzz targets make no userspace-visible promise of their own: `fuzz/` is not in
the kernel graph and owns no row in `docs/uapi/`. They fuzz the *parsers* that rows
elsewhere in the tree depend on; which syscall or ioctl reaches each parser is the
owning module's page, and the row's pin is the `t-*` program named in `docs/uapi/`.
So this section is empty by design, not by omission.

## Pins

There is **no gate** that runs the fuzz targets: `cargo xtask test` neither builds
nor invokes them, and no CI hook here does. The real pin is the rule in `AGENTS.md`
§3 (extended rule 2) — a new parser lands with a target, or `docs/notes/` says why
not — and the manual commands:

| Command (run from `fuzz/`) | What it does |
|---|---|
| `cargo fuzz build` | compiles every target |
| `cargo fuzz run <target>` | runs one target (until it crashes or is stopped) |
| `cargo fuzz run ext4_image -- -max_total_time=300` | a bounded run |
| `cargo fuzz tmin <target> artifacts/<target>/crash-<hash>` | minimises a crash |

`cargo-fuzz` and the nightly toolchain are prerequisites (`cargo install cargo-fuzz`;
the toolchain is pinned by `rust-toolchain.toml`), and ASan is what makes a run
find the wrap the kernel would not.

## Evidence and open work

- `docs/notes/` §13.29 records why `p9_stat` exists at all: a `statu` record's own
  length word decides where it ends over bytes a *server* chose, and a slice that
  panics instead of answering `None` takes the machine down when a share answers
  something odd — neither reachable through well-behaved message framing, which is
  exactly why the parser is fuzzed directly.
- `AGENTS.md` §3, extended rule 2, is the standing requirement; `docs/ROADMAP.md`'s
  status board carries any parser-related work, and `fuzz/README.md` is the run book.
- Open work is whatever a new parser lands without a target; the tell is a parser
  reachable from user or wire bytes whose target is not in the table above. A new
  target arrives with its `[[bin]]` block, its seed if it has a real artefact, and a
  `README.md` row (the entry `p9_stat` still owes).
