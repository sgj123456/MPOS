# Chromium on MPOS: what actually blocks it (measured 2026-11-16)

This is an **analysis**, not a plan retrofitted to look tidy. Everything below was
measured on the machine this repo boots, by booting the installed disk image and
running Chromium inside it.

## 0. The starting fact, and why it reframes the question

**Chromium is already installed in the image and already starts.**

```
/usr/bin/chromium -> chromium-browser -> ../lib/chromium/chromium-launcher.sh
/usr/lib/chromium/chromium  296242984 bytes   (Alpine chromium 157.0.8103.50)
$ /usr/lib/chromium/chromium --version
Chromium 157.0.8103.50
```

So "support Chromium" is **not** a packaging question, and not a "make it exec"
question — item 272 (bounded `execve`), item 273 (load outside the table lock) and
item 274 (four-byte `getresuid`/`getresgid`) already closed that road, and this
analysis confirms the binary gets to `main`.

What remains is the gap between "the process starts" and "the browser renders a
page". This document measures where that gap actually is.

## 1. How it was measured (so the readings can be trusted)

- `xtask run` cannot be used from this sandbox: its privileged image rebuild needs
  `sudo`, which the container blocks (`sudo: 已设置"no new privileges"标志`).
- The **installed image** was booted directly as a virtio disk with OVMF, which is
  the same image `run` boots.
- The command was injected as `/etc/local.d/zz-bootcmd.start` — a hook the image's
  own OpenRC `local` service executes — and its output read from the serial log.
- **One trap cost several boots and is worth recording**: OpenRC with
  `rc_verbose=no` redirects a `local.d` script's output to `/dev/null`, so a hook
  that *runs* prints nothing. `/etc/rc.conf` had to be changed to
  `rc_verbose=yes` before any reading was trustworthy. A silent hook is not an
  unexecuted hook.
- `virtio-gpu-gl-pci` is unusable here: this sandbox has **no DRM render node**, so
  QEMU refuses with `egl: no drm render node available`. The readings below are on
  `virtio-gpu-pci` (2D) with `--headless --disable-gpu`, which is the right fixture
  for a *startup* question: it takes the GPU out of the picture deliberately.

## 2. What the boot produced (the raw failure)

```
Chromium 157.0.8103.50                          <- --version works
inotify_reader[719]: segfault at 7e269cad419b ip 00007e269cad419b
   sp 000000003fa9de78 error 14 in /usr/lib/libdbus-1.so.3[1119b,7e269cac3000+25000]
PFDIAG vma=0xd (r-xu) kind=object(private)
PFDIAG fault_in=0x7e269cad4000 vma[/usr/lib/libdbus-1.so.3] ... r-xu
PFDIAG L0 entry=0x3fba6025 P=1 W=0 U=1
  trap 3 code 0x0 rip ...        <- #BP, musl's a_crash()
traps: chromium[726] breakpoint ip:... error:0
[crashpad] ptrace: I/O error (5)   x12
[crashpad] thread ID 719 not found in process
Terminated                             <- the 150 s budget, not a crash
```

Two distinct things are visible and they must not be conflated.

## 3. Finding A — the `libdbus` fault and the `trap 3` are **Chromium's own CHECK**, and the kernel is right

The first reading of this run produced two alarming lines and both turned out to be
**correct kernel behaviour**, which is worth recording because the wrong reading was
written down here first and had to be overturned by measurement.

```
inotify_reader[719]: segfault at 7e269cad419b ip 00007e269cad419b
   sp 000000003fa9de78 error 14 in /usr/lib/libdbus-1.so.3[1119b,...]
  trap 3 code 0x0 rip 0x7d5568b88127 rsp 0x3fdafbc0 rflags 0x246
  bytes at rip: [0f, 0b, cc, 0f, 0b, cc, cc, cc, cc, 55, 48, 89, e5, ...]
```

### `error 14` is not a reserved-bit violation

The Intel decode reads `14 = 0b1110` as `P=0, W=1, U=1, RSVD=1`, and the obvious
reading — "the kernel set a reserved bit in a page table" — is **wrong**. On x86 the
W bit is *undefined* when `P=0` (the SDM only defines it for protection violations),
and this kernel's own note (`src/sched/runtime/child_exit.rs`) records the measured
shape: **`error 14` is an instruction fetch from a not-present page.** QEMU sets the
bits that are undefined rather than leaving them clear.

