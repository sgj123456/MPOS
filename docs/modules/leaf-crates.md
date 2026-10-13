# leaf-crates

**Home:** `crates/` · **Owns:** the downward-only, host-testable leaves — the crates that may be named by anything above them and may name nothing back.

## What it owns

`crates/` is not "small stuff" and it is not a sorting bin. The directory means one
thing: a **host-testable leaf**. A crate lands here when its body is a decision that
can be argued with on the host — arithmetic over bytes, a wire format, a flag
vocabulary, a policy table — and when the code around that decision is machine-only.
The rule is stated in `AGENTS.md` §3: arch-independent logic lives in the library and
is host-tested, and a module is host-compiled exactly when its logic touches no
hardware, no port I/O, no virtio ring and no real clock, *and its dependencies are
host-compiled too, transitively*. The transitive half is why the leaves are
separate crates rather than modules: a crate boundary is the only thing on this
tree that turns "downward only" from a convention into a compile error.

The direction is the second half of what this directory owns. A leaf may not name
the kernel — not `vfs`, not `syscall`, not `sched`. That is what made moves like
`err` and `abi` worth doing: before `mpos-err` existed, `vfs/epoll.rs`,
`vfs/timerfd.rs`, `net/*`, `drivers/virtio/sound.rs` and `uevent.rs` all reached
*up* into `syscall` for a minus sign. Once the constants live below, a leaf cannot
acquire that edge by accident, and `cargo xtask depgraph` (AGENTS.md §2, `xtask/src/depgraph.rs`)
re-measures every `crate::` reference and fails on an unrecorded one. The reasoning
behind each extraction, including the ones that had to be postponed and why, is the
Phase-11 leaf-crate work in `docs/ROADMAP.md` §11.E and its evidence in `docs/notes/`
§11.2 Step E.

What deliberately does **not** live here:

| Not a leaf | Why |
|---|---|
| `arch/` (`mpos-arch`) | a crate so the machine layer cannot name the kernel, but it cannot be compiled for the host, and `crates/` means host-testable |
| `boot/` (`mpos-boot`) | the UEFI loader has its own target (`-unknown-uefi`) and no kernel dependency; the handoff record it reads is the leaf, the loader is not |
| the kernel crate (`src/`) | it owns the wiring, the devices and the state; a decision that is pure belongs in a leaf even when its only caller is bare metal |

The kernel re-exports each leaf under its old module name (`pub use mpos_err as err;`
and the same shape for the others), so the extraction moved no call site's spelling.

## Key files

