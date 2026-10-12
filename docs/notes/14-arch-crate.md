# §14 Phase 14: `arch` becomes a crate — the machine seam, enforced by the compiler

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 14: `arch` becomes a crate — the machine seam, enforced by the compiler

**Goal** (one sentence): `--arch x86_64|aarch64` switches the machine; `arch` and the HAL are one
crate `mpos-arch` that depends on nothing in the kernel; every other module is written once.

**Why a crate and not the ratchet we already have.** `cargo xtask depgraph` (§11.4) already fails
on a module outside `arch`/`hal`/`main`/`lib` naming `crate::arch::`, and `src/hal` already carries
zero `target_arch` gates (§13.1). Both are *measured by a tool this repository wrote*. A crate
boundary is the same rule enforced by rustc, and it is the only version of it a port cannot
accidentally edit around. Everything below is the work of getting to that boundary; none of it
changes what the HAL abstracts.

### 14.0 What was already true, and the one gap that was not

| Claim | State |
|---|---|
| `--arch` selects the machine | **Was not true.** The switch reached `run`, `test` and `boot-aarch64` only; `cargo xtask build --arch aarch64` compiled the x86_64 kernel and reported success, because `utils::build_kernel` never passed `--target` and the loader's target came from `boot/.cargo/config.toml`'s `[build] target`. `boot-aarch64` carried its own copy of both triples for exactly that reason. **Fixed** (S0 below). |
| `hal` is architecture-neutral | True since §13.1: capability traits, `mpos_*` cfg flags, no `dyn` on the downward path. |
| A new port writes `arch/<arch>/` and edits no shared file | True in practice for both ports. |

### 14.1 The whole obstacle: 128 upward references out of `src/arch/**`

Measured (`crate::` in `src/arch/**`, excluding `crate::hal`/`crate::arch`, comments excluded), after
S1a. A crate boundary turns every one of these into a compile error, so each is either **moved down**
(a leaf crate both sides depend on), **moved out** (the caller belongs above the machine layer), or
**inverted** (the machine declares the service and the kernel registers it).

| Kind | Sites | Disposition |
|---|---|---|
| `sched` (fault resolvers, `exit`, `preempt`, `current`, signals) | ~30 | **invert** — hook table |
| `println!` / logging | 23 | **invert** — hook table (a log line), or move the checking code out |
| `mm` (`KernelSections`, `MapFlags`, `Mapping`, `PageTaken`, `PAGE_SIZE`, frame allocation, `map_mmio`) | ~25 | **move down** the four vocabulary types; **invert** frame allocation and the dynamic MMIO window |
| `drivers` (`serial` 9, `tx_ring` 8, `virtio` 2, `qemu` 4) | 23 | **invert** — the console sink and the transmit ring are kernel services; `qemu`'s debug exit is a hook |
| `boot` (`BootInfo` 5, `handoff` 2, `set_cmdline` 1) | 8 | **move down** the record (and it is the same record `boot/src/handoff.rs` duplicates — S3 closes that too); `set_cmdline` inverted |
| `err`, `date`, `fdt` | 5 | **already leaf crates** — `mpos-arch` depends on them directly, no work |
| `syscall::linux_dispatch`, `vfs::vcmux`, `net::irq_service`, `console::force_serial` | 5 | **invert** — hook table |

### 14.2 The inversion: a static hook table, upward only

`mpos-arch` declares the kernel services it needs as one trait (or an equivalent struct of
`fn` pointers) and the kernel registers its implementation once at boot. The *downward* direction —
generic code asking the machine — stays exactly as it is today: capability traits, one `impl`, no
`dyn`, monomorphised. That asymmetry is deliberate and is the reason this is a table and not a
generic parameter:

- every upward call is on a slow path (a fault, a tick, a log line, a page-table frame), so an
  indirect call costs nothing measurable;
- `Machine<K: Kernel>` would have to thread `K` through every arch type, and the exception entry
  points reached from `global_asm!` **cannot be generic** — each would need a `#[no_mangle]` shim
  the kernel defines and the arch crate's assembly branches to, i.e. a symbol the machine layer
  references but does not own.

### 14.3 Stages

