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

### 5.5 `mseal(2)` — a span that may not be unmapped, resized or reprotected — **DONE** (2026-10-28)

`mseal(2)` is the call that lets a program promise its own address space will
not change: seal the text, seal a JIT's code, seal a framebuffer nobody may
unmap. This kernel had no arm for it (the number `462` was unserved), which is
one of the cheap shapes to add *if* the answers are Linux's - and the two edge
answers are not the ones a reader guesses.

**Measured first**, on Linux 7.2.11 (`build/scratch/mseal/probe.c`, `probe4.c`,
`probe5.c`, `probe6.c`, `probe7.c`):

| question | answer |
|---|---|
| `mseal(addr, len, 0)` on a mapped span | `0` |
| sealing an already-sealed span | `0` (idempotent) |
| `flags = 1` | `EINVAL` |
| unaligned start | `EINVAL` |
| `len = 0` | `0`, seals nothing |
| unmapped address, or `start = NULL` | `ENOMEM` |
| a hole anywhere inside the span | `ENOMEM` for the whole call |
| after a seal: `munmap` overlapping it | `EPERM` |
| after a seal: `mprotect` of it | `EPERM` |
| after a seal: `mremap` in place / `MREMAP_MAYMOVE` | `EPERM` |
| after a seal: `MAP_FIXED` over a sealed page | `EPERM` (the mapping is not placed) |
| a `munmap` that only *abuts* a sealed page | `0` - sealing page 0 of 4 leaves pages 1..4 free |
| `madvise(DONTNEED)` on sealed **writable anon** | `0` |
| `madvise(DONTNEED)`/`madvise(FREE)` on sealed **read-only anon** | `EPERM` |
| `madvise(DONTNEED)` on a sealed file-backed private mapping | `0` |
| `fork` | the seal is inherited |
| `exec` | the seal is gone with the address space |

The `.text` case is why the file-backed row matters: a program seals its image
(a file-backed private mapping, which in this kernel is an *eager* range with no
VMA at all) and the seal has to land there, not only on `mmap`-described ranges.
That is why the seal is a bit on **both** descriptions - `Vma.sealed` and
`MappedRange.sealed` - rather than a side table: the two descriptions are what a
mapping can be, and a seal that lived in one of them would not cover the image.

**The refusal is on the whole call**, not the sealed slice of it, and the entry
points that ask are the scheduler's mapping operations, not the handler:
`munmap_user`, `protect_user_range`, `mremap`'s source guard and `madvise`'s
destructive advice all answer `EPERM` before changing anything - which is what
makes the operation atomic from the caller's side. `seal_range` splits the VMA at
the seal's ends (so the unsealed neighbours stay usable), refuses a span with a
hole (`ENOMEM`, before splitting anything), and `abuts` compares the seal so a
sealed piece never folds back into an unsealed neighbour. `madvise`'s rule is the
man page's sentence, reconstructed from the measurements: a destructive advice is
refused only where the pages are anonymous **and** not writable, because a
sealed writable page and a file-backed page both have an answer for what comes
back.

**Pinned** by `t-mseal` (new, both machines, 27 claims) - and because `mseal` is
unprivileged, the same binary ran against the host first: **27/27 ok, `PASS`,
exit 0**. That is the counterfactual direction: every claim in the program is
Linux's answer, not this kernel's expectation.

**Gates** - `cargo xtask test` after the pin: x86_64 `pass=225 fail=0 (225
built)`, aarch64 `pass=224 fail=0 (224 built)`, `mpos-kunit 34 passed, 0 failed`
both, host suite **682 passed**. The counterfactual - the dispatch arm removed -
answers `CF-EXIT=1`, `pass=224 fail=1 (225 built)`, and only `t-mseal` red (every
call falls to `_ => ENOSYS`, so the program panics at its first claim).

