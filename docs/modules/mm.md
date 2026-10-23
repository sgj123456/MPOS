# mm

**Home:** `src/mm/` · **Owns:** physical frames, the slab heap, the address-space mapping plan and the VMA table, and the ELF64 program-image parser

## What it owns

`mm` is the kernel's memory subsystem, layered deliberately: the frame allocator comes first and
everything else is built on it, because the heap, the page tables and the task stacks are all
*clients* of it rather than prerequisites. `src/mm/mod.rs`'s header states the ordering as a design
rule — `init` brings up physical memory and nothing else, and the heap needs no setup of its own
because it takes frames on demand. `pframe.rs` is the bottom of the stack: a buddy allocator with
one zone per usable memory region, whose metadata is carved out of the very memory it manages (a
free-list head per order plus one state byte and one reference-count word per frame), because it
has to run before the heap exists and must not itself allocate. `heap.rs` is the next layer:
power-of-two size classes served by intrusive free lists, growing by taking runs of frames from the
allocator. `vm.rs` is the *plan*: which virtual ranges point at which physical ranges, with which
permissions, re-exported from the `mpos-vm` leaf. `vma.rs` is the per-process record of what an
address space is *supposed* to be, which is the question a page fault asks and a page table cannot
answer. `elf.rs` parses an executable's program headers into a placement plan the loader consumes.

What deliberately does **not** live here is stated in the module itself. The page-table *format*
belongs to `arch`; the direct map (`phys_to_virt`, `virt_to_phys`, `hhdm_offset`) lives in `hal`,
where the machine's page-table code can reach it, and `mm::mod` keeps only one-line forwards rather
than a second copy. The MMIO window allocator that used to sit here moved to the machine it belongs
to. And a frame-backed mapping is backed by `mm::vma::MappedObject` — three questions and no files —
rather than by `vfs::Inode`, which is the seam that closed the `mm`↔`vfs` cycle recorded as V-13 in
`docs/ARCHITECTURE.md` §9. The VFS answers that trait for its inodes through `vfs::MappedInode`.

**The layering rule this module is the clearest test case of.** `AGENTS.md` §3 and `src/lib.rs`'s
header make host-compilation a per-module judgement, and the transitive half is the one the
compiler cannot see: `src/mm/mod.rs` is where it bites. `elf`, `pframe`, `vm` and `vma` are
declared unconditionally and so compile and run on the host; `heap` is declared
`#[cfg(target_os = "none")] pub mod heap;`, so it does **not**. That gate is the honest
consequence of a real dependency — the heap takes and returns frames through
`alloc_pages`/`free_pages`, which are themselves machine-only — but it is also the one place in
`src/mm/` where logic that is pure arithmetic (the size-class choice in `class_of`, the order
arithmetic in `large_order`, the block derivation in `block_of`, and the `Slab` bookkeeping)
sits behind the machine gate and hosts no `#[cfg(test)]` at all. `heap.rs`'s own comment says
"the heap is the same on both machines" while the *voice* of one is not, which is exactly the
shape the rule is meant to keep host-testable. Note the discrepancy this leaves standing, as an
inference from the code rather than a claim the code makes: `src/lib.rs`'s aarch64 ledger and
`docs/ARCHITECTURE.md` §8's "host-compiled" box both list the heap among the host-compiled parts
of `mm`, and the `mod` declaration in `src/mm/mod.rs` says otherwise. One of the two is stale;
this page follows the code.