| Crate (`crates/`) | Package | What it owns | Depends on |
|---|---|---|---|
| `err/` | `mpos-err` | the errno values, already negated for `rax` — ABI, not policy | — |
| `abi/` | `mpos-abi` | the arch-neutral ABI facts `sched` reads off a syscall path: `CLONE_*` bits, `PTRACE_*` protocol, default umask, the `RLIMIT_*` model | `err` |
| `bootinfo/` | `mpos-bootinfo` | the `Handoff` wire record (and `HANDOFF_MAGIC`/`BOOT_STACK_CANARY`) plus the kernel's `BootInfo` view of it — the one vocabulary both sides of the loader seam speak | — |
| `vm/` | `mpos-vm` | what a mapping *is*: `Mapping`, `MapFlags`, `PageTaken`, `KernelSections`, `PAGE_SIZE` and `kernel_mappings` — the plan, not the page tables | `bootinfo`, `bitflags` |
| `pci/` | `mpos-pci` | the PCI device model and the way Linux spells a device in `/sys`/`/proc` (`bar_range`, `rom_bar`); the port I/O that *finds* devices stays in the kernel | `critical-section` |
| `date/` | `mpos-date` | civil-calendar arithmetic: `days_from_civil`/`civil_from_epoch`, with the `YEAR_MIN`/`YEAR_MAX` clamp | — |
| `binfmt/` | `mpos-binfmt` | the one piece of interpretation the kernel does for a program: the `#!` first-line rule (`shebang`) | — |
| `cred/` | `mpos-cred` | the process credential model: capability sets, the dumpable tri-state and `KEEPCAPS`, and the rules over them | — |
| `uts/` | `mpos-uts` | the UTS namespace as the object a task holds — `nodename`/`domainname`, shared by `Arc` until `unshare` copies | `critical-section` |
| `seccomp/` | `mpos-seccomp` | `SECCOMP_MODE_FILTER`'s classic-BPF program: the interpreter, its validation and the action ordering | — |
| `blk/` | `mpos-blk` | the block-layer seam: the `BlockDev` trait, the disk registry, and the GPT/MBR decode in `part.rs` | `critical-section` |
| `term/` | `mpos-term` | the terminal emulator: the cell `Grid`, cursor, blitter, keymap, font and `vtmode` — a cell grid is not a filesystem fact | `critical-section` |
| `ahci/` | `mpos-ahci` | what an AHCI port's status words *mean*, as arithmetic on three `u32`s rather than bus cycles | — |
| `gpu/` | `mpos-gpu` | the GPU's pure half: the virtio-gpu wire ABI, the two DRM blobs (`blob.rs`), the cursor blend, the sync-object state machine and `sync::Waker` | `err`, `bitflags`, `virtio-drivers`, `critical-section` |
| `evdev/` | `mpos-evdev` | the evdev record window and its clients — who reads which record, and what a grab hides; the waiter is an opaque `WaiterId` | — |
| `uinput/` | `mpos-uinput` | `/dev/uinput`'s ABI: the request vocabulary (`nr_of`), the bitmap rules, the `struct uinput_setup` layout | `evdev` |
| `acpi/` | `mpos-acpi` | ACPI tables: the root pointer (both checksums), an SDT header validated against the bytes that arrived, the RSDT/XSDT walk, and MCFG's memory-mapped configuration windows — the window-relative offset (`EcamRange::offset`) and the bus-relative half a kernel needs when it maps the window a bus at a time (`function_offset`), from one definition of the layout. `fuzz/acpi_tables`. Read by `drivers::pci`, first to find the window and then to read the bus through it | — |
| `fdt/` | `mpos-fdt` | flattened device trees: a blob in, names and numbers out — a walk over the caller's bytes | — |
| `link/` | `mpos-link` | the link layer's wire format: `AF_PACKET`'s `struct sockaddr_ll` decode/encode at `linux/if_packet.h`'s offsets, the Ethernet header (`parse_ethernet`/`encode_ethernet`) and the packet socket's bounded frame ring. Extracted from `src/net/packet.rs`, which is welded to the NIC seam and therefore `target_os = "none"` — a parser behind that gate can be neither host-tested nor fuzzed (`docs/notes/` §4.5); `fuzz/link_wire` | — |

The intra-directory edges are the whole set the leaves are allowed: `abi → err`,
`gpu → err`, `uinput → evdev`, `vm → bootinfo`, all downward and all cross-crate.
Every other dependency in the table is an external crate or a `critical-section`
implementation gated exactly as the kernel gates it (the stock `std` one on the
host, the bare-metal one for `target_os = "none"`).

## Contracts

Each leaf is entered through the symbols its dependents name:

- `mpos-blk`'s `BlockDev` is all a filesystem gets to know about its storage, and
  the crate's narrow contract is deliberate: calls are synchronous, sector
  boundaries are the caller's job, and a device error is a panic rather than a
  return value on this kernel.
- `mpos-bootinfo` splits its two halves on purpose. `Handoff` is a wire format —
  `#[repr(C)]`, crossing from the loader's compilation into the kernel's — while
  `BootInfo` is the kernel's own view and is not one. A change to a field changes
  the ABI the loader and the kernel read each other's records with, which is why
  the record is one definition both sides depend on.
- `mpos-vm` is the vocabulary `hal::Paging` and `hal::AddressSpaceOps` are written
  in: the kernel owns what a mapping *is*, the machine owns how one is made, so
  the vocabulary sits below both.
- `mpos-gpu`'s `sync::Waker` is the seam that let the crate leave the strongly
  connected component: the kernel injects its implementation in `drivers::drm`, a
  host test injects one that records.
- `mpos-evdev`'s `WaiterId` is opaque for the same reason — who to wake is the
  driver's business, and `drivers/virtio/input.rs` converts at the four call sites
  it owns.