Measured on a Linux host for comparison (`build/scratch/pfcode/p.c`): a write to a
present `r-x` page, a present `r--` page and a `PROT_NONE` page all report `error 6`;
a never-mapped address reports `error 6` with `si_code` `SEGV_MAPERR`. So `14` is
this machine's spelling of "not present", not a distinct fault class.

### A never-touched executable page **does** populate

The follow-on hypothesis — that the demand path refuses to fill an executable page
because the fault carries `W=1` and `Vma::permits(true)` is false for `r-x` — was
**tested and refuted** (`probe-fetchfault`, a 256 KiB `r-x` private mapping of
`/lib/ld-musl-x86_64.so.1` with a fetch into a page never touched):

```
  mapped 262144 bytes ... r-x
  data read of last page ok (255)
  about to fetch from a mid page ...
  trap 6 ... invalid opcode          <- the page WAS populated; the bytes were junk
```

The fetch reached the page's contents and died of whatever bytes were there. So the
demand path serves executable pages correctly, and finding A's "the kernel mishandled
this" was wrong.

### `trap 3` with `bytes = [0f, 0b, …]` is self-consistent

`#BP`'s saved `rip` points **after** the `int3` — the kernel's own comment says so —
so a byte dump from that `rip` shows the *next* instruction. Verified directly with a
purpose-built page (`probe-ripafter`, bytes `cc 0f 0b cc 0f 0b cc cc`):

```
  trap 3 code 0x0 rip 0x7e5615c9c001        <- the int3 was at ...c000
  bytes at rip: [0f, 0b, cc, 0f, 0b, cc, cc, ...]
  child signal=5 (SIGTRAP)
```

So Chromium's `int3` followed by `ud2` is exactly what produces the line observed, and
`trap 3` really is its `IMMEDIATE_CRASH()` — Chromium's own `CHECK`/`LOG(FATAL)` path,
not musl's `a_crash()` (which is `ud2` → `trap 6`, verified separately: `ud2` →
`SIGILL(4)`, `int3` → `SIGTRAP(5)`, both correct).

**Conclusion: the process did not die of a kernel fault.** It logged a dbus failure
storm (`Failed to connect to the bus: Could not parse server address` — the image's
`DBUS_SESSION_BUS_ADDRESS` is unset/empty), then took its own CHECK and trapped. That
is a **userspace/environment** problem, not a syscall gap.

## 3b. Finding A2 — a second, **different** fault: a present page with `U=0`

With `DBUS_SESSION_BUS_ADDRESS` set, Chromium got further and produced a fault that
is a genuine kernel anomaly. It is recorded here as **open** — located precisely,
not yet explained — because the analysis is worth more than a guess.

```
CacheThread_Blo[725]: segfault at 7e820912c000 ip 7e6ecc892b40 error 5
   in /lib/ld-musl-x86_64.so.1
PFDIAG page=0x7e820912c000 pte=0x800000003acfd003  P=1 W=1 U=0
PFDIAG vma=NONE (no mapping describes 0x7e820912c000)
PFDIAG fault_in=0x7e820912c000 eager[/tmp/cp3/Default/Web Data]
PFDIAG rip_in=0x7e6ecc892b40 eager[/lib/ld-musl-x86_64.so.1]
   op=[f3, 0f, 6f, 06, ...]        <- `movdqu xmm0,[rsi]`, a 16-byte SSE load
```

What it says:

- `error 5 = 0b101` → `P=1` (present), `U=1` (user), **`W=0`** — a user **read** of a
  page that **is** present;
- the PTE is `0x800000003acfd003`: `NX=1, P=1, W=1, U=0`. The page is present and
  writable but **supervisor-only**, so a ring-3 read faults. NX on a read is
  irrelevant.
- `rip_in` is a **16-byte SSE load inside `ld-musl`'s `memcpy`**, and the address
  being read is one the process believes it owns — it is reading out of its own
  `/tmp/cp3/Default/Web Data`.

Compare with the *first* fault of the same run, whose PTE was
`0x3fba6025` = `P=1 W=0 U=1` — **`U=1`, correct**. So the two faults are not the same
defect, and only the second has the supervisor-only bit.

**What was ruled out, by measurement:**

- ordinary `MAP_PRIVATE|MAP_ANONYMOUS` and private file mappings both get `U=1`
  (`probe-pte`: an anonymous page and a file mapping of `/bin/busybox` are written and
  read from ring 3 without faulting);
- `page_flags` does translate `MapFlags::USER` → `USER_ACCESSIBLE`
  (`arch/src/arch/x86_64/paging.rs`), so the translation is not missing;