- [x] **S0 — `--arch` is one switch.** `xtask/src/arch.rs`: `Arch` carries the target triples, and
      `Arch::parse` refuses a machine this front end does not build. `build_kernel(profile, arch)`
      passes both `--target`s; `boot-aarch64`'s private kernel/loader build (and its two target
      constants) are gone. The `--arch` word now also crosses the sudo boundary, since the
      privileged half re-parses it. `initramfs`/`disk`/`rootfs`/`all` refuse `--arch aarch64` by
      name — they assemble *the x86_64 disk*, and the second machine's disk is laid out by
      `boot-aarch64` / `alpine-aarch64` / `run --arch aarch64`; building an x86_64 root under an
      aarch64 banner is the silent-wrong-machine failure `AGENTS.md` §3 refuses.
      Measured: `cargo xtask build` -> `target/x86_64-unknown-none`, `--arch aarch64` ->
      `target/aarch64-unknown-none` + `aarch64-unknown-uefi`; `cargo xtask boot-aarch64` still
      green through the shared builder (full marker chain, `INIT_EXITED`).
- [x] **S1a — aarch64 device discovery moves to `main`.** The virtio-MMIO identity constants,
      `read_device`, `virtio_device_windows` and the six device probes (`check_virtio`, `check_rng`,
      `check_blk`, `check_fs`, `check_net`, `check_input`) — 566 lines — moved from
      `src/arch/aarch64/mod.rs` to `src/board/aarch64.rs`, reached through one more pointer `boot`
      is handed (`devices: fn(&BootInfo)`), exactly as `milestone: fn()` already is. Opening a
      window as a virtio device, mounting a filesystem over it and registering it with the block
      layer is driver/filesystem/block work; a machine crate cannot name any of them. The one
      machine question in the moved code — *where is this physical window* — became
      **`hal::map_mmio_window`**, which on this machine is `paging::device_virt` and on the other is
      a fresh kernel-half mapping: the probes are now architecture-neutral code.
      Measured: upward refs 140 -> 128, `boot-aarch64` green with the identical marker chain.
- [ ] **S1b — the remaining `check_*` milestone probes.** `check_context`, `check_paging`,
      `check_timer`, `check_wallclock`, `check_cpu`, `check_serial_rx`, `check_tls`,
      `check_user_memory`, `check_paging_ops` (~1900 lines) still call `sched`/`mm`/`println!` from
      inside `arch`. **They can stay**: they are "does this machine's HAL work" checks and after S3
      they reach the scheduler through the hook table like everything else in `arch`. Moving them
      is optional tidiness, not a prerequisite — `check_el0` and `el0_returned` in particular own
      the EL0 entry and the handoff to `init`, and extracting those from the machine layer is a
      design job of its own. The reason S1a *was* a prerequisite is the opposite one: those probes
      were not testing the machine, they were driving devices.
- [x] **S2 — move the shared vocabulary down.** Two new leaves, both pure data and
      host-tested, both re-exported by the kernel under the names they already had so
      no caller changes:
      - `crates/mpos-bootinfo`: `BootInfo`, `MemRegion`, `MemKind`, `MAX_MEM_REGIONS`, and the
        wire record `Handoff`/`AbiRegion`/`KIND_*`/`HANDOFF_MAGIC`. `boot::handoff::collect`
        — the translation, which is the part that decides what a region *means* — stays in
        the kernel. `src/boot/mod.rs` and `src/boot/handoff.rs` re-export.
      - `crates/mpos-vm`: `Mapping`, `MapFlags`, `PageTaken`, `KernelSections`, the page
        arithmetic (`PAGE_SIZE`/`PAGE_SHIFT`/`align_up`/`align_down`) and the
        machine-independent `kernel_mappings` plan. `src/mm/vm.rs` and `src/mm/pframe.rs`
        re-export; the five tests that pin the plan moved with it.

      `err`/`date`/`fdt` were already leaves, so `mpos-arch` depends on them directly with
      no work. The two-places-to-sync problem with `boot/src/handoff.rs` is *not* closed
      here — the loader still carries its own copy of the record, and making it depend on
      `mpos-bootinfo` is a separate, self-contained cut (§14.3 S2-follow).

      **A gate hole this surfaced**: `run_unit_tests` named every leaf crate by hand, and
      `mpos-fdt` had never been added — eight tests, never run by any gate. The list is a
      const now and `selftest::tests::every_leaf_crate_is_tested` pins it against
      `crates/`, so a leaf added without a line there fails the xtask tests instead of
      quietly shrinking the sweep. Measured: the host count went 515 -> 510 + 5 (`mpos-vm`)
      + 8 (`mpos-fdt`), i.e. the same tests, now all inside the gate.
