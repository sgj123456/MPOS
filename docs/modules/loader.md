# loader

**Home:** `boot/` (crate `mpos-boot`) · **Owns:** the UEFI firmware phase that becomes the kernel — decompress the kernel it carries, read the two files beside it off the boot partition, build an address space, and stop being a program.

## What it owns

The firmware loads this, it loads the kernel, and then it stops existing. What it leaves
behind is a `Handoff`'s worth of plain data and an address space, handed over in one
instruction — so the interesting part of this crate is not the list of steps but the order
they have to happen in (`boot/src/lib.rs`):

1. **Take the kernel out of this image, and read its two companions off the boot
   partition** — the kernel is *inside* the loader (`PACKED_KERNEL`, written by
   `boot/build.rs`) and is inflated here; `\initramfs.tar` and `\mpos.cmdline` are the
   files. This is first because it is the only step that can fail in a way worth
   reporting, and because reading needs the firmware's file protocols, which do not
   survive step 5.
2. **Work out the memory map** — also firmware data, also gone at step 5, and what both
   the page tables and the kernel's frame allocator are built from.
3. **Allocate everything** — the page-table pool, the image frames, the boot stack, the
   decompressed kernel and the copy of the early userspace. After step 5 there is no
   allocator left, not even the loader's own.
4. **Build the address space and load the image into it**, relocated against
   `KERNEL_VIRT_BASE` so the higher-half jump lands where the kernel's own data already
   points.
5. **Leave boot services**, the point of no return.
6. **Fill in the handoff record and jump.**

The one thing the loader deliberately does *not* do is decide anything the kernel could
decide itself: it does not pick a root device, parse a command line or touch a file
system on the disk. It moves bytes and builds page tables; everything above that is the
kernel's business.

**The kernel travels inside the loader.** The boot volume carries one bootable file, not a
stub beside a kernel. `boot/build.rs` compresses the kernel for this machine into the
loader's own image — DEFLATE, with `miniz_oxide` on *both* sides of the wall, once as a
build dependency to compress and once as a runtime dependency to inflate — and `lib.rs`
inflates it before it opens the volume. This is Linux's `bzImage` shape, and it is worth
naming what it is and is not: the stub is still a stub, the kernel is still not a firmware
application, and the load path is unchanged in every step but the first. The shorter
spelling would have been an `include_bytes!` of a fixed path, and it is the worse one: the
kernel is precisely the input that moves, and a path written in the source is a loader that
goes on quietly carrying the previous kernel, because cargo has no reason to rebuild this
crate when only the kernel changed. A build script can declare the dependency that is real
— the kernel file's own bytes.

The two companions stay files, and the split is not arbitrary. They are *data* the kernel
is handed and unpacks itself; carrying them would mean rebuilding the loader into a larger
image for every file the early userspace gains, and the early userspace is a whole
distribution's worth of them. The kernel is the one thing the loader has to place in memory
and jump to. Linux draws the line in the same place — the kernel is in `bzImage` and the
initrd is not.

Because the kernel is inside the loader, the record crossing the two of them is now written
and read by compilations from a single build. That removes a whole class of ABI mismatch;
the `HANDOFF_MAGIC` check below stays as the guard for the class that remains (a boot
volume assembled or half-updated by hand), and it is cheap enough to keep for a reason that
has merely got smaller.

Since the crate split, the loader is a **library plus a thin `#[entry]` binary**:
`boot/src/lib.rs` holds the whole sequence as `mpos_boot::run()` and `boot/src/main.rs` is
a firmware entry that calls it and returns its `Status`. That shape is deliberate — it is
what a second caller needs, and the kernel is the intended second caller (docs/notes/ §14.7).
There is **no architecture in this crate at all**. The three machine things it needs — the
page-table descriptor, the progress wire, and the last instruction — are
`mpos_arch::early`, one arm per machine; `boot/src/vm.rs` contains no `cfg`, no descriptor
bit and no register name, because the *plan* is architecture-free and only the encoding is
the machine's. The two questions that really are per-machine are asked by name from
`mpos_arch::early`: `FIRMWARE_PUBLISHES_DEVICE_TREE` decides whether a device tree is
copied out of the UEFI configuration table, and `extra_mapped_bytes()`/`map_extra()`
decide whether anything beyond RAM is mapped (aarch64 maps its device window; x86_64 maps
nothing, because its kernel walks a BAR).

## Key files