- `munmap_user` does clear the page tables (`unmap_user`) and does forget eager
  ranges (`forget_ranges`), so a plain unmap is not leaving a stale supervisor entry;
- `PFDIAG vma=NONE` is a limitation of the **diagnostic** (it consults the VMA table
  and not the eager-range table), not necessarily of the kernel: the eager line
  printed immediately after names a range covering the address.

**Also ruled out by measurement** (`probe-mprot`): `mprotect` keeps the User bit.
An 8 KiB mapping was read, write-protected (`PROT_READ`), read again, set to
`PROT_NONE`, restored to `PROT_READ|PROT_WRITE` and read a third time — every read
succeeded. So a `mprotect` that drops `PROT_WRITE` does **not** leave the leaf
supervisor-only on this kernel.

**What is left to check**, in order, and this is the honest handover:

1. `protect` and `map_shared` take different flag paths from `try_map`
   (`arch/src/arch/x86_64/paging.rs`: `protect` walks leaves by address, which is
   what an eager range needs and what nothing above exercises with `USER`);
2. whether the `run=[0x7e820912c000..0x7e820913a000)` in the PFDIAG line spans more
   than one page — if the *first* page of a run gets the correct flags and a later
   one does not, the run-scatter loop (`Cache::run_places`) is the suspect;
3. whether `mremap` of an eager range (`move_eager_ranges`) rewrites the leaves at
   the new address with the stored flags or with a default.

Three hypotheses have been tested and refuted (fetch-demand, plain `mprotect`, plain
`munmap`); the remaining ones all involve a path that **rewrites an existing leaf**,
which is where a missing `USER_ACCESSIBLE` would survive unnoticed by every other
test here — no other program on this machine remaps 40 KB of eager pages per
allocation the way Chromium's allocator does.

**This is a real bug and it is now cheap to find**: the PFDIAG line already names the
address, the PTE, the range and the instruction. What it needs is the third
hypothesis tested with one boot, not more reading.

## 4. Finding B — `PTRACE_SEIZE`/`PTRACE_INTERRUPT` are missing, and crashpad needs them

This is the real, nameable gap in the trace:

```
[crashpad] ptrace: I/O error (5)   x12
```

`EIO` from `ptrace` is Linux's own answer for **a request the kernel does not know**
(`src/syscall/linux/proc.rs`, the `_ => err::EIO` arm), and crashpad probes with it.
What this kernel serves, and what crashpad asks for:

| request | number | this kernel |
|---|---|---|
| `PTRACE_TRACEME` | 0 | served |
| `PTRACE_ATTACH` | 16 | served |
| `PTRACE_GETREGSET` | 0x4204 | served |
| `PTRACE_GETSIGINFO` | 0x4202 | served |
| `PTRACE_CONT` | 7 | served |
| **`PTRACE_SEIZE`** | **0x4206** | **not defined → `EIO`** |
| **`PTRACE_INTERRUPT`** | **0x4207** | **not defined → `EIO`** |
| `PTRACE_LISTEN` | 0x4208 | not defined → `EIO` |

Crashpad is Chromium's crash handler; since Linux 3.4 it prefers the
`SEIZE`/`INTERRUPT` pair over `ATTACH`, because `SEIZE` attaches **without** stopping
the tracee and `INTERRUPT` then asks for a stop explicitly. Every probe of it here
gets `EIO`, crashpad concludes it cannot inspect the crashing process, and
`thread ID 719 not found in process` follows.

**Why this is the blocker it looks like**: Chromium treats its crash handler as
load-bearing. A browser whose crashpad cannot attach is not required to abort, but it
logs a storm and loses its crash reporting; and on a kernel where the *renderer*
crashes (which is likely during bring-up) the crashpad failure is what turns a
recoverable renderer death into an unexplained hang.

**Scope of the fix**: `PTRACE_SEIZE` is `ATTACH` minus the group-stop, plus the
`PTRACE_O_*` options word; `PTRACE_INTERRUPT` is "stop a seized tracee now". The
scheduler already has the attach link, the stop state and the `wait` event path that
`ATTACH` uses — so this is a **shaped extension of an existing mechanism**, not a new
subsystem. That is why it is the recommended first item.

## 4b. Finding C — `/proc/<pid>/maps` reports **file offset 0 and dev/inode 0 for every segment**, and that is a bug

Two crashpad lines come *before* the ptrace storm and are a **separate** defect:

```
[crashpad] Incorrect ELF magic number
[crashpad] no module mappings 0x7d5404541000
```

The first hypothesis — that a path was wrong or the header read returned bad bytes —
was **measured and refuted** inside the guest:

```
exe -> /usr/lib/chromium/chromium
 177   E   L   F          <- reading the file directly gives the right magic
dbus path = /usr/lib/libdbus-1.so.3
 177   E   L   F
```

The paths from `/proc/<pid>/maps` are correct and the files read correctly. The
defect is in the **maps columns themselves**:

```
this kernel:
7e2fe9a47000-7e2feb8ac000 r--p 00000000 00:00 0   /usr/lib/chromium/chromium
7e2feb8ac000-7e2ffa6e5000 r-xp 00000000 00:00 0   /usr/lib/chromium/chromium
7e2ffa6e5000-7e2ffb3ce000 rw-p 00000000 00:00 0   /usr/lib/chromium/chromium

Linux, the same binary:
55b242496000-55b242497000 r--p 00000000 00:4a 8   /tmp/mref
55b242497000-55b242498000 r-xp 00001000 00:4a 8   /tmp/mref
55b242498000-55b242499000 r--p 00002000 00:4a 8   /tmp/mref
55b242499000-55b24249a000 r--p 00002000 00:4a 8   /tmp/mref
55b24249a000-55b24249b000 rw-p 00003000 00:4a 8   /tmp/mref
```

**Every `maps` line here says offset `00000000`, device `00:00`, inode `0`.** Linux
prints the segment's own file offset (`00001000`, `00002000`, `00003000` for the
code, rodata and data segments) and the file's real device and inode.

That is exactly what produces crashpad's two lines. A snapshot reader that builds a
module's layout from `maps` computes each segment's file offset from column 3; given
0 for all of them it reads the **ELF header** where it expects the code segment, and
`7f 45 4c 46` where it expects program data is "Incorrect ELF magic number". The
module list then comes out empty, which is `no module mappings`.

**The diagnosis below is from reading the four functions the columns come from and
from two probes in the guest, not from guessing.** The chain is:

- `maps_of` → `format_map_lines` prints `line.offset`, `line.dev`, `line.inode` —
  **the formatter is correct**;
- `mapping_lines` builds each `MapLine` from `VmaKind::Anon` → `(0, (0,0), 0, false)`
  and from `VmaKind::Frames { offset, .. }` → `object.identity()`;
- `map_frames` records the offset the syscall was given, and `sliced()` adjusts it —
  so the offset column has the right input, **and this was verified in the guest**:
  mapping one file at offsets 0 and 8192 prints `00000000` and `00002000`
  respectively. **The offset column is correct.**
- `MappedInode::identity()` is `(self.0.rdev().unwrap_or((0, 0)), self.0.metadata().ino)`.

So there is **one** defect, and one *different* fact that explains the rest:

1. **FIXED (this turn) — the device column was `00:00` for every ordinary file,
   because `rdev()` is the wrong question.** `rdev` is the device a *device node*
   names — right for `/dev/*`, and `(0,0)` for every regular file. `maps` wants
   **`st_dev`, the filesystem the file lives on**. The comment in `MapLine` stated the
   alibi plainly: "Both zero for a mapping with no file behind it, and for a file on
   this kernel - `stat` reports zero there too, so the two agree rather than
   disagreeing." **Two files agreeing about a value neither can compute is not
   agreement**; it is one shared gap, reported twice.

   There were in fact **two** such functions, in different layers: `FilePages`
   (`src/sched/runtime/child_mm.rs`, the object a **private** file mapping is built
   from) and `MappedInode` (`src/vfs/mod.rs`, the frame-backed one). Both asked
   `rdev()`; fixing only `MappedInode` changed nothing for an ordinary file, and the
   measurement said so — the guest still printed `00:00`. Both now ask the mount
   table, the same door `fstat` uses, so the two files agree by construction.
   Verified in the guest: `/bin/busybox` mapped at offset 4096 now prints
   `00001000 00:01 323` where it printed `00001000 00:00 323`, and `stat` answers
   `(0,1)`/`323` — the same pair. Pinned by `t-maps-identity` (9 claims, host and
   both arms).