- [x] **S3 — build the hook table.** `src/hal/kernel.rs` declares [`Kernel`] — the
      services a machine's trap entry needs: `started`, `current`, the three fault
      resolvers, `kernel_lazy_accessible`, `send_signal`/`disposition_is_default`,
      `exit`, `preempt`, `charge_cpu`, `device_not_available`, `dispatch_syscall` —
      with one forward per method, so a handler reads `hal::kernel::resolve_cow_fault(addr)`
      and never names `sched`. `main/<arch>.rs` implements it and calls
      `hal::kernel::register` before the machine's vectors/IDT go in.

      **Slice 1 landed**: every `crate::sched::*` and `crate::syscall::*` site in
      `src/arch/**` (37 of them) now goes through it, on both machines. Upward refs
      **128 -> 89**. Measured: `boot-aarch64` green with the identical marker chain,
      `test --arch aarch64` 25/25, and the x86_64 sweep unchanged at 154/155 — the
      `#PF`, timer and syscall paths it converted are the ones that sweep leans on
      hardest, so that number is the evidence the indirection costs nothing.

      `device_not_available` is gated on `mpos_fpu_trap` inside the trait, the way
      every other optional capability is: aarch64 has no `#NM` to fault on, so the
      method does not exist for it rather than being a stub it must fake.

      **Slice 2 landed — the machine layer now names no kernel module at all.**
      Measured: `crate::` references in `src/arch/**` *and* `src/hal/**` that are
      neither `crate::hal` nor `crate::arch` are **89 -> 0**. What that took, by
      kind, and what each answer actually was:

      - **48 sites that were never hooks.** `mm::phys_to_virt` and `mm::hhdm_offset`
        are the loader's *direct map*, not a kernel service: the loader chose the
        offset and the loader mapped physical memory at it. They are now
        `hal::phys_to_virt` / `hal::direct_map_offset` / `hal::virt_to_phys`, backed
        by one `hal` static that `mm::init` fills from the boot record. This is a
        *downward* call — no `dyn`, no indirect branch — so the forty-odd page-table
        walks pay nothing for the move. `mm`'s three helpers stayed as forwards.
      - **The hook table's second half.** `print_line` (+ a `kprintln!` macro),
        `force_serial`, `route_console_input`, `service_network`, `set_cmdline`,
        `boot_info`, `start_memory`, `alloc_frames`/`free_frames`/`frames_free`, and
        `wake`. The batch's one trap: the macro's newline delimiter cannot be a
        `concat!`, because an implicit named argument (`"{rsp:#x}"`, which dozens of
        the fault reports use) is only captured from a *string literal* and
        `concat!`'s expansion is a macro call. `format_args!("{}\n", format_args!(…))`
        is the nested form that works.
      - **Three relocations, because the call was not an inversion.** `tx_ring` (the
        console's transmit queue) is machine-*neutral* — parameterised by a
        `Transmitter` and knowing no register — so it moved to
        `src/hal/tx_ring.rs`, beside the `SerialRing` trait it implements. `serial`
        (the 16550) and `qemu` (the `isa-debug-exit` port) are one machine's own
        hardware and moved to `src/arch/x86_64/`. `drivers/` no longer declares any
        of the three.
      - **`qemu` became a capability, not a path.** `hal::TestExit` +
        `mpos_qemu_debug_exit` + `hal::exit_qemu(code)`, with `TEST_EXIT_OK` /
        `TEST_EXIT_PANIC` beside it because those codes are the *harness's*
        contract. A machine without the flag halts instead — which is what the
        second machine's `reboot(2)` stop path now gets, honestly, rather than a
        second body of code.
      - **The per-BAR MMIO window moved out of `mm`.** `mm::map_mmio` (the x86
        bump allocator behind `hal::map_mmio_window`) is `arch/x86_64/mmio.rs`; the
        `mpos_dynamic_mmio_window` flag is **deleted**, because it described a
        *mechanism* and a mechanism is exactly what a capability flag must not name.
        `ahci`, `virtio::msix` and the local APIC now call `hal::map_mmio_window`.
      - **`virtio-rng` left the machine layer.** Both machines' `Entropy::mix_into`
        folded in the rng device; the device is a driver the kernel enumerates, not
        an answer the machine gives, so `virtio::entropy()` names the transport and
        `crate::rand` folds it in. aarch64's `mix_into` is now *empty* and says so,
        which is the honest answer for a core with no `RDRAND`.
      - **One name for both machines' console arming.** `board/x86_64.rs` no longer
        names `drivers::serial`: `arch::enable_console_interrupts` is now the
        x86_64 twin of the aarch64 re-export of the same name.

      Measured: both kernels build warning-free (for what this change touched),
      `boot-aarch64` green with the identical marker chain, module graph within the
      ratchet (no edge appeared, no count grew, 9/9 mutual pairs).

      [`Kernel`]: ../src/hal/kernel.rs
- [x] **S4 — the move itself.** `arch/` is a workspace-root crate (`mpos-arch`, **not** under
      `crates/` - that directory means "host-testable leaf" and `xtask`'s
      `every_leaf_crate_is_tested` pins it against the host sweep, which a crate of `cli`/`sti`,
      MSRs and descriptor tables can never join). `src/hal/` and `src/arch/` moved to
      `arch/src/{hal,arch}/`, so the module paths *inside* are exactly what they were
      (`crate::hal::…`, `crate::arch::x86_64::…`) and the whole body moved unchanged.
      `src/arch` and `src/hal` are gone.

      **The kernel kept its names.** `src/lib.rs` has `pub use mpos_arch::{arch, hal};` - the same
      treatment the leaf crates got in S2. That is what makes the thousands of words of commentary
      above every `crate::hal::` call site still true rather than merely nearby, and it is why this
      step is a move and not a 139-site rename. What the crate boundary bought is not a different
      spelling in `mpos`; it is that `mpos-arch` has no `crate::sched`, `crate::mm` or
      `crate::println!` *in scope*, so the machine layer's independence is cargo's business now.

      Three things followed the code, and one did not:

      - `.cargo/config.toml`'s two `-C link-arg=--script=` paths point at
        `arch/src/arch/<arch>/linker.ld`. (They are passed to every crate built for the target and
        ignored by the ones with no link step; only the kernel binary links.)
      - `feature(abi_x86_interrupt)` went to `arch/src/lib.rs` with the `extern "x86-interrupt"`
        handlers it exists for, and the kernel's crate root dropped it - "declared but not used" was
        the warning that said the last machine body had left.
      - The `mpos_*` capability vocabulary became `[workspace.lints.rust]` with `[lints] workspace =
        true` in every member, because a cargo lint table is per-package and the second copy - the
        one `mpos-arch` needs - is the copy whose staleness would silently stop a warning.
        (`missing_lints_inheritance` then fires once per manifest that does not opt in, which is why
        every member opts in; two members had an empty `[lints.cargo]` placeholder that cargo
        rejects alongside `workspace = true` and that did nothing, so they are gone.)
      - The capability *flags* stayed global: `.cargo/config.toml`'s `[target.…]` blocks are
        per-target, not per-package, and the kernel reads `mpos_net`, `mpos_signal_frame`,
        `mpos_framebuffer_console` and `mpos_serial_ring` itself.
      - And the *dependencies* found their own places, because cargo reports `unused dependency`
        per target: `mpos-fdt` and `mpos-err` are aarch64's (the device tree, and the errno an
        unhandled `svc` answers with), `mpos-date` is x86_64's (the CMOS clock's calendar
        arithmetic), and only `mpos-vm`, `mpos-bootinfo` and `critical-section` are the seam's own.
        Written as one flat list first - which is what a move that just wants to compile produces -
        and then told apart by the tool.

      **The ratchet had to change two things, and one of them was a bug.** `ARCH_EXEMPT` drops to
      `["main", "lib"]`: `arch` no longer exists as a module of this crate, so the rule stopped
      being "the machine layer may name itself" and became "only the crate root and the wiring may
      reach for the machine at all". And the mutual-pair count went **9 → 8** - but not by the one
      that was expected:

      - `hal ↔ mm` collapsed with the move, as it should (the mapping vocabulary is a leaf path
        now, so `hal → mm` is zero while `mm → hal` remains, one-way).
      - `drivers ↔ rand` **arrived in the previous slice** (`docs/notes/` §14.3 S3, slice 2) and was
        *cancelling* that removal exactly. Folding the virtio-rng into `rand`'s seed is what let the
        machine layer stop naming a driver - but `rand → drivers` closed a cycle with `drivers →
        rand` (`/dev/urandom` reads the pool), so one pair left and another took its place while the
        count stood still. That is the plainest demonstration of what this ratchet cannot see, and
        the fix is the project's usual one: the source is now *injected*
        (`rand::set_device_entropy`, called from `main/<arch>.rs`), so `rand` names no driver.

      A fourth thing was found on the way and is filed here rather than fixed quietly:
      `rand::tests::a_reseed_re_keys_the_generator` was **flaky** - five tests over one global
      generator, run on several threads, so a neighbour's re-key made "a passive read must not
      advance the key" false. Measured 1 failure in 4 parallel runs, 0 in 4 serial ones; the tests
      now take a lock for the length of each body (0 in 6 after). It predates this step, and it is
      here because the gate runs it: a quarter of runs red is how a suite stops being believed.