| File | What lives there |
|---|---|
| `boot/Cargo.toml` | The crate's two targets: `uefi` with `alloc` + `global_allocator` (the firmware's pool allocator is the only one before boot services go), and the `[[bin]]` (`mpos-boot`, `src/main.rs`) beside the `[lib]` (`mpos_boot`, `src/lib.rs`). `miniz_oxide` appears twice on purpose — once in `[dependencies]` (the loader's inflate) and once in `[build-dependencies]` (the build script's deflate) — so that the two ends of the stream cannot be different DEFLATE dialects |
| `boot/build.rs` | Compresses the kernel for this machine into the loader's image. Finds it by `MPOS_KERNEL_ELF` (which `xtask` always sets, to the kernel it just built) or by deriving the path from the target, declares the file's bytes as a rebuild dependency, and writes `kernel.deflate` into `OUT_DIR` for `lib.rs` to `include_bytes!`. Writes an *empty* payload plus a `cargo:warning` when there is no kernel — the crate stays buildable, and the loader refuses to boot |
| `boot/.cargo/config.toml` | `[build] target = "x86_64-unknown-uefi"`, so a build started inside this directory does the right thing on its own |
| `boot/src/main.rs` | The `#[entry]`: `fn main() -> Status { mpos_boot::run() }`. Nothing else |
| `boot/src/lib.rs` | The whole sequence: `run()`, the two path constants and the carried kernel (`PACKED_KERNEL`, `unpack_kernel`), the stack sizing and its canary, the handoff `HANDOFF` static and `write_handoff`, `copy_to_frames`, `copy_device_tree`, `allocate`, `read`/`trim`/`kind_of`, `log`, the `#[panic_handler]`, and the `wcslen` shim |
| `boot/src/elf.rs` | Just enough ELF64: `Segment`, `Relocation`, `Range`, `Kernel::parse` (program headers, `PT_DYNAMIC`, section boundaries) and `Kernel::load` (copy segments, zero `.bss`, apply `R_X86_64_RELATIVE`/`R_AARCH64_RELATIVE` against the virtual base). Every read goes through a bounds-checked `Reader` that panics with the offset it refused |
| `boot/src/vm.rs` | The architecture-free plan: `HHDM_OFFSET`, `KERNEL_VIRT_BASE`, `Pool` (a bump allocator implementing `mpos_arch::early::Frames`), `is_ram`, `map_physical_memory`, `map_kernel_image`, `pool_bytes_for`, `merge`, and the `pub use mpos_arch::early::PageTables` re-export |
| `crates/bootinfo/src/lib.rs` | The one definition of the record both sides read: `Handoff`, `AbiRegion`, `HANDOFF_MAGIC`, `BOOT_STACK_CANARY`, `MAX_MEM_REGIONS`, the `KIND_*` constants, and the kernel's `BootInfo`/`MemKind`/`MemRegion` view. The loader depends on it; `src/boot/handoff.rs` re-exports the kernel's side |

## Contracts

**The entry point.** `mpos_boot::run() -> Status` is the whole sequence and the only thing
`main.rs` calls. The ordering contract is the load-bearing part: everything the firmware
owns — its file protocols, its memory map, its allocator, its console — is used before
`boot::exit_boot_services`, which is the point of no return, and
`write_handoff`/`HANDOFF`'s single write happens after it with no allocation, because
there is no allocator left (`write_handoff` writes the regions straight into the record's
fixed array and folds neighbours in one pass, UEFI memory maps being ordered by physical
address). A build with no kernel to carry — a `cargo check`, a lint — makes `run` answer
`Status::UNSUPPORTED` with one line on the wire, before the volume is opened: a loader
carrying nothing refuses by name rather than booting whatever it can find.

**The record.** `crates/bootinfo`'s `Handoff` is a `#[repr(C)]` wire format crossing from
one compilation into another, and it is the one crate both sides depend on, so its layout
is written once and cannot drift out of step with itself. `HANDOFF_MAGIC` is still checked
by the kernel before it reads anything else. Since the kernel moved inside the loader the
two ends are built together, so a mismatch cannot happen by accident — one whole class of
failure is gone — and the check stays for the class that remains, an image assembled or
half-updated by hand: changing a field changes the ABI the record is read with, so
`HANDOFF_MAGIC` is bumped unless the change is append-only (AGENTS.md §4). `BOOT_STACK_CANARY` lives beside the magic for the same reason: the loader
is the only party that knows where the boot stack begins, so it plants the mark in the
lowest word and reports the address in `boot_stack_base`; the kernel checks it once it is
up. The address is written through the direct map, not the physical one, because the
kernel's own tables have no identity mapping by the time it reads it.

**Where frames come from.** The loader decides where page-table frames come from and the
machine decides what is written into them. `Pool` is the loader's: one contiguous
`LOADER_DATA` allocation sized from RAM before the first frame is taken
(`vm::pool_bytes_for`), never returned, and not memory the kernel's frame allocator will
hand out — the memory kind stays bootloader-reclaimable after boot services exit. It
implements `mpos_arch::early::Frames`, whose single method hands back a zeroed frame
(zeroed because an empty page-table entry is an absent one). Running out of the pool is a
panic, not an `Option`, because the size is computed before the allocation.

**The address space.** `vm` lays out three ranges, each for a stated reason (its own module
docs): an **identity** map so the loader and its tables keep running at the addresses the
firmware loaded them at; a **direct** map (`HHDM_OFFSET + phys` over all RAM) so
`mm::init` can write the frame allocator's metadata before any kernel page tables exist;
and the **image in the higher half** at `KERNEL_VIRT_BASE`, mapped at both its physical and
its linked address so the jump to the alias makes dropping the identity map safe. A fourth
range — the device window — is added on top by `early::map_extra` on the machine that has
one. Only RAM is mapped: `vm::is_ram` excludes device registers and holes, and `vm::merge`
folds adjacent same-kind regions so the direct map does not map the same pages twice.
`vm::map_kernel_image` gives each section the permissions the kernel's own plan gives it —
text executable and read-only, rodata read-only, data writable and non-executable — which
is the W^X the kernel re-installs a moment later.

