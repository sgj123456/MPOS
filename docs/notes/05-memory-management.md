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

### 5.4 The occupancy question forgot a whole class of mapping — **DONE**, and it was killing the desktop

Every "is this address free?" question in this kernel funnels through one
function, `Mm::vma_gaps` (it is what `vma_range_free`, `mmap`'s hint check,
`MAP_FIXED_NOREPLACE`, `free_range`'s cursor search and `mremap`'s grow all ask).
It asked the **VMA table** and nothing else. But not every mapping has a VMA:
a `MAP_PRIVATE` mapping of an ordinary file is filled in *at `mmap` time* and
deliberately carries none — its pages came from the file and nothing may drop
them again, because there is no file behind them to refill from (that is what
`MappedRange` records them for). So those addresses were invisible to the one
question that decides whether an address may be handed out, and a **second
mapping asked for with such an address as its hint was placed straight on top of
the first**:

```
PFDIAG page=0x7e6692925000  P=1 W=0 U=1 B9=0  rsp=0x7e6692b30f40  below_sp=0x20bf40
       op=[4c,89,1e,…]                                  ← mov [rsi], r11
PFDIAG vma=0xb (rw-u) kind=anon
PFDIAG fault_in=0x7e6692925000 vma[unnamed] 0x7e6692925000..0x7e669292c000 rw-u kind=anon
                               + eager[unnamed] 0x7e6692850000..0x7e6692927000
PFDIAG rip_in=0x7e3198d0c421   eager[/lib/ld-musl-x86_64.so.1] 0x7e3198cf5000..0x7e3198d4e000
```

The page is described **twice**: by a fresh writable VMA and by the eager range
that had already filled it read-only. The PTE kept the first mapping's
permissions (present, user, **read-only**, no copy-on-write marker), so a write
to it had **no resolver at all** — `resolve_cow_fault` wants the software marker,
`resolve_anon_fault` fills only *absent* pages, `resolve_stack_growth` wants a
fault near `rsp` — and the task took SIGSEGV. Linux's rule differs in exactly
this place: a write fault on a read-only PTE of a *writable* VMA is `do_wp_page`
and always succeeds.

**How it was found, and why it looked like anything but a memory bug.** It
surfaced as a flood of `user #PF … PROTECTION_VIOLATION | CAUSED_BY_WRITE |
USER_MODE` while running weston (and, on the second machine, as
`ESR 0x9200004f` — `DFSC 0xF`, a level-3 *permission* fault, `WnR` set, which is
the same class), one fault per task, and **only on an image that had `niri`
installed**. The instrument is what closed it: the report prints what the
*process* asked for (`vma=`), the faulting instruction and the distance below
`rsp`, and the name of *both* mappings (`fault_in`/`rip_in`). That last line is
what turned "some anonymous region ~2 MiB below the stack pointer" into
`/lib/ld-musl-x86_64.so.1`, and the writer into musl's dynamic linker: `mov
[rsi], r11` is a relocated slot being written, on the **first page of a 7-page
anonymous mapping the linker had just mapped writable** — the writable half of a
shared object whose read-only pass had already filled that page. "Only with
`niri` installed" is the trigger being a *library layout*: the bug is latent,
and it takes a library whose two passes meet on one page.

Twenty shapes a loader can legitimately make (anonymous, `mprotect` both ways,
whole-arena `RW→RX→RW` flips, `fork` copy-on-write, `mremap`, memfd and ordinary
file mappings shared and private, `MAP_FIXED` over a live mapping, address reuse
after `munmap`, a file page shared by two segment-shaped mappings) were run in
the guest and **all behave as Linux does** — which is why the answer had to come
from the report and not from guessing at an ordinary path.

**The fix** is that the occupancy question reads **both** descriptions: a range
is free when no VMA *and* no eagerly-mapped range describes any part of it, which
is Linux's "no mapping here". Two consequences of making that list a real record
rather than a reader's aid were needed with it, and both are pinned:
`munmap` now drops the ranges it covers (`forget_ranges`) — otherwise an address
would stay occupied after its pages were gone and `/proc/<pid>/maps` would keep
printing it — and `mremap`'s move carries them to the destination
(`move_eager_ranges`). `mprotect` writes the new permissions into them too
(`reprot_ranges`), because for a VMA-less mapping that list *is* the record of
what the mapping is.

**The pin is `t-mmap-hint`**, and it is the counterfactual in both directions,
measured on the machine: with the fix reverted it prints
`FAIL: a hint naming a mapped address is not honoured` and then dies with the
`PFDIAG` above; with the fix it prints eleven `ok:` lines and `PASS`. Its claims
are all Linux's answers, checked on the host first: a hint naming a mapped
address is refused (the mapping lands elsewhere), the file's bytes survive, that
second mapping is writable and private, `mprotect` is what `/proc/self/maps`
reports, `munmap` really frees the address (the same hint is honoured then), and
`mremap` releases the address it left while keeping the one it moved to occupied.

**Measured after the fix**, on the same image and the same trigger that used to
flood: a weston + `weston-terminal` boot produces **0 faults and 0
`unexpected timerfd events`**, and the clients (`weston-keyboard`,
`weston-desktop-shell`, both terminals) are still running afterwards. The gate:
x86_64 `=== SUMMARY pass=170 fail=0` (one more than before — the new program),
`mpos-kunit 30 passed, 0 failed`, exit 33.

---