2. **The executable's segments show offset 0 because exec does not create file-backed
   VMAs at all.** `map_image` (`src/sched/runtime/load.rs`) allocates a frame per page,
   fills it from the image's source, and records `space.map(Mapping { .. })` — a
   **pre-populated private anonymous** mapping. That is a deliberate design (the image
   is placed and honoured, and `.bss` falls out of the zeroed tail), and it means the
   program's text is genuinely not backed by the file afterwards. `maps` reports what
   the VMA is, so `0` is *honest* about the offset — the mapping really has none.

**That second point is what crashpad actually trips over**, and it is a far more
interesting finding than a wrong column: a snapshot tool that reads `maps` to recover
each module's segment layout cannot work against a kernel that never keeps the file
backing. The ELF header is at file offset 0 in *every* `[vdso]`-less module, so
`pread(exe_fd, 0, 4)` is what crashpad does for all of them, and for the data
segments that returns header bytes. **This is not fixable by correcting a column** —
it needs either file-backed VMAs for `exec` images (a real memory-management change)
or an acceptance that `maps`-based unwinders cannot resolve this kernel's images.

**Why this matters beyond crashpad**: `maps` is what every profiler, heap tracer and
JIT symboliser reads to map an address back to a file and an offset. A device column of
`00:00` for every file, and possibly an offset of zero for every segment, makes *any*
such tool unable to attribute an address — and it fails **silently**, because the
numbers are well-formed, just wrong. That is the same class as §6.101's "a field
readable in the common case is answered from the kernel, not from the common case":
here the common case (no device node) is answered for every file.

## 5. Third item — the GFXINFO/sandbox surface, unmeasured here

Not reached in this run because the process died before the browser proper started,
but known from the package's own dependency list and from what Chromium does first:

- it is started **without** `--no-sandbox` in a desktop session, so `chrome-sandbox`
  (a setuid helper, present in the image at mode `04755`) runs, and it wants
  **user namespaces**, `prctl(PR_SET_NO_NEW_PRIVS)`, and `seccomp` — all served, but
  the combination under a sandbox is not pinned;
- `--headless --disable-gpu` deliberately avoids the GPU path. A **rendering**
  browser needs DRM/GBM/EGL, and this repo's own §7.4 records that the GL path
  works for weston and niri and kmscube — but `chromium`'s own GPU process has
  never been measured here.

These are listed as **unmeasured**, not as gaps, precisely because this run did not
reach them. Naming them as gaps would repeat the mistake §6.194 records.

## 6. Recommended order, and why

1. **`PTRACE_SEIZE` / `PTRACE_INTERRUPT`** (Finding B). Measured, named, bounded, and
   it is the one item that makes every *other* failure legible: without a working
   crash handler a browser bring-up is guesswork, because the interesting deaths are
   the ones that leave no output. It is a shaped extension of `PTRACE_ATTACH`'s
   existing machinery (`may_inspect` → `set_tracer` → stop → `wait` event), not a new
   subsystem — `SEIZE` is `ATTACH` without the group-stop plus an options word, and
   `INTERRUPT` is "stop a seized tracee now".
2. **`st_dev` in `Metadata`, and the maps device column** (Finding C's defect 1).
   Small and self-contained, and it is right on its own terms regardless of Chromium:
   `maps`' device column is a fact about the file that this kernel can compute and
   does not.
3. **The `libdbus` `r-x` write** (Finding A). Not a syscall gap — Linux faults on the
   same access — but a library writing its own text segment means something upstream
   is wrong: a relocation the loader mishandled, or a `TEXTREL` the package should not
   have. Worth finding because it is a *reader* of the loader, and the loader is what
   every program here goes through.
4. **The `exec`-image file-backing question** (Finding C's fact 2). This is the one
   that needs a design decision rather than a fix. It is listed last because it is the
   largest, and because the first three items may change what it needs to be.

**What Chromium itself needs after these is unknown, and that is stated rather than
predicted.** The run above never reached the browser proper — it died in the crash
handler before the GPU or sandbox paths were touched. Any claim about "Chromium needs
X next" beyond the four items above would be a guess dressed as a plan.

## 7. What this analysis does **not** claim

- It does not claim Chromium is close to rendering. `--version` and a `main` entry
  are not a browser.
- It does not claim the `libdbus` fault is harmless — only that it is not a
  **syscall-shape** bug, and that Linux behaves the same way for the same mapping.
- It does not claim `PTRACE_SEIZE` is all that stands between this kernel and a
  browser. It claims it is the **first measured** blocker, which is a different and
  checkable statement.
- It does not claim a browser will render after the four items. It claims those four
  are what the measurements name, and that everything past them is unmeasured.
