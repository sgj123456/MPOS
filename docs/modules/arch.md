# arch

**Home:** `arch/` (crate `mpos-arch`) · **Owns:** the machine layer — the capability traits a port implements, the two ports behind them, and the machine code the loader needs before a kernel exists.

## What it owns

This crate is what used to be `src/hal/` and `src/arch/` inside the kernel, and it is a
crate rather than two more modules for exactly one reason: a module boundary could not
stop the machine layer from reaching up into the kernel, and a crate boundary can.
Everything in here is written against `mpos-vm`, `mpos-bootinfo`, `mpos-fdt`, `mpos-date`
and `mpos-err` — leaves the kernel depends on too — and against nothing else, so
`crate::sched`, `crate::mm` and `crate::println!` are not names that merely *should* not
appear here; they are names that are not in scope, and a machine that reaches for one
does not link (`arch/src/lib.rs`). That is the rule the two programs of AGENTS.md §3 —
a module may call downward only — get from rustc instead of from a ratchet.

It owns the seam in both directions. The **downward** face is `arch/src/hal/mod.rs`: a
set of capability traits (`Clock`, `WallClock`, `Cpu`, `Interrupts`, `Paging`, `Contexts`,
`Console`, `Entropy`, `Gate`) that a port implements *for* `hal::Current`, plus one
forward function per capability, so code above names no architecture and pays no
indirection — there is one implementation and no `dyn`, so every `hal::ticks()` compiles
to the direct `timer::ticks()` it replaced. The **upward** face is `arch/src/hal/kernel.rs`:
the questions a trap entry has to ask the kernel ("is this page one to populate?",
"charge this tick"), declared as the `Kernel` trait here and answered by the kernel
through one registered `&'static dyn Kernel`. The asymmetry is deliberate: the downward
path is hot and stays monomorphised, while the upward path is a fault, a tick or a log
line and can afford one indirect call — and the exception entry points are reached from
`global_asm!`, so they cannot be generic anyway (docs/notes/ §14.2).

The crate is target-gated in one unusual way, and that gate is what lets one crate hold
both halves of `arch`. `hal` and `arch` are compiled for `target_os = "none"` — the
running kernel — while `early/` is compiled for `target_os = "uefi"` — the loader. A
loader build compiles the machine code it needs (page-table descriptors, the progress
wire, the last instruction) and none of the kernel's; a kernel build compiles the
capability traits and the ports and none of the loader's. The two trees write the *same
descriptor format* and that is exactly the trap: `arch/<machine>/paging.rs` decodes as
well as encodes and runs on tables the kernel owns, while `early/` encodes only, a leaf
at a time, on tables built from a firmware allocation — and a caller that reached the
wrong one would be reading a `BootInfo` nobody built, or writing into an identity map
that no longer exists (`arch/src/early/mod.rs`, notes/ §14.6).

What deliberately does **not** live here is as much the point:

- The Linux ABI's register layout is the machine's and stays in `arch/<arch>/`:
  `user_regs_struct`, the `sigcontext` field order, the `syscall`/`iretq` gate. Wrapping
  it in a neutral type would be the private dialect AGENTS.md §4 rule 3 forbids. So "no
  leakage" means a port writes `arch/<arch>/` and edits no shared file — not that a
  register frame becomes architecture-neutral (`arch/src/hal/mod.rs`).
- The boot wiring, the device bring-up and the milestone checks are `src/board/<arch>.rs`
  in the kernel, because what is left in them names `vfs`/`blk`/`net`/`drivers`, names a
  machine crate cannot have.
- The loader's *plan* — the three ranges, `HHDM_OFFSET`/`KERNEL_VIRT_BASE`, the pool
  sizing, `merge` — is `boot/src/vm.rs`, because it is architecture-free; only the
  *encoding* of that plan is this crate's `early/`.
- The crate is deliberately **not** under `crates/`. That directory means "host-testable
  leaf", and `xtask`'s `every_leaf_crate_is_tested` pins its contents against the crates
  the host unit-test sweep builds; this one cannot be host-compiled at all — it is
  `cli`/`sti`, descriptor tables and MSRs (`arch/Cargo.toml`).

## Key files

