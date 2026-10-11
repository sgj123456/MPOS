# §5 Phase 5: Memory Management Enhancements (Medium Priority)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 5: Memory Management Enhancements (Medium Priority)

### 5.1 Shared Anonymous mmap — **DONE**

`mmap(MAP_SHARED | MAP_ANONYMOUS)` is a *shared object*, not a private range:
the mapping is a frame-backed VMA over a fresh unnamed `Memfd` - the same frame
store `memfd_create` hands out, with real pages grown on demand, refcounted and
released when the last mapping of it goes. Nobody holds a descriptor for it and
nobody needs to: the mapping's VMA is its only name, and `fork` copies that.

That is what makes the aliasing survive a plain `fork`. Both processes keep the
same object, and the one rule that would have broken it is stated where the walk
is: **`fork` does not mark a frame-backed range's pages copy-on-write**, and it
does not harden them in the parent either. A copy-on-write flag there would turn
the first store on either side into a private frame, which is the sharing the
mechanism exists to keep. The `memfd` and dma-buf mappings get the same rule, so
"aliased frames don't cross a plain `fork`" is no longer a documented divergence
anywhere. `t-mmap-shared-fork` pins both directions - the child's write read by
the parent after a `wait4`, and the parent's write after the `fork` read by a
child the pipe held back - because only the second fails for a mapping that was
copied before the fork.

### 5.2 madvise — **DONE**

`madvise` answers `EINVAL` for a range whose start is not page-aligned (Linux's
rule) and recognises the `MADV_*` advisories, of which one now does something:
`MADV_DONTNEED` walks the range and drops the pages a VMA covers, leaving the
mapping in place so the next access is an ordinary fault. What that fault
produces is what the backing decides, which is Linux's rule in Linux's words -
"repopulating the memory contents from ... the underlying mapped file ... or
zero-fill-on-demand pages for anonymous private mappings". A private anonymous
page comes back freshly zeroed (its frame is gone, so a new one is allocated);
a page of a frame-backed range comes back from the object behind it, which still
holds it. `t-madvise` pins both, and `t-mmap-demand` shows the release and the
refill across a 64 MiB arena.

The other advisories stay the recognised no-ops they are, and two of them are
deliberate rather than unfinished:

* **`MADV_FREE`** keeps its no-op. Its contract is that the pages *may* be
  reclaimed but their contents are still there until memory pressure arrives,
  and this kernel has no pressure model - returning the frames immediately would
  be wrong rather than merely different.
* **`MADV_DONTNEED` on a shared mapping** does the per-mapping half of what Linux
  does. Linux drops the pages for everyone mapped to the object, which needs the
  object's reverse map of mappings (`i_mmap`) to find them; this kernel's object
  has no such index, and dropping the frames while another mapping still points
  at them would be a use-after-free rather than a saving. So the pages here are
  dropped for the caller's mapping and the object keeps its own copy, which the
  next fault finds - the "repopulate from the underlying mapped file" half of
  the promise, which is the half that is observable without the index. Nothing
  beyond that is claimed; `t-madvise` pins the single-process case and
  `t-mmap-shared-fork` the fork case.

### 5.3 Process_vm_readv / Process_vm_writev — **DONE**

`process_vm_readv(310)` and `process_vm_writev(311)` are implemented in
`proc.rs` (`handle_process_vm_readv`/`_writev`), walking two iovec arrays in
step to move bytes between the caller and a remote task's address space.
Used by debuggers and checkpoint/restore tools.

---