**Machine-owned facts the loader asks rather than branches on.** `early::serial_init`/
`early::serial_write` are the progress wire; `early::set_log` points `early::log` at this
crate's formatter, so the loader owns the text and the machine owns the wire;
`early::enter(record, entry, stack_top, root)` is the last instruction, and which register
carries the record is the machine's business. `elf::Kernel::parse` picks the one relocation
kind a static PIE against itself needs from the file's own `e_machine`, so a loader built
for one machine still refuses the other's image by name.

**A note on the boundary.** `mpos-boot` depends on `mpos-arch` and `mpos-bootinfo`, and it
must **not** depend on `mpos` — the kernel and the loader are separate compilations that
agree on a `#[repr(C)]` record, and a `mpos-boot → mpos` edge would be a cycle. `mpos-arch`
must not depend on `mpos` either. The loader's second prerequisite — the kernel becoming
the UEFI application itself and calling `mpos_boot::run` in-process — is scheduled, not
landed.

## What userspace sees

The loader leaves no syscall row: it is gone before userspace exists. What it does leave
userspace is data. The command line it copied is what `/proc/cmdline` reports and what the
initramfs's `init` reads to learn `root=` (the kernel records it through
`hal::kernel::set_cmdline`); the initramfs it loaded is the x86_64 boot's first root until
the initramfs's own `/init` pivots. The device tree it copies out of the firmware's
configuration table (on the machine whose firmware publishes one) is what the kernel's
aarch64 boot reads back as `BootInfo::dtb`. The record itself and the command line are
documented in `docs/modules/boot.md`; `docs/uapi/` is where the userspace-visible
surface as rows lives, and this crate contributes none of them directly.

## Pins

`mpos-boot` is bare-metal/UEFI-only and is not under `crates/`, so it is not in `xtask`'s
`LEAF_CRATES` and has no host unit test of its own — there is also no `t-*` program that
can run before the kernel exists. The pin is the gate, and it is real: every
`cargo xtask test` run boots an image this crate assembled, and `cargo xtask test --arch
aarch64` is the only run that exercises the aarch64 arm of `mpos_arch::early` (its
descriptors, its PL011 writer and its `enter`) as relocated code. A green x86_64 gate with
a red aarch64 one is exactly the failure the split pair is there to catch.

What is host-testable and is pinned:

| Pin | What it would catch |
|---|---|
| `cargo xtask test` (x86_64) | Boots the kernel through the record and the page tables this crate wrote; the loader's own progress lines precede every kernel line on the wire |
| `cargo xtask test --arch aarch64` | The same, for the second machine's `early` arm |
| `xtask/src/arch.rs`'s `each_machine_names_its_own_kernel_and_loader_targets` (host test) | That `x86_64`/`aarch64` name `x86_64-unknown-uefi`/`aarch64-unknown-uefi` — the loader is built for the target the machine says, not the workspace default |
| `xtask/src/arch.rs`'s `a_machine_this_front_end_does_not_build_is_refused_by_name` (host test) | That a machine the front end does not build is refused rather than run as another |
| `crates/bootinfo` in `xtask`'s `LEAF_CRATES` | That stage 1 host-builds the record's one definition. The crate carries no `#[test]` function today, so the record's behaviour is pinned by the boots above, not by a unit test — stated plainly rather than implied by the crate's presence in the list |

There are no linker `ASSERT`s in this crate (it has no linker script of its own); the
link-time placement checks the loader's generated image inherits are the kernel's, in
`arch/src/arch/aarch64/linker.ld`, and are documented on `docs/modules/arch.md`. The
loader's ELF reads are bounds-checked at run time instead: every offset goes through
`elf::Reader`, which panics with the offset it refused.

## Evidence and open work

The reasoning behind the crate's shape is notes/ §14.6 ("the loader's half of the
machine layer" — the three machine things that moved to `mpos_arch::early`, and the two
that deliberately did not) and §14.7 ("the loader becomes a crate, and the record one
definition" — the library-plus-`#[entry]` split and the deletion of the loader's own
`handoff.rs`, which closed the V-10 duplication recorded in docs/ARCHITECTURE.md). The
sequence itself is stated as prose at the top of `boot/src/lib.rs` and as the boot
diagram in docs/ARCHITECTURE.md §4 — link, do not copy. The one ordering rule that keeps
being load-bearing (allocate everything before `exit_boot_services`; leave boot services
once) is the loader's whole design, and the second caller that would reuse `run` in-process
is item 5 of docs/TEST-AND-BOOT-REFACTOR.md, tracked from docs/ROADMAP.md. The handoff
record and the command line userspace comes to depend on are `docs/modules/boot.md`'s
subject, not this page's.