| File | What lives there |
|---|---|
| `arch/Cargo.toml` | The crate's own rules: `mpos-vm`/`mpos-bootinfo` for both halves; x86_64's `uart_16550`/`spin`/`x86_64`/`mpos-date` and aarch64's `mpos-fdt`/`mpos-err` in per-target blocks; `critical-section` gated to `target_os = "none"` because its only user (`hal::tx_ring`) is not built for the loader |
| `arch/src/lib.rs` | The crate docs and the only three module gates: `arch` (`target_os = "none"`), `hal` (`target_os = "none"`), `early` (`target_os = "uefi"`). The one `#![cfg_attr]` enables `abi_x86_interrupt` on the x86_64 kernel build |
| `arch/src/hal/mod.rs` | The capability traits, their associated types, `Current`, the one-line forwards, the optional capability traits, the direct-map helpers (`phys_to_virt`/`virt_to_phys`/`set_direct_map_offset`), and the QEMU test-exit contract (`TEST_EXIT_OK`/`TEST_EXIT_PANIC`, `exit_qemu`) |
| `arch/src/hal/kernel.rs` | The upward face: the `Kernel` trait, the `MemoryStartup` answer, the once-written `REGISTERED` slot, `register`, one forward per method, and the `kprintln!` macro |
| `arch/src/hal/tx_ring.rs` | The console's transmit queue, parameterised by a `Transmitter`: machine-neutral, so both UARTs drain one copy of "who sleeps for room, and what a visit from the interrupt hands over" |
| `arch/src/arch/mod.rs` | The port facade: private submodules and only the items the wiring layer may name (`kernel_sections`, `boot`, `print_args`, `halt`, `enable_console_interrupts`; x86_64 also `breakpoints_caught` and the ABI register-table check) |
| `arch/src/arch/x86_64/` | `gdt`/`idt`/`tss`, `pic`, `lapic`, four-level `paging` + `mmio` (the BAR window allocator), `context`, the `syscall` gate, `signal`, the PIT `timer`, the CMOS `rtc`, `serial` (COM1), `qemu` (debug exit), `critical` |
| `arch/src/arch/x86_64/linker.ld` | The position-independent kernel image: no absolute base, section symbols (`__text_start`/`__text_end`, `__rodata_*`, `__data_start`, `kernel_end`) the kernel maps its own image by |
| `arch/src/arch/aarch64/` | `vectors` (the exception table), `gic` (GICv2), `paging`, `context`, `signal`, the generic `timer`, `pl011` (the console), `pl031` (the wall clock), `critical` |
| `arch/src/arch/aarch64/linker.ld` | The same section layout, plus the link-time `ASSERT`s that every `global_asm!` code symbol lands in the executable range (see Pins) |
| `arch/src/early/mod.rs` | The loader's half: `PAGE_SIZE`, the one-method `Frames` trait, the per-machine re-exports (`PageTables`, `Frames`, `enter`, `map_extra`, `extra_mapped_bytes`, `serial_init`, `serial_write`, `FIRMWARE_PUBLISHES_DEVICE_TREE`), the `LOG` slot and `set_log`/`log` |
| `arch/src/early/x86_64.rs` / `arch/src/early/aarch64.rs` | The descriptor encoding, the wire (COM1 / PL011) and `enter` — `cr3`+`jmp` / `ttbr0_el1`+`br` — each written to match the kernel-side `paging.rs` bit for bit |

## Contracts

