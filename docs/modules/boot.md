# boot

**Home:** `src/boot/` · **Owns:** the handoff record and the command line the loader leaves behind, and the kernel's view of the machine they describe.

## What it owns

`src/boot/` is the kernel's side of the seam between the firmware and the machine. The loader
(`mpos-boot`, crate `mpos-boot`) reads three files off the boot partition, builds an address space,
leaves boot services and jumps; what crosses that jump is one `#[repr(C)]` record and a command
line. This module is what the kernel does with them: it *translates* the record into plain data,
keeps the command line where the filesystems can read it, and owns the kernel console list the
command line spells.

The record itself is deliberately **not** declared here. `Handoff`, `AbiRegion`, the `KIND_*`
constants, `HANDOFF_MAGIC` and `BOOT_STACK_CANARY` are one definition in the `mpos-bootinfo` leaf
crate (`crates/bootinfo/src/lib.rs`), because both sides of the seam have to name them and neither
can own them: `mpos-arch`'s `Paging::install` takes a `BootInfo` and the aarch64 entry takes a
`Handoff`, and a machine crate cannot depend on the kernel. `src/boot/handoff.rs` re-exports that
one definition under the names it has always had, so every `crate::boot::handoff::` caller is
unchanged, and the loader (`boot/src/lib.rs`) depends on the same leaf - so the layout is written
once and cannot drift out of step with itself. The loader's `boot/src/handoff.rs` is gone; there is
no second copy of the record anywhere.

What stays in this module is what the *kernel* does with the record, which is not the loader's
business: `collect` is the code that decides what a memory region *means*, `cmdline` is where the
bytes become a `&'static str` the VFS can serve, and `console_list` is a policy the kernel applies
to a word the loader only carried. What does not belong here: the loader's sequence and its ELF
relocations (`boot/src/`, see [loader](loader.md)); the machine layer's translation of the record
into page tables and a direct map (`mpos-arch`); and the `/proc/cmdline` node, which is the VFS's.

## Key files

| File | What lives there |
|---|---|
| `src/boot/mod.rs` | The `BootInfo`/`MemKind`/`MemRegion` re-exports, the `CMDLINE` static, and `set_cmdline`/`cmdline`/`cmdline_has` |
| `src/boot/handoff.rs` | The record's re-export, and `collect` - the kernel's translation of a `Handoff` into a `BootInfo` |
| `src/boot/console_list.rs` | `ConsoleSink`, `ConsoleList::from_cmdline`/`DEFAULT`, and the `has_serial`/`has_vt`/`prefers_serial` accessors |
| `crates/bootinfo/src/lib.rs` | The one `Handoff`/`AbiRegion` definition, `HANDOFF_MAGIC`, `BOOT_STACK_CANARY`, `MemKind`/`MemRegion`/`BootInfo` and `MAX_MEM_REGIONS` |
| `boot/src/lib.rs` | `mpos_boot::run` - the whole loader sequence as a library function, and `write_handoff` |
| `boot/src/main.rs` | The `#[entry]` whose whole body is `mpos_boot::run()` |
| `boot/src/elf.rs`, `boot/src/vm.rs` | The loader's PIE loading/relocation and its page-table/mapping internals (loader side of the seam, documented with [loader](loader.md)) |

## Contracts