`mm` therefore owns the *policy* of the mapping plan and none of the *mechanism*: a port consumes a
list of `Mapping`s and does not get to invent policy (`vm.rs`'s header). The "how is it mapped"
half is `hal`'s — the vocabulary had to move below both `mm` and the machine layer once the machine
layer became a crate that cannot name `mm` (`docs/notes/` §14, step S2b). The paths every caller
already used are unchanged.

## Key files

| File | What lives there | Host-compiled? |
|---|---|---|
| `src/mm/mod.rs` | The module wiring: the `PMM` static, the module-level frame API, the pinned-range registry, the boot `init`, and the forwards to `hal`'s direct map. Almost every item is `#[cfg(target_os = "none")]` | partly — only the declarations and re-exports |
| `src/mm/pframe.rs` | `Pmm` (the zone set) and `Buddy` (one zone's buddy system): `alloc`/`free`, per-frame reference counts for copy-on-write, `MAX_ORDER`/`MAX_ZONES`/`metadata_bytes`, and the double-free panic | yes — `#[cfg(all(test, not(target_os = "none")))] mod tests` |
| `src/mm/heap.rs` | `KernelHeap`, the `GlobalAlloc` behind the `#[global_allocator]`: `Slab` size classes, `Large` over-sized/allocation bookkeeping (`MAX_LARGE`), and `HeapStats` | **no** — `#[cfg(target_os = "none")]`, and no host tests |
| `src/mm/vm.rs` | An 18-line re-export of the `mpos-vm` leaf: `Mapping`, `MapFlags`, `PageTaken`, `KernelSections`, `PAGE_SIZE`/`PAGE_SHIFT`, `align_up`/`align_down`, `kernel_mappings` | yes (the leaf is a `crates/` leaf) |
| `src/mm/vma.rs` | `Vma`, `VmaKind`, `VmaTable`, `MappedObject`, `MappedRange`/`merge_ranges`: the sorted-`Vec` address-space descriptor table and its map/unmap/protect/split/merge/gap arithmetic | yes — `mod tests` |
| `src/mm/elf.rs` | `Image`/`Segment`, `parse`, `extract_rela`, `USER_LIMIT` and `MMAP_BASE` (per machine): the ELF64 loader's decisions — what is accepted, where each piece lands, what the caller must still place | yes — `mod tests` |
| `src/mm/sysv_shm.rs` | System V shared memory: the segment table (slot + generation numbers), `Segment` as a `MappedObject` whose frames arrive on first touch, the `IPC_RMID`-marks lifetime, and this kernel's `SHMMAX`/`SHMALL`/`SHMMNI` bounds | the arithmetic yes (`mod tests`); the frames and the `MappedObject` impl are `#[cfg(target_os = "none")]` |
| `src/mm/ipc_id.rs` | The number a System V IPC object is known by: `build`/`decode`/`next_generation` over `slot + generation * 32768`, shared by the three families because a retained number must not name the slot's next occupant | yes — `mod tests` |
| `crates/vm/src/lib.rs` | The actual mapping-plan vocabulary and page arithmetic, and the tests that pin the plan (they moved out with the vocabulary rather than staying to test a re-export) | yes |

## Contracts

Everything below is a symbol other modules actually call. The *edges* — who is allowed to name
`mm` — are `docs/ARCHITECTURE.md` §2/§3 plus its measured-edge tables in §6 and §9; this page does
not copy them. What is worth stating here is the invariant each entry point keeps.

**Frame allocation (`src/mm/mod.rs`, `src/mm/pframe.rs`).** `alloc_frames(order)`/`free_frames`
are the machine-facing API over the `PMM` static; `alloc_pages`/`free_pages` are the same two at the
kernel's direct-map address. The invariants: an allocation is `2^order` contiguous frames aligned to
its own size; an order-0 allocation starts with a reference count of one; `free_frames` refuses to
release anything inside a `pin_frames` range (a device's DMA region the kernel cannot reclaim), and
otherwise drops an order-0 page's reference count and returns it to the buddy only when no owner
remains. `clone_frame_ref`/`frame_refcount`/`free_frame_ref` are the copy-on-write surface `fork` and
the fault path use. `Buddy::free` panics, in the shipping build, on a double free — the one place the
allocator chooses to be loud rather than quiet, because a double free corrupts unrelated memory later
and the diagnosis is impossible by then.

**The global allocator (`src/mm/heap.rs`).** `KernelHeap` implements `GlobalAlloc` over a
`Slab`. The invariant worth naming is the alignment one: a run starts page-aligned and every slot is
a power-of-two multiple of the class size from it, so a slot is always aligned to at least its own
size, and only a request with alignment *larger than a page* needs the `Large` side table at all.
Runs are never returned to the frame allocator (waste is bounded by each class's high-water mark).
The `PMM` still panics on failure — `docs/notes/` §8.0 — which is why the heap carries a bounded
`MAX_LARGE` table rather than an unbounded map.

**The mapping plan (`src/mm/vm.rs` → `mpos-vm`).** `kernel_mappings` produces disjoint page-granular
`Mapping`s: the direct map over *all* physical memory, then the kernel image with per-section
permissions, so nothing is both writable and executable. `MapFlags`'s `PERM` distinguishes real page
permissions from `COW`, a logical state the hardware enforces as read-only. `PageTaken` reports an
already-mapped page as an ordinary syscall answer, where the kernel's own plan asserts on it.

**The address-space table (`src/mm/vma.rs`).** `VmaTable` is a sorted `Vec` and never overlapping;
`insert` has `MAP_FIXED`'s take-the-range semantics and folds neighbours that describe the same
object, `insert_present` appends a *placed* run without disturbing a lazily-described neighbour,
`gaps(start, end)` reports the holes a growth may claim, and `take_range`/`lift_range` distinguish
"descriptions leaving *and* freeing their pages" from "descriptions leaving, pages travelling with
them". `Vma::abuts` is the merge predicate. `MappedRange` records eagerly-filled ranges (`execve`, a
private mapping of a file) *beside* the fault-answering table, because recording them as
`VmaKind::Anon` would invent a fault where the address space must refuse one.

**The loader (`src/mm/elf.rs`).** `parse(bytes) -> Option<Image>` accepts a statically linked
`ET_EXEC` or `ET_DYN` whose `PT_LOAD` segments fit the lower half, reports the interpreter a
dynamically linked image asks for, and extracts only the one relative-relocation kind the file's
`e_machine` names. `Image.pie` is the loader's whole basis for moving a program; `Image.machine` is
reported for the caller (`binfmt`/`execve`) to reject a file built for the other ISA. `USER_LIMIT`
and `MMAP_BASE` are per-machine constants with their divergence written down in the file — the
aarch64 `USER_LIMIT` is `1 << 48`, not x86_64's `1 << 47`, and a shared value was measured *wrong*
(it made `MAP_FIXED` mappings of a shared library fail).

## What userspace sees

`mm` owns **no dispatch row** of its own in `docs/uapi/`. The surface it participates in — `mmap`,
`munmap`, `mprotect`, `madvise`, `mremap`, `brk`, `process_vm_readv`/`writev` — is dispatched in
`src/syscall/linux/proc.rs`, and the `MAP_*`/`MREMAP_*` flag words live in `src/syscall/linux/abi.rs`;
the inventory's rows are the syscall module's to keep (docs/uapi/ §1, the "`mmap`
flags" row in §3, and the `madvise` advice vocabulary in §3). What `mm` supplies is the *semantics*
those rows promise: which range a mapping may claim, what a split does to a frame-backed offset, and
where a `MAP_FIXED`/hint/`MAP_FIXED_NOREPLACE` placement lands. The one piece `mm` declares itself
is `USER_LIMIT`/`MMAP_BASE`; it lives here rather than in the syscall layer because `sched` had to
name `crate::syscall::MMAP_BASE` to place an image, a recorded `sched → syscall` edge blocking the
aarch64 un-gate (`elf.rs` has the reason).

## Pins

| Pin | Kind | What it would fail on |
|---|---|---|
| `src/mm/kunit.rs` `const SUITE` — `frames`, `large_block`, `heap` | boot (`--features selftest`) | The allocators on a real machine: zones exist, a block survives the direct map, the buddy merge re-hands the same block back, the largest block a zone gives out is usable end to end, every slab class returns every byte, and the heap and frame allocator agree about what they have handed out. Runs at `Phase::Heap`; the boot suite's shape is `docs/notes/` §15.20 |
| `src/mm/pframe.rs` `mod tests` — `seeding_accounts_for_every_frame`, `allocations_are_disjoint`, `exhausting_a_zone_then_refilling_it_loses_nothing`, `freed_buddies_merge_back_into_a_larger_block`, `merging_cascades_all_the_way_up`, `stale_list_entries_are_skipped_not_handed_out`, `orders_above_the_maximum_are_refused`, `a_region_smaller_than_its_metadata_is_rejected`, `the_pmm_only_takes_allocatable_regions` | host | The buddy allocator's disjointness, alignment, merge-cascade accounting and zone selection |
| `src/mm/elf.rs` `mod tests` — `a_static_executable_parses_into_its_load_segments`, `the_executable_bit_and_the_write_bit_come_out_of_the_header`, `segments_that_do_not_load_are_skipped`, `refuse_what_the_loader_cannot_place`, `a_segment_past_the_lower_half_is_refused`, `only_a_position_independent_image_moved_from_its_named_addresses`, `a_static_image_names_no_interpreter`, `a_dynamic_image_reports_the_interpreter_it_asks_for`, `the_relative_relocations_are_the_ones_the_loader_reads`, `a_relocation_target_outside_every_segment_refuses_the_image`, `a_rela_table_of_an_unknown_record_size_is_left_alone`, `a_rela_table_outside_every_segment_is_left_alone`, `an_empty_interpreter_path_is_not_an_interpreter`, `an_x86_64_image_reports_its_machine`, `an_aarch64_image_gets_its_relative_relocations_read`, `an_image_for_an_unknown_machine_is_refused`, `a_file_that_names_no_machine_is_refused` | host | What the loader accepts/refuses, the `ET_EXEC` vs `ET_DYN` placement rule, and per-machine relocation extraction |
| `src/mm/vma.rs` `mod tests` — `lookup_finds_the_covering_area_and_nothing_else`, `a_later_insert_replaces_exactly_its_own_range`, `adjacent_areas_of_one_object_merge_and_dissimilar_ones_do_not`, `one_object_is_recognised_across_two_handles_to_it`, `a_split_keeps_the_frame_offset_with_its_address`, `a_shared_bit_the_two_halves_do_not_agree_on_is_two_areas`, `a_name_splits_an_area_and_a_shared_one_folds_it_back`, `a_split_keeps_the_name_on_both_halves`, `remove_range_leaves_the_ends_and_takes_the_middle`, `take_range_hands_back_what_it_removed`, `set_prot_touches_only_what_is_already_there`, `set_prot_splits_so_the_untouched_part_keeps_its_bits`, `gaps_report_what_growth_may_claim`, `gaps_are_one_hole_per_hole_whatever_order_the_table_holds`, `mmap_placement_rules_over_a_lazy_arena` | host | The merge/split/gap semantics, the object-identity rule, and the `mmap` placement replay |
| `crates/vm/src/lib.rs` `mod tests` — `every_mapping_is_page_granular`, `nothing_is_both_writable_and_executable`, `no_two_entries_cover_the_same_page`, `the_kernel_image_is_mapped_with_per_section_permissions`, `the_direct_map_covers_all_physical_memory` | host | `kernel_mappings`: granularity, W^X, disjointness and direct-map coverage |
| `user/tests/src/bin/t-mmap.rs`, `t-mmap-demand.rs`, `t-mmap-reserve-commit.rs`, `t-mmap-shared-fork.rs`, `t-munmap.rs`, `t-mprotect.rs`, `t-mremap.rs`, `t-madvise.rs`, `t-maps.rs`, `t-brk.rs`, `t-memfd-shm.rs`, `t-lazy-buffer.rs`, `t-process-vm-libc.rs`, `t-fork.rs` | `t-*` (std) | The mapping/placement/lazy-fault/COW behaviour end to end on a booted machine |
| `user/tests/src/bin/t-aslr.rs` | `t-*` (std) | Which layout landmarks are randomised (`docs/notes/` §8.1; the heap-anchor half is a named gap) |

There is **no `t-*` program under `user/abitest` that targets `mm`** today: the `no_std` suite's
live programs are `src/bin/integration/t-*.rs`, and none of them exercises the allocator, the VMA
table or the ELF parser directly — the memory family is pinned from the std side above. And the
heap specifically has no pin of either kind: its arithmetic is machine-gated and nothing checks it
on the host.

## Evidence and open work

The reasoning and the measurements are elsewhere and are not restated here. The frame allocator's
statics and the heap's worst-case rule: `docs/notes/` §8.0. The VMA/`mmap` placement lesson (one
hole reported twice from an unsorted table) and the `insert_present` measurement: `docs/notes/`
§15.18. The `mm`↔`vfs` seam and `MappedObject`: `docs/ARCHITECTURE.md` §9 (V-13) and
`docs/notes/` §11.4. The mapping vocabulary's move to the `mpos-vm` leaf: `docs/notes/` §14
(step S2b). The `/proc/<pid>/maps` gap that `MappedRange` carries rather than hides: `docs/notes/`
§7.4. The aarch64 `USER_LIMIT`/`MMAP_BASE` divergence and the `dlopen` failure it caused:
`docs/notes/` §9.1. Feature status (`mmap`, `madvise`, shared anonymous memory) is on the board in
`docs/ROADMAP.md`, and the userspace verdicts are `docs/uapi/`'s rows. The remaining work this page
points at — without inventing it — is the heap's host testability relative to the rule, and the
`brk`-anchor randomisation gap `t-aslr` names (which is `sched`'s layout, not a wrong number here).