- `mpos-link` promises one endian fact that is easy to get wrong and is stated
  once rather than at each call site: a frame's Ethernet type is compared as *the
  two bytes on the wire*, so `IPV4_ETHERTYPE` is the number whose little-endian
  bytes are `08 00` and
  `u16::from_le_bytes([frame[12], frame[13]]) == IPV4_ETHERTYPE` holds with no
  host-order detour. A second spelling of that constant written with the other
  detour is what once made the raw-IP door's seam return on every frame
  (`docs/notes/` §4.5).

Who may call each of these, and the rule that a needed edge changes the design
record in the same commit, is `docs/ARCHITECTURE.md` §2/§3 — read it there rather
than here.

## What userspace sees

The leaves do not serve syscalls, but several of them own the shape a
userspace-visible row is written in. `docs/uapi/` names them by crate:

| Rows | Leaf | What it fixes |
|---|---|---|
| `'E'` (0x45), `/dev/input/eventN` | `crates/evdev` | the evdev request family and its answers |
| `'U'` (0x55), `/dev/uinput` | vocabulary in `crates/uinput`, device in `drivers/uinput.rs` | the `UI_*` request words and the setup/bitmap layouts |
| `'d'` (0x64), `/dev/dri/*` | `crates/gpu/src/virtgpu.rs` and its siblings | the `DRM_VIRTGPU_*` answers and the wire records |
| `struct sockaddr_ll`, `AF_PACKET` | `crates/link` | the address struct's layout and the Ethernet type's on-the-wire spelling |
| the `RLIMIT_*` model | `crates/abi` | the resource rows `prlimit64` speaks |
| every errno a row answers with | `mpos-err` | the "no" in Linux's spelling |

The errno table is why every refusal in `docs/uapi/` has one spelling; the
leaf's header names the upward edges it removed.

## Pins

The leaves are pinned by host tests first, and by the graph second:

| Command | What it pins |
|---|---|
| `cargo xtask test` | stage 1 runs `selftest::run_unit_tests`, which names `mpos` and every package in `xtask/src/selftest.rs`'s `LEAF_CRATES` with `-p` and runs them for the host (`--target x86_64-unknown-linux-gnu`) |
| `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu` | just the kernel's host unit tests; the leaf stage is the same command per crate |
| `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | xtask's own, including `every_leaf_crate_is_tested` |
| `cargo xtask depgraph` | the module graph against the Phase-11 ratchet — an unrecorded upward edge or a grown count fails |
| `cargo clippy --target x86_64-unknown-none` / `--target x86_64-unknown-linux-gnu` | the leaves on both sides of the `no_std` split |
| `cargo fmt --check` | house format |

`xtask/src/selftest.rs`'s `every_leaf_crate_is_tested` is the gate that keeps the
list honest: it reads every `crates/*/Cargo.toml`'s package name and asserts the set
equals `LEAF_CRATES`. The comment records why the counterfactual is not
hypothetical — a leaf added without a line there is compiled but its tests never
run, and `mpos-fdt` sat in exactly that state.

## Evidence and open work

- `docs/notes/` §11.2 Step E — the extraction of the leaves and the enforcement
  it bought; `docs/ROADMAP.md` §11.E (including the three that followed and the
  reasons `gpu`/`evdev`/`uinput` had to wait for an injectable wake).
- The crate headers carry the per-leaf evidence: `mpos-err` and `docs/ARCHITECTURE.md`
  §9 V-14; `mpos-abi` §11.2 (the `sched → syscall` row); `mpos-ahci` §7.3;
  `mpos-gpu`   §7.4; `mpos-term` §11.1.4; `mpos-evdev` §7.2/§11.1.3; `mpos-bootinfo` and
  `mpos-vm` §14 S2a/S2b; `mpos-link` §4.5.
- Open work is the edges the leaves did *not* remove by themselves: the surviving
  `drivers ↔ vfs` pair and the rest of the Phase-11 debt audit live in
  `docs/ARCHITECTURE.md` §9 and `docs/notes/` Phase 11. A new leaf arrives with
  this page updated, in the commit that adds it (the rule in `docs/modules/README.md`).