One divergence is named rather than served: a sealed range cannot be *grown* by
a stack fault (this kernel's stack grows by extending its eager `[stack]` range,
and a sealed stack makes that growth fail, which is Linux's `SIGSEGV` answer);
the guard is in `resolve_stack_growth` and is measured by the pin only indirectly
(the pin seals heap mappings, not the stack).

Full record: [`docs/uapi/`](../uapi/05-work-list.md) §5.1 item 118.

### 5.6 `mmap` of a node with no pages is `ENODEV` — **DONE** (2026-11-13)

A user reported `mkfontscale` dying in the guest. The kernel's own fault report
named the whole story in three lines:

```
PFDIAG page=0x7e2fced3a000 NOT MAPPED task=11 rip=0x7e2fcf1ae141 mapped=none …
PFDIAG vma=0x9 (r--) kind=object(private) - the page is inside a mapping, so nothing brought it in
PFDIAG fault_in=vma[/root/.local] 0x7e2fced3a000..0x7e2fced3b000 r--u kind=object(private)
```

`mkfontscale` walks the entries of the directory it is asked to scan and maps the
ones that open. `./.local` is a **directory**; the guest's own `strace`, read off
the image's `/root` partition, shows the call that should have refused and did
not:

```
open("./.local", O_RDONLY|O_LARGEFILE) = 4
fstat(4, {st_mode=S_IFDIR|0700, st_size=4096, ...}) = 0
mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, 4, 0) = 0x7e2f2ccc1000
close(4) = 0
+++ killed by SIGSEGV (core dumped) +++
```

The mapping was accepted as a private file mapping, `FilePages::frame_at` asked
the directory for its bytes, `read_at` answered `EISDIR`, the frame was refused,
and the first touch of the page killed the task. The program had already been
told the call succeeded - **nearly-right is worse than absent**, in its exact
form: an errno at the call would have been freetype's own "not a font" path;
instead the process died inside a mapping it believed in.

**Measured on Linux 7.2.14 first** (`build/scratch/mmapdir/{p,q,r}.c`):

| node | `mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0)` |
|---|---|
| `/root`, `/tmp` (mode `040750`/`041777`) | `-1 ENODEV` |
| a FIFO (mode `010600`), a socket (mode `140777`) | `-1 ENODEV` |
| `/dev/null` (mode `020666`) | `-1 ENODEV` |
| `/proc/self/stat`, `/proc/self/maps`, `/sys/kernel/notes` | `-1 ENODEV` |
| `/etc/hostname` (mode `100644`) | maps; first byte `97` |
| `/dev/zero` | maps - it has its own mmap operation (what it maps is anonymous memory: §5.7) |

Order of the refusals, same probes: `len 0` → `EINVAL`; `offset 1` → `EINVAL`;
`fd = -1` → `EBADF`; `PROT_NONE` → `ENODEV`; `MAP_SHARED` → `ENODEV`;
`MAP_FIXED` → `ENODEV`; a regular file with `offset 1` → `EINVAL`;
`MAP_SHARED|PROT_WRITE` of a read-only file → `EACCES`. So the node question is
asked **after** the length/offset/descriptor checks and **before** anything is
mapped.

**What the fix is.** A defaulted `Inode::mappable()` - Linux's `f_op->mmap`
question, phrased for this kernel: *are this node's bytes storage a mapping can
be filled from?* The default is **`false`**, and that direction is the point: a
node that forgets to answer is refused with the number Linux gives a node with no
mapping operation, where a default of `true` would let it accept a mapping its
faults cannot serve - the failure being a `SIGSEGV` on first touch rather than an
errno, which is the bug. Three nodes say yes because their bytes are storage:
`RamFile`, `Ext4Ino` (a regular file's blocks, and not a directory's - it answers
from the inode record's own kind) and `P9Inode` (the share's regular files). The
`frame_backed` arm is asked first and unchanged, so a memfd, a framebuffer and a
dma-buf still map; the door answers `err::ENODEV` for everything else in one arm,
placed before both copy arms (`src/syscall/linux/proc.rs`).

**Pinned** by `t-mmap-refuse` (new, both machines, 14 claims), written against
Linux first: `MPOS_ON_HOST=1` **14/14 ok, `PASS`, exit 0**. Guest: **14/14 ok,
`PASS`, `exit=0`** - the control (a regular file's bytes map and its first byte
is the file's), a directory `ENODEV` privates **and** shared, the `len 0` /
`offset 1` / `fd -1` ordering, a `procfs` file `ENODEV`, `/dev/null` `ENODEV`, a
pipe end `ENODEV`, and a memfd's pages still mapping and aliasing.

**Counterfactual** - the door's arm deleted, kernel and loader rebuilt: the guest
dies at the *control's next step*, which is the user's own report reproduced -
`t-mmap-refuse[10]: segfault at 0x7ef275045000 … task 5 died of SIGSEGV it could
not be handed … === t-mmap-refuse exit=139`, and it never reaches a claim about
the directory. With the arm restored (`diff -q` identical) the run is the green
one above.

**Gate** — `cargo xtask test`, `build/gate-mmaprefuse.log`, `GATE_EXIT=0`: stage 1 **1320**
host tests, 0 failed (51.24 s); x86_64 `verdict pass=313 fail=0`; aarch64 `pass=312 fail=0`
(312 built, 16 of 328 left out by `tests.toml`); `mpos-kunit 35 passed, 0 failed` both;
`Boot instrument: the <arch> kernel carries no hang-heartbeat table`; lint ratcheted at
162/168/389/273/3/1, both workspaces rustfmt's shape, the fuzz workspace 23 targets built;
`t-mmap-refuse` green in both arms (0 ms x86_64 - the whole run is refusals - 191 ms
aarch64).

Full record: [`docs/uapi/`](../uapi/05-work-list.md) §5.1 item 271.

### 5.7 `/dev/zero` maps, and what it maps is anonymous memory — **DONE** (2026-11-15)

`§5.6` left one name behind, in its own closing sentence and in four other
places that quoted it: "`/dev/zero` still answers `ENODEV` where Linux maps it
(it needs a zero-page mapping operation, named rather than faked)". That was
honest about the refusal and wrong about the prerequisite — there is no
zero-page mapping operation to write. What `/dev/zero` hands a mapping is
**fresh private anonymous memory**, and this kernel already has that: the arm
`MAP_ANONYMOUS` takes. The four other memory streams (`/dev/null`, `/dev/full`,
`/dev/random`, `/dev/urandom`) are the ones a mapping must refuse, and they were
already refused.

**Measured on Linux 7.2.14 first** (`build/scratch/zeromap/{z,w,v,mat,n}.c`):

| question | answer |
|---|---|
| `/dev/zero`, `MAP_PRIVATE` / `MAP_SHARED` | maps both ways |
| `/dev/null`, `/dev/full`, `/dev/random`, `/dev/urandom` | `-1 ENODEV` both ways |
| two `MAP_SHARED` mappings of one page | **not aliased** — `a[0]=0xabc` leaves `b[0]=0` |
| `MAP_PRIVATE` beside a shared one | independent |
| `offset 4096*1000`, `len 2^46` | accepted — no extent to run past |
| `len 0`, unaligned `offset` | `EINVAL`, before anything else |
| fd closed after mapping | the mapping survives; `fork` shares it |
| `maps`, private | `rw-p 00000000 00:09 6    /dev/zero` |
| `maps`, shared | `rw-s 00000000 00:01 <ino>  /dev/zero (deleted)` |
| smaps, private page | `Rss: 4 kB`, `Anonymous: 4 kB` |
| smaps, shared page | `Rss: 4 kB`, `Anonymous: 0 kB` |

So a shared `/dev/zero` mapping prints the *same shape* Linux prints for
`MAP_SHARED|MAP_ANONYMOUS` (`/dev/zero (deleted)` there, because Linux builds
that from a deleted tmpfs inode) — the two are the same memory with different
paperwork. The aliasing row is the one that decides the design: a mapping that
looked like a window on the device would alias, and a fixture that only read
zeroes could not tell the difference. `/dev/zero` is anonymous memory wearing a
device's name.

**The descriptor's access mode is part of the rule, and it does not exist in this
kernel at all** (`build/scratch/zeromap/mat.c`, `n.c`):

| fd mode | `MAP_PRIVATE` | `MAP_SHARED` |
|---|---|---|
| `O_RDONLY`, `PROT_READ` | ok | ok |
| `O_RDONLY`, `PROT_WRITE` or `PROT_READ\|PROT_WRITE` | **ok** (a private write is the process's own copy) | `-1 EACCES` |
| `O_WRONLY`, any prot incl. `PROT_NONE` | `-1 EACCES` | `-1 EACCES` |
| `O_RDWR` | ok | ok |

The same table holds for a read-only *file* (`/etc/hostname`), so it is not a
device rule. `O_WRONLY` is refused even for `PROT_NONE` and for a private
mapping: a mapping is a view of the file's bytes, and a write-only descriptor
has no view to give. Across all of it the order is fixed, measured by asking two
questions at once: `EINVAL` (length, offset) first, then the descriptor's access
mode, then the node's own answer — a directory asked `MAP_SHARED|PROT_WRITE` is
`EACCES` while the same directory asked `MAP_SHARED|PROT_READ` is `ENODEV`.

**What the fix is.** A third question beside `mappable()` and `frame_backed()`,
because `/dev/zero` answers neither: it has no frames to alias and its
`read_at` invents the bytes rather than serving storage. `Inode::maps_as_anonymous()`
defaults to `false`, `CharDevice` carries the same predicate (Linux asks the
driver through `f_op->mmap`), `DevNode` forwards its driver's answer, and
`ZeroDev` says `true`. The `mmap` door's arm sits immediately after the
`MAP_ANONYMOUS` arm and before the `frame_backed`/`ENODEV`/copy arms: private
routes to `map_anon_named` (new — the name cannot be inferred from
`VmaKind::Anon`, and Linux prints `/dev/zero` beside anonymous pages), shared
reuses the unnamed `Memfd` object the `MAP_SHARED|MAP_ANONYMOUS` arm already
builds. Deliberately **not** a new `VmaKind`: the memory is `Anon`, and a
variant would have touched 24 match sites to describe something this kernel
already models. The descriptor rule went into the backing resolution with
`err::EACCES` (which already existed) and a comment naming the measured order.

**Pinned** by `t-dev-zero-mmap` (new, both machines, 17 claims), written against
Linux first: `MPOS_ON_HOST=1` **17/17 ok, `PASS`, exit 0**. Guest: **17/17 ok,
`PASS`, `exit=0`**. The claims: `/dev/null` still `ENODEV` both ways (the half a
"let every char device map" fix would break), `/dev/zero` maps both ways and
reads zero and keeps a written byte, two shared mappings are separate memory, an
offset a thousand pages in is accepted, a write-only descriptor is `EACCES` for
`PROT_READ` / `PROT_READ|PROT_WRITE` / `PROT_NONE`, a read-only descriptor
refuses a shared write with `EACCES` **and serves the same write privately**, a
private mapping is named `/dev/zero` in `maps`, and the mapping outlives its
descriptor.

**Counterfactual** — the kernel half stashed (`git stash push` of the six
files), kernel and loader rebuilt: the two `/dev/null` refusals stay green and
**all fifteen `/dev/zero` claims go red**, every one of them answering errno 19
(`ENODEV`) — including the three `EACCES` rows, which is the reading that proves
the descriptor rule is new rather than an accident of the node question. Output:
`build/scratch/zeromap/guest-cf.log`, `=== t-dev-zero-mmap exit=1`. Restored
with `git stash pop` and `sha256sum -c build/scratch/zeromap/good/good.sha` (all
eight 成功), rebuilt, 17/17 green again.

**The first gate was red, on a pin that predates this item.** `t-fbdev` opened
`/dev/fb0` **`O_RDONLY`** and then mapped it `MAP_SHARED|PROT_WRITE`, which the
new descriptor rule refuses with `EACCES` — `FAIL: the framebuffer maps`, both
arms. The measurement that decided which side was wrong is the host's own card,
as root (`build/scratch/zeromap/fb.c`): `open RDONLY /dev/fb0 = 3`,
`RDONLY + SHARED|RW = EACCES`, `RDONLY + PRIVATE|RW = ok`. Linux refuses that
call too, so the *test* was asking for a mapping no kernel hands out; a
compositor draws through the mapping and therefore opens the card for writing.
`t-fbdev` now opens `O_RDWR`, with the measurement in its comment. That is the
"when the gate goes red, the code changes, not the test" rule being answered
rather than followed: the counter-evidence had to be Linux's own answer for the
test's own call.

**One divergence, recorded rather than faked.** The guest's `maps` line for a
private `/dev/zero` mapping reads `7e9cb4a84000-7e9cb4a85000 r--p 00000000 00:00 0
/dev/zero` where Linux reads `00:09 6`: the name is right, the device and inode
are `00:00 0` because this kernel's `stat` reports no filesystem for any node.
`/memfd: (deleted)` carries the same two zeroes for the same reason, so this is
one divergence with two spellings rather than a new one — and a fabricated inode
number in a field a program parses would be worse than the zero.

**What this does not do, named.**

1. `maps`'s device and inode fields are `00:00 0` for every mapping, `/dev/zero`
   included (above). Fixing it means a `stat` that reports a node's owning
   filesystem, which no node carries today.
2. A shared `/dev/zero` mapping prints `/memfd: (deleted)` where Linux prints
   `/dev/zero (deleted)`. Both name the same thing — an object with no
   filesystem — and this kernel's spelling is already documented for
   `MAP_SHARED|MAP_ANONYMOUS`; matching Linux's exactly would mean a mapping
   name derived from the *file* for a range that does not alias it.
3. 1 GiB of `/dev/zero` maps but still costs a frame per page on first touch —
   there is no shared zero page, so two processes touching the same offsets get
   two frames. Linux's kernel does the same for a *write*, and this kernel has no
   read-only zero page to hand out; a process that maps 1 GiB and touches all of
   it will OOM, as it would on Linux without overcommit.
4. `mmap` of a **pipe**'s or a **socket**'s node is `ENODEV` (`§5.6`), and
   `/dev/zero`'s new predicate does not change that — no memory stream but
   `zero` answers it.

**Gate** — `cargo xtask test`, `build/gate-devzero2.log`, `GATE_EXIT=0`: stage 1
**1331** host tests, 0 failed (29.56 s); x86_64 `verdict pass=322 fail=0`; aarch64
`pass=318 fail=0`; `mpos-kunit 41 passed, 0 failed` both; lint ratcheted at
162/168/389/273/3/1, both workspaces rustfmt's shape, the fuzz workspace's targets built;
`t-dev-zero-mmap` green in both arms.

Full record: [`docs/uapi/`](../uapi/05-work-list.md) item 285.