- [x] **S5 — verify both machines.** Measured after S4: `cargo xtask boot-aarch64` green with the
      identical marker chain and `INIT_EXITED` (which is the evidence that `global_asm!`'s vector
      table and `critical-section`'s `set_impl!` survive being linked out of an rlib);
      `cargo xtask test --arch aarch64` **25/25**; `cargo xtask test` **154/155**, the one failure
      being `t-unix`, which was red before this work began and is not this work's; host unit tests
      **510**; `cargo xtask depgraph` within the ratchet with `mutual pairs 8/8`; xtask's own
      **65**.

### 14.4 What deliberately does not move

The Linux register ABI (`user_regs_struct`/`user_pt_regs`, the signal frame, the syscall gate) stays
in `arch/<arch>/`: it *is* the machine's register layout, and wrapping it in a neutral type would be
the private dialect `AGENTS.md` §3 rule 3 forbids. "No leakage" means a port writes `arch/<arch>/`
and edits no shared file — not that a register frame becomes architecture-neutral.

### 14.5 Reading the paths in this document

Every section above (and the stage ledgers in §9.1, and `ARCHITECTURE.md`) names the files as they
stood *when it was written*, and S4 moved two directories without rewriting them. The translation,
for every path that starts with one of these prefixes:

| Written as | Means, after S4 |
|---|---|
| `src/hal/…` | `arch/src/hal/…` |
| `src/arch/…` | `arch/src/arch/…` |
| `src/arch/x86_64/serial.rs`, `qemu.rs`, `mmio.rs` | the same under `arch/src/`, and the first two came from `src/drivers/` in S3 |

The history is left as history on purpose: a stage ledger that has been rewritten to match the tree
can no longer be used to tell what a stage actually did. This note is the single place the mapping
is stated, so a reader has one hop to make instead of a search.

### 14.6 The loader's half of the machine layer

`mpos-boot` was the last thing in the tree that named an architecture - 20 `target_arch` sites in
three groups, which its own file header listed:

| What | Was | Now |
|---|---|---|
| the progress lines' wire | `main.rs`'s `mod serial`, one `imp` per machine | `mpos_arch::early::{serial_init, serial_write}` |
| the page-table descriptor | `boot/src/vm/{x86_64,aarch64}.rs` | `mpos_arch::early::{PageTables, Frames}` |
| the last instruction | two `unsafe fn enter` bodies in `main.rs` | `mpos_arch::early::enter` |

**Criterion, measured**: `grep -r target_arch boot/src/` is **0**, and `boot → mpos` (the kernel)
is still absent from `cargo xtask depgraph`. The only machine-dependent thing left in the loader is
the target triple its files are compiled for.

**One deviation from §14.3's plan, and why.** The stage list said `arch/src/arch/<arch>/early.rs`.
It landed at `arch/src/early/{mod,x86_64,aarch64}.rs` instead, one level up, because
`arch/src/arch/**` is the *kernel's* machine layer and is gated `#[cfg(target_os = "none")]` while
the loader is `{x86_64,aarch64}-unknown-uefi`. Putting `early` inside that tree would have meant
gating most of `arch/mod.rs`'s re-exports on the target's operating system, which is a larger and
more fragile change than the one this stage is. So the loader's machine code is a sibling tree -
`arch/src/early/` beside `arch/src/{arch,hal}/` - and `arch/src/lib.rs` gains the crate's only
`#[cfg(target_os = "uefi")]`. When the kernel becomes a UEFI stub itself (item 5 of
`docs/TEST-AND-BOOT-REFACTOR.md`) that gate widens to `any(…)` and nothing else moves.

**`Frames`: who decides what.** The loader decides where frames come from - one contiguous
`LOADER_DATA` allocation, sized from RAM before the first frame is taken, never returned - and
`early` decides what is written into them. That seam is a one-method trait, `PageTables<F: Frames>`,
implemented by the loader's own `Pool`. Monomorphised, so the narrowing costs nothing at run time
and needs no `dyn`; and the builder *cannot* allocate anything the loader did not hand it, which is
a property the old arrangement could only state in prose.

**`set_log`: who owns the format.** The loader owns the text (`mpos-boot: ` and a newline) and
`early` owns the wire, so `early` registers a sink (`mpos_arch::early::set_log`) and its own two
lines - the device window it maps, and the configuration-table GUIDs when no tree is found - come
out through the loader's formatter. Same shape as `hal::kernel`'s registration, same reason: the
layer with the facts is not the layer that owns the presentation.

**Two things deliberately did *not* go down**, and the distinction is the point:

- `copy_device_tree` stays in `boot`. Reading the firmware's configuration table, taking the tree's
  self-declared length and copying the bytes into frames is UEFI work, not machine work - and it is
  the same code on both machines. The machine answers one boolean, `FIRMWARE_PUBLISHES_DEVICE_TREE`
  (false on x86_64, whose firmware describes it with ACPI), and the loader asks it by name instead
  of hiding the branch in a `#[cfg]`. That is also why `DTB_GUID`/`FDT_MAGIC`/`FDT_MAX_BYTES` are
  now unconditional constants: they were never per-architecture facts, they just happened to be
  compiled on one machine.
- `Pool`, the three ranges, `merge` and `pool_bytes_for` stay in `boot/src/vm.rs`, which now
  contains no `cfg` at all: the *plan* is architecture-free, and only the encoding is the machine's.

**A dependency that had to move with the code**: `mpos-arch`'s `critical-section` became a
`[target.'cfg(target_os = "none")'.dependencies]` entry. `hal::tx_ring` is its only user and `hal`
is not built for the loader, so leaving it unconditional made cargo report `unused dependency` for
both UEFI targets. An unused dependency is a dependency that will be wrong without anyone noticing,
which is exactly what that lint is for.

**Verification** (each of these is a command that was run):

| Gate | Result |
|---|---|
| `cargo build -p mpos-boot --target {x86_64,aarch64}-unknown-uefi` | both build, zero warnings |
| `cargo build -p mpos-arch --target {x86_64,aarch64}-unknown-uefi` | both build, zero warnings |
| `cargo xtask boot-aarch64` | full marker chain + `INIT_EXITED`, and the loader's own eleven progress lines byte-identical, including `device window 0x8000000..0xc000000 …` |
| `cargo xtask test` / `cargo xtask test --arch aarch64` | x86_64 155/155, aarch64 25/25 |
| `cargo xtask depgraph` | inside the ratchet, mutual pairs 8/8, no new edge |
| host unit tests | `mpos` 511 + every leaf crate |

`boot-aarch64` is the load-bearing one of those: it is the only gate that runs the loader's page
tables, its PL011 writer and its `enter()` as relocated code inside another crate, and the marker
chain it prints is unchanged character for character.

### 14.7 The loader becomes a crate, and the record one definition

Two smaller things landed before §14.8, and both are prerequisites of it.

`boot/` became a **library plus a thin `#[entry]` binary**: `boot/src/lib.rs` holds the whole
sequence as `mpos_boot::run()` and `boot/src/main.rs` is fifteen lines that call it. That is the
shape a second caller needs - and the kernel is the intended second caller (§14.8's successor is
item 5 of `docs/TEST-AND-BOOT-REFACTOR.md`, where the kernel takes the loader's place).

`boot/src/handoff.rs` was **deleted**. The loader now depends on the `mpos-bootinfo` leaf and reads
the same `Handoff` the kernel does, which closes risk **V-10** (`docs/ARCHITECTURE.md`): the two
copies kept honest by a magic number are one definition, read by both sides of the seam. Two type
aliases (`AbiRegion as Region`, `HANDOFF_MAGIC as MAGIC`) left every call site untouched.

| Gate | Result |
|---|---|
| `cargo build -p mpos-boot --target {x86_64,aarch64}-unknown-uefi` | both build, zero warnings |
| `cargo xtask boot-aarch64` | full marker chain, the loader's eleven progress lines byte-identical |
| `cargo test -p mpos-bootinfo --target x86_64-unknown-linux-gnu` | passes |

### 14.8 The kernel's entry is one file, and the machine's half goes down

**What was wrong.** `src/main.rs` was a 34-line dispatcher whose whole body was a `#[path]` choice
between two bodies, and each of those bodies re-stated the *machine's* half of the boot - the entry
register, SSE, the record, physical memory, the descriptor tables, the page tables, the wire -
before getting to the kernel's. Worse, the same seam already existed for one machine and not the
other: aarch64 had been given `mpos_arch::arch::aarch64::boot(handoff, milestone, devices, init)`,
and x86_64 inlined the equivalent. So the architecture-independent part of the entry was written
twice, and the machine layer's own entry point was written once.

**What changed.**

| File | What |
|---|---|
| `src/main.rs` | now the one entry and the one panic handler. `_start` takes the record in the platform's first argument register (`RDI` vs `x0` is the compiler's business, not this file's), registers the kernel services, and hands `board::start` the record; `panic` writes one line through `mpos_arch::arch::print_args`, calls the machine's context dump, and stops. **No `#[cfg]`, no `#[path]`** |
| `src/board/mod.rs` | **new**: picks the machine's body in one place - exactly the arrangement `arch/src/arch/mod.rs` uses for the library - and exports `start` and `panic_context` |
| `src/board/x86_64.rs` | was `src/main/x86_64.rs`. Keeps the acceptance checks, the bus enumeration (`devices`), the console and banner (`milestone`), the kernel's own boot (`init`) and the panic context dump; loses the entry, `init_arch` and the panic handler |
| `src/board/aarch64.rs` | was `src/main/aarch64.rs`. Keeps the device probes, the EL0 handoff and its checks; loses the entry and the panic handler |
| `arch/src/arch/x86_64/mod.rs` | gains `boot(handoff, milestone, devices, init)` and `print_args`: the x86_64 half of the seam, symmetric to the aarch64 one and built on the `hal::kernel` table §14.2 already put there |
| `arch/src/arch/mod.rs` | exports both under one name each, so `mpos_arch::arch::{boot, print_args}` resolve on either machine and the call site has no architecture in it |
| `arch/src/arch/aarch64/mod.rs` | `milestone` now takes the record: the command line in it decides *where* the kernel log goes (`console=`), which is why the first machine's counterpart reads it too |

**What moved down, and what could not.** The entry, SSE for user processes, the record, physical
memory, the descriptor tables, the kernel's page tables and the wire that reports them are now
`mpos-arch`'s on both machines - which is what "the machine layer is one crate" has to mean.
Everything else stayed in the kernel, and by construction rather than by taste: the milestone
checks, the device bring-up and the handoff all name `vfs`, `blk`, `net`, `drivers` or `sched`, and
those are names that do not exist in `mpos-arch` (§14's whole point). So the kernel-side per-machine
code is `src/board/<arch>.rs` - a body, not a second `main`.

**The one thing deliberately not unified, and why.** The two machines' boot *sequences* remain their
own, and so do their check suites. A shared sequence would have to reorder aarch64's milestones,
which are load-bearing and were each a bug once: `check_dev` must run after `vfs::tty::init`,
`check_console_rx` before the receiver is armed, `check_serial_tx` after it. Unifying the two suites
into one capability-driven one is a real change with a real payoff, but it changes what aarch64
*accepts*, so it belongs in its own stage rather than in the one that makes the entry one file.

**A log-order note.** The banner is now the kernel callback's first line (`milestone`), which runs
after the machine has read the record - so the three machine lines (`pmm:`, `image:`,
`page tables:`) precede it where they used to follow it. Nothing parses the order; the self-test
protocol reads `=== RUN/PASS/FAIL/SUMMARY`.

**Verification** (each of these is a command that was run):

| Gate | Result |
|---|---|
| `cargo build -p mpos --target x86_64-unknown-none` | builds; the two warnings are the pre-existing drift in `net/packet.rs` and `seccomp` |
| `cargo build -p mpos --target aarch64-unknown-none --release` | builds; only the pre-existing lib drift |
| `cargo build -p mpos-arch --target {x86_64,aarch64}-unknown-none` | both build |
| `cargo xtask test` | `=== SUMMARY pass=156 fail=0 (156 built)`, exit 33 |
| `cargo xtask boot-aarch64` | full marker chain, all twenty-two markers, `INIT_EXITED` |
| `cargo xtask depgraph` | inside the ratchet, mutual pairs 8/8, no new edge |
| host unit tests | `mpos` 511 passed |
| stale-path sweep | `grep -rn 'main/x86_64\.rs\|main/aarch64\.rs\|src/main/'` over `AGENTS.md docs/ src/ arch/` finds none |

`boot-aarch64` is the load-bearing one again: it is the only gate that runs the second machine's
`arch::boot` with three function pointers crossing the seam, and every marker it prints is
unchanged character for character.

### 14.9 The steps every machine's `init` runs are stated once

**What was wrong.** `board/x86_64.rs` and `board/aarch64.rs` each end in an `init` that hands the
machine to `/init`, and each one re-stated the same five steps. Some of the statements were
byte-identical - `vfs::file::init_standard(vfs::tty::init())`, the ramfs root over it, and
`assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1")` - and two more carried the
same *ordering contract* written twice: the console mux goes up before the receiver is armed, and
the first program must be pid 1. One of the two copies said so explicitly ("the order is the same
one and the same reason as x86_64's `vcmux::init(); serial::enable_rx()`"), which is the comment
that says the two should have been one function.

**What changed.**

| File | What |
|---|---|
| `src/board/steps.rs` | **new**: the five steps - `start_scheduler`, `mount_ramfs_root`, `unpack`, `arm_console`, `hand_to_init` - each carrying its contract in its own doc, and the module saying why the two `init`s are still two |
| `src/board/x86_64.rs` | `init` and `handoff_to_init` call the steps; the one-line `init_sched` wrapper is gone. `handoff_to_init` keeps what is its own: the boot-stack check at the deepest point, the exit-code discard, and the wait for the shell's child |
| `src/board/aarch64.rs` | `init` calls the steps in its own order; the read/`argv`/spawn trio it had inline is now one call |
| `src/board/mod.rs` | declares `mod steps;`; its doc says which half of a body is not the machine's |

**Why the steps are in the wiring crate and not the library.** `arm_console` is half machine
wiring - arming the receiver is `crate::arch::enable_console_interrupts` - and `ARCH_EXEMPT` in
`xtask/src/depgraph.rs` allows only `main`/`lib` to reach the machine layer, naming that very call
("arms its console interrupts") as wiring. The library's `boot` module was tried first and the
depgraph refused it for a second, independent reason: the steps need `crate::sched` and
`crate::vfs`, and `vfs` already names `boot` once (the command line behind `/proc/cmdline`), so
`boot` reaching back closed the graph's ninth mutual pair. The wiring crate sits above both, names
the machine by right, and adds no edge the ratchet can see.

**Why still not one function.** Only the five steps are shared; their *placement* is not. x86_64
arms the console after the spawn - nothing reads the wire until the shell is up - while aarch64 arms
it before `check_serial_tx`, which is a check *of* the armed path; x86_64 unpacks inside its
filesystem bring-up while aarch64 unpacks after its devices and switches root after that. A single
`run()` would have to take a list of hooks and reorder them, which is the two sequences wearing one
hat (§14.8's argument, still standing).

**Verification** (each of these was run, after the change):

| Gate | Result |
|---|---|
| `cargo xtask test` | `=== SUMMARY pass=156 fail=0 (156 built)`, exit 33 |
| `cargo xtask boot-aarch64` | all twenty-two markers, `INIT_EXITED` |
| `cargo xtask depgraph` | inside the ratchet, mutual pairs 8/8, no new edge |
| host unit tests | `mpos` 511, `mpos-xtask` 65 |
| both kernel targets | `x86_64-unknown-none` and `aarch64-unknown-none --release` build; warning counts unchanged from before the change |

The depgraph shaped this stage rather than merely checking it: the first placement of these steps
(`boot::init`) was rejected by it, and the rejection was right on both counts.

---