- `Handoff` is a wire format: `#[repr(C)]`, written by one compilation (the UEFI loader) and read by
  another (the kernel), with every field a promise to a binary this crate never sees. `HANDOFF_MAGIC`
  is checked before anything else is read: a boot volume assembled or half-updated by hand is a real
  thing to refuse, and the check costs one comparison. `boot_stack_base` and the two device-tree fields are last on purpose, so
  appending them left every older offset untouched (the field's own doc argues why).
- `collect(at: *const Handoff) -> BootInfo` is `unsafe` and validates rather than trusts: it asserts
  the magic, caps `region_count` at `MAX_MEM_REGIONS`, reads the command line as UTF-8 (a bad line
  fails here, not at every later use), and refuses a record whose *allocatable* regions contain the
  boot stack (`NOTES.md` §13.17) - the frame allocator's zones are exactly those regions, so a
  loader that described the stack as `Usable` would hand the kernel the stack it is standing on.
- `BootInfo` is not a wire format: plain data, no pointers, no page-table root, no EFI table, with
  `phys_to_virt`, `regions`, `allocatable_bytes` and `MemKind::is_allocatable` (`BootloaderReclaimable`
  is deliberately not allocatable, because it holds the boot stack until the scheduler gives every
  task its own).
- `set_cmdline` / `cmdline` / `cmdline_has` - the command line kept where the VFS reads it
  (`/proc/cmdline`), with `cmdline_has` the one whole-word spelling two boots must agree on so
  `mpos.selftest` cannot mean one thing on one machine and another on the other.
- `ConsoleList::from_cmdline(cmdline, unknown)` - a pure, allocation-free parse of every `console=`
  word, following Linux's rule: repeatable, output to every console named, the last preferred, and a
  value this machine does not have reported to `unknown` and ignored (`DEFAULT` when none is known).
  Only `ttyS0`/`ttyAMA0` (the serial wire) and `tty0` (the framebuffer VT) are recognised; `ttyS1..`,
  `ttyAMA1..` and `tty1..tty6` are reported unknown rather than approximated.
- The loader's side of the contract: allocate everything before `exit_boot_services`, write the
  canary into the lowest word of the boot stack, and fill the record only after boot services are
  gone (one static write, so no half-filled record is ever reachable).

The edges - who may call `boot`, and which modules this one may name - are
`docs/ARCHITECTURE.md` §2/§3. The whole handoff sequence is drawn in `docs/ARCHITECTURE.md` §4.

## What userspace sees

`boot` owns one userspace fact: the command line. `src/vfs/procfs.rs` renders `/proc/cmdline` from
`crate::boot::cmdline()`, and the initramfs's `/init` reads `root=`, `rootfstype=` and `rootflags=`
from it (`assets/initramfs/core/init`, one script for both machines). The `console=` parsing is kernel-side policy,
not a syscall, and its honesty gap is recorded in `docs/UAPI.md` §5.3: `console=ttyN` pinning is
refused rather than approximated, and `/dev/console` has no preferred-console plumbing (`NOTES.md`
§7.1c). The handoff record itself never reaches userspace.

## Pins

The module's real pins are host tests, and they are the reason the crate is `no_std` but
`test`-compiled on the host. Reading `src/boot/handoff.rs` (`#[cfg(test)] mod tests`):

| Test function | The claim it would fail without |
|---|---|
| `the_record_lays_out_the_way_the_loader_writes_it` | every `Handoff`/`AbiRegion` field offset, so a field inserted in one copy is a compile-time-shaped failure rather than a wrong boot |
| `the_magic_is_the_loaders_name_spelled_backwards_in_le_bytes` | `HANDOFF_MAGIC == *b"MPOSBOOT"` as LE bytes |
| `every_region_kind_the_loader_names_is_translated` | each `KIND_*` maps to its `MemKind`, and an unknown kind is `Reserved` (pessimistic, not usable) |
| `an_empty_record_reads_as_nothing_loaded` | a zeroed record reads as absent, not as address zero |
| `the_command_line_and_the_regions_come_out_of_the_record` | `collect` copies the cmdline, bases and region kinds out of the record |
| `a_zero_length_range_is_absent_not_a_slice_at_zero` | a zero-length initramfs is `None`, not a slice from the stale pointer |
| `the_device_tree_comes_out_of_the_record_like_any_other_blob` | the DTB crosses like the initramfs, and zero length is `None` |
| `a_record_that_keeps_its_boot_stack_unallocatable_is_read` | the record is read and the stack stays put |
| `a_record_that_offers_the_boot_stack_is_refused` | `should_panic("offers the boot stack")` - the refusal above |

And reading `src/boot/console_list.rs` (`#[cfg(test)] mod tests`):

| Test function | The claim it would fail without |
|---|---|
| `a_line_with_no_console_word_keeps_the_default_list` | no `console=` word yields `ConsoleList::DEFAULT` |
| `a_repeated_parameter_names_every_console_and_the_last_is_preferred` | repeatability and last-wins preference, in both orders |
| `one_word_picks_one_console` | `console=ttyS0` is serial-only and `console=tty0` is VT-only |
| `a_console_the_machine_does_not_have_is_reported_and_dropped` | `tty1`/`ttyS1` are reported unknown and dropped |
| `an_aarch64_serial_console_names_the_wire_and_is_preferred` | `ttyAMA0` is the wire, preferred, with no phantom VT sink |
| `an_aarch64_extra_port_is_reported_unknown_like_its_x86_twin` | `ttyAMA1` is refused like `ttyS1` |

Two things are **not** pinned, stated plainly rather than guessed. `crates/bootinfo/src/lib.rs` and
everything under `boot/` contain no `#[cfg(test)] mod tests` (verified by reading and grepping), so
the one record definition and the loader's sequence are compile-checked by both sides reading the
same layout, not host-tested. And there is no `t-*` program covering this module: grepping
`user/tests/src/bin` and `user/abitest` for the record, `Handoff`, `cmdline` or `console=` finds none
(the `handoff`/`cmdline` hits are unrelated words in `t-prime`/`t-memfd-shm`). The command line's
userspace consumer is the initramfs `/init` script, which is a shell program, not a `t-*` test.

## Evidence and open work

The reasoning is in `docs/NOTES.md`: §14.7 (the loader becomes a library and the record one
definition), §14.6 (the loader's half of the machine layer moved to `mpos-arch::early`), §13.17 (why a
record offering the boot stack is refused), §13.23 (`BOOT_STACK_CANARY` lives in the shared leaf),
§7.1c (the console-list gaps), §14.9 (why the boot *steps* did not come back into this module), and
§15.6/§15.7 (the boot flow and what a boot's verdict is now). The plan and status are
`docs/ROADMAP.md` (Phase 14, Phase 15); the sequence is `docs/ARCHITECTURE.md` §4 and §3's `Board`
row. No number from those sections is restated above.

One doc-level note: `docs/ARCHITECTURE.md` §9's `V-10` still says "the canary constant in
`src/main.rs` is still spelled on both sides". Reading the code, that sentence is stale - `src/main.rs`
declares no canary, `src/board/report.rs` imports `BOOT_STACK_CANARY` from `mpos::boot::handoff`, and
`boot/src/lib.rs` imports it from `mpos_bootinfo`, i.e. one definition in `crates/bootinfo`. This is
the doc's inference from reading, not something the source comment states.