**Downward.** Everything above the machine layer calls the forwards in `arch/src/hal/mod.rs`,
never `Current`'s methods and never a port module. `hal::Platform` names only the baseline
concerns; the optional capabilities are reached through their own forwards, each gated on
an `mpos_*` capability flag the build declares per target in `.cargo/config.toml` —
`mpos_msi`, `mpos_serial_ring`, `mpos_fpu_trap`, `mpos_signal_frame`, `mpos_console_input`,
`mpos_qemu_debug_exit`. The gate names the capability, never the architecture: `hal/mod.rs`
contains no machine name at all, and a capability a machine does not have is *absent*
rather than a method every machine has to fake an answer for. The invariant each trait
keeps is the reason it exists: `AddressSpaceOps::map` asserts on a collision (a bug in the
kernel's own plan) while `try_map` answers `PageTaken`, because a process's `MAP_FIXED`
must be refusable; `Paging::map_mmio_window` answers `None` when the machine cannot map
the range rather than a bogus address; `Console::interrupt_driven` returns an honest
`false` on a polled-only machine, so the console takes the polled path and never parks on
a queue nothing drains; `SignalFrame::frame_at` exists because the two machines' frames
begin at different offsets from the `rt_sigreturn` stack pointer; `Gate`'s frame stays an
opaque `*mut ()` because no type both machines share can be named without inventing one.

**Upward.** A trap or interrupt entry asks the kernel through `arch/src/hal/kernel.rs`:
`hal::kernel::resolve_cow_fault`, `resolve_anon_fault`, `resolve_stack_growth`,
`kernel_lazy_accessible`, `send_signal`, `disposition_is_default`, `exit`, `preempt`,
`charge_cpu`, `device_not_available` (gated `mpos_fpu_trap`), `dispatch_syscall`, plus the
boot and diagnostic half (`print_line`/`kprintln!`, `force_serial`,
`route_console_input`, `service_network`, `set_cmdline`, `boot_info`, `start_memory`,
`alloc_frames`/`free_frames`/`frames_free`, `wake`). The kernel registers one
implementation with `hal::kernel::register` before anything that can take a trap exists;
`register` panics rather than overwriting a second registration, and a read before that
point panics by name rather than becoming a null call.

**The facade.** `mpos` re-exports both modules under the names they already had
(`pub use mpos_arch::{arch, hal};` in `src/lib.rs`), so every `crate::hal::…` and
`crate::arch::…` call site and its surrounding commentary stayed true across the split.
`arch/src/arch/mod.rs` is a deliberately small facade: the submodules are `pub(crate)`,
so a new caller cannot reach `arch::aarch64::pl011` by accident, and `arch → hal` is the
only arrow between the two modules (`Current` is `hal`'s own type, so a `hal`-names-`arch`
alias would make the pair mutual and `cargo xtask depgraph` counts that as a cycle).

**Boot-time contracts the ports keep.** Each `arch/<arch>/mod.rs` exposes
`boot(handoff, milestone, devices, init)` — the same signature and the same three
callbacks on both machines, which is what makes the kernel's `_start` one piece of
architecture-independent code — plus `kernel_sections()`, `print_args`, `halt`,
`init_interrupts` and `enable_console_interrupts`. The three callbacks travel down because
the console service, the device drivers and the handoff all sit above this crate and may
not be named from it; `arch` only ever invokes the pointers (docs/notes/ §14.8).

**The loader's half.** `early::Frames` is the seam that decides who allocates: the loader
hands over one contiguous run of `LOADER_DATA` frames (`boot::vm::Pool`), and
`PageTables<F: Frames>` decides what is written into them — monomorphised, so the
narrowing costs nothing and the builder cannot allocate anything the loader did not give
it. `set_log`/`log` is the same shape as `hal::kernel`'s registration and for the same
reason: the loader owns the text (`mpos-boot: ` and a newline) and `early` owns the wire;
a line logged with no sink registered is dropped, not panicked, because the only witness
to a boot that dies while trying to print would be the line that could not be printed.

**The rule the whole crate exists for.** `mpos-arch` must not depend on `mpos`: the
kernel depends on this crate, and the reverse edge would be a dependency cycle — which is
why the crate cannot name a kernel service except through the registered `Kernel` table,
and why `mpos-boot` must not depend on `mpos` either. Who may call what is drawn in
docs/ARCHITECTURE.md §2 and §3 and measured by `cargo xtask depgraph`; this page does not
restate the edges.

## What userspace sees

This crate owns no `docs/uapi/` row of its own — userspace never names a capability or
a port. What it owns is the *per-machine half* of rows that other modules document: the
`open(2)` flag vocabulary, the `struct stat`/`statx` sizes and field placements, the signal
frame's layout and the register file's field order all have one spelling per machine, and
it is the machine's spelling. `docs/uapi/` carries each row with Linux's answer, this
kernel's answer and the pin; the machine's half of those rows is implemented by
`arch/<arch>/signal.rs`, `arch/<arch>/context.rs` and the ABI tables the ports expose
(`ABI_REG_COUNT`/`ABI_REG_INDEX`), and every one of them is pinned on both machines (see
Pins).

## Pins

`mpos-arch` is not a host-testable crate — it is descriptor tables, MSRs and port I/O, and
`xtask`'s `every_leaf_crate_is_tested` deliberately does not list it because it is not
under `crates/`. So the pin is the bare-metal gate and the link, and both are real:

| Pin | What it would catch |
|---|---|
| `cargo xtask test` (x86_64) | Boots the kernel through this crate's `arch::x86_64::boot`: the page tables, the interrupt controllers, the syscall gate and every `t-*` program below, under the `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol |
| `cargo xtask test --arch aarch64` | The same gate for the second machine, which is the only run that exercises `arch::aarch64` — its vectors, GIC, timer, PL011, PL031 and `paging` — and the machine layer's own boot checks: the timer, context switches, the kernel's page tables, the device tree's windows, EL0 entry, address spaces, the paging operations, the wall clock, per-CPU state, the serial receiver and TLS. They run on every boot (not only in a test build) and halt the machine with a line naming the subsystem. Nothing parses their output; what the sweep reads is the `user` half's own protocol |
| `arch/src/arch/aarch64/linker.ld` `ASSERT`s | That `mpos_enter_frame`, `mpos_context_switch`, `mpos_context_round_trip` and `mpos_task_trampoline` land inside `__text_start..__text_end`. Every `global_asm!` in the crate is one assembly unit, and a block that changes section without restoring moves everything assembled after it; measured, one such leak put a context-switch block in `.rodata.el0_program` and the first call to it was an instruction abort inside the image (docs/notes/ §13.16). The x86_64 script has no such `ASSERT`s |
| `cargo xtask depgraph` | That no module outside `arch`/`hal`/`main`/`lib` names `crate::arch::`, and that no new edge appears — the crate boundary and the measured ratchet move together |
| `mpos-vm`'s host tests (`crates/vm/src/lib.rs`) | The mapping *plan* this crate's `Paging::install` consumes: `every_mapping_is_page_granular`, `nothing_is_both_writable_and_executable`, `no_two_entries_cover_the_same_page`, `the_kernel_image_is_mapped_with_per_section_permissions`, `the_direct_map_covers_all_physical_memory` |
| `mpos-bootinfo`'s host build | The record `Paging::install` receives and the aarch64 entry is handed; the crate is listed in `xtask`'s `LEAF_CRATES`, so stage 1 of the gate builds it for the host |

The `t-*` programs are the real-machine pins for the contracts above; each fails if the
capability's answer changes:

| Program | Contract it pins |
|---|---|
| `user/tests/src/bin/t-signal-frame.rs` | `SignalFrame::enter`/`exit` and the two machines' `rt_sigframe` layout — every register the trapped call was using comes back, and on aarch64 the vector half |
| `user/tests/src/bin/t-fpu-across-switch.rs` | `Contexts`/`FpuTrap`: the floating-point state belongs to a task, not the CPU |
| `user/tests/src/bin/t-regs-across-syscall.rs` | `Gate`/`UserFrameOps`: what a trap from EL0 gives back unchanged, integer and SIMD alike |
| `user/tests/src/bin/t-arch-prctl.rs` | x86_64's `Cpu::set_thread_pointer` (the `%fs` base behind `arch_prctl`); it is the program `tests.toml`'s `aarch64` group excludes, because the call has no arm64 counterpart |
| `user/tests/src/bin/t-set-tid-address.rs` | The thread-pointer/TLS contract from the `set_tid_address` side on both machines |
| `user/tests/src/bin/t-lazy-buffer.rs` | `Paging::user_accessible`'s three rules: an absent page is the VMA table's to answer, a copy-on-write page counts as writable, `len == 0` is accessible |
| `user/tests/src/bin/t-open-flags.rs` | The per-machine `open(2)` flag *numbers* — the table `arch/<arch>/` must not share |
| `user/tests/src/bin/t-rtc.rs` / `t-localtime.rs` | `WallClock::boot_epoch`/`set_boot_epoch`/`write_epoch` through `/dev/rtc0` and a real calendar conversion |

## Evidence and open work

The machine seam's reasoning, every measurement and the lessons that produced it are
notes/ §13 ("Machine-seam purity and aarch64 parity") — §13.1 (the flag, not the
architecture), §13.2 (the callers stop spelling the machine; still in progress), §13.9
(the uaccess rules), §13.14 (a boot check must leave the machine as it found it), §13.16
(the assembly section leak and the `ASSERT`s), §13.21/§13.22 (the exception entry's
`ELR_EL1`/`SPSR_EL1` discipline). §14 records the crate split itself: §14.2 the static
hook table and why it is not a generic parameter, §14.6 the loader's half, §14.8 the
kernel's entry becoming one file. §15 records the two machines' boot flow converging.

The status of the remaining work — the aarch64 parity thread and the not-yet-unified
callers (13.2) — is the status board at the top of docs/ROADMAP.md (the "13
Machine-seam purity + aarch64 parity" line) and Phase 15's own section. The intended shape
of the module graph and the meaning of the edges are docs/ARCHITECTURE.md §1 (the
repository as crates), §2 (the module graph) and §3 (what each layer owns); this page
points at them rather than copying them.
