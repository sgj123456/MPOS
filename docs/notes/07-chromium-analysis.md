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

## 3b. Finding A2 — FIXED: `mremap` made an eager page supervisor-only

With `DBUS_SESSION_BUS_ADDRESS` set, Chromium got further and produced a fault that
**was** a real kernel bug. It is fixed; this is how it was found, because the route
matters for the next one.

```
CacheThread_Blo[725]: segfault at 7e820912c000 ip 7e6ecc892b40 error 5
   in /lib/ld-musl-x86_64.so.1
PFDIAG pte=0x800000003acfd003  P=1 W=1 U=0
PFDIAG vma=NONE (no mapping describes 0x7e820912c000)
PFDIAG fault_in=0x7e820912c000 eager[/tmp/cp3/Default/Web Data]
   op=[f3, 0f, 6f, 06, ...]     <- `movdqu xmm0,[rsi]`, a 16-byte SSE load
```

### The decode

- `error 5 = 0b101` → `P=1`, `U=1`, `W=0`: a ring-3 **read** of a page that **is
  present**;
- the PTE is `0x800000003acfd003`. Masking the physical address out
  (`phys = 0x3acfd000`, which PFDIAG printed) leaves **`P=1, W=1, U=0, NX=1`** — and
  that is exactly `page_flags(MapFlags::READ | MapFlags::WRITE)`, i.e. a leaf
  written from a flag set with **no `MapFlags::USER`**;
- `rip` is a **16-byte SSE load inside `ld-musl`'s `memcpy`**, reading the process's
  own `Web Data` file — memory it plainly owns;
- the *first* fault of the same run had `P=1 W=0 U=1`, i.e. **correct**. Only this
  shape was wrong.

### The bug

`mremap` carries each page's frame to the new address and re-maps it with `USER`
only when the page is *reachable from ring 3*:

```rust
let reachable = sched::vma_prot_at(page).is_some_and(|p| p.contains(MapFlags::USER));
let mut mf = MapFlags::READ | MapFlags::WRITE;
if reachable { mf |= MapFlags::USER; }
```

and `vma_prot_at` consulted **only the VMA table**. An **eager** range — a
`MAP_SHARED` file copy, an image placed by `exec` — has **no VMA at all**
(`record_eager_range`'s own note says so), so `vma_prot_at` answered `None`, the
page looked unreachable, and its new leaf was written without `USER_ACCESSIBLE`.
The process's own readable memory became supervisor-only.

**The fix**: `vma_prot_at` now falls back to the eager-range table when no VMA
describes the page — the same two sources `/proc/<pid>/maps` already printed
(`mapping_lines`), so a fault report and `maps` cannot disagree.

### The pin, and the counterfactual that had to be fixed first

`t-mremap-eager` (6 guest claims, both arms). Its **first** version used an
anonymous mapping and passed **against a deliberately broken kernel**, because
`MREMAP_MAYMOVE` only *permits* relocation — a page that can stay put is left
alone, so no page ever travelled and the loop never ran. The pin now reserves a
destination and uses `MREMAP_FIXED`, which forces the carry.

With the fallback disabled and the move forced:

```
t-mremap-eager[10]: segfault at 7ee63fca5000 ... error 5
=== x exit=139
```

— the same `error 5` on a present page that Chromium produced. Restored, it is
green on both arms.

**Named leftover**: `move_eager_ranges` moves the *record* and `unmap_user` clears
the old leaves, but a page that is eager **and** covered by a VMA (the dynamic
linker's read-only pass over a shared page) is still answered from the VMA first.
That ordering is deliberate — the VMA is the narrower claim — and is now the one
case a reader should suspect if a similar fault reappears.

## 3c. Re-measured on a kernel carrying the A2 fix — and how to boot one at all

### The measurement harness, recorded because it is not obvious

`cargo xtask run` cannot refresh the image here (its privileged half needs `sudo`,
which this container blocks), and the **loader embeds the kernel**, so editing
`build/mpos-uefi-x86_64.img`'s root filesystem can never change which kernel boots.
The only way to run a newly-built kernel against the real Chromium environment is
to **replace `EFI/BOOT/BOOTX64.EFI` inside the image's ESP**. There is no
`mtools`, no loop device and no root in this sandbox, so that was done with a
small FAT32 writer over the image (walk the root directory, follow the cluster
chain, rewrite the data, both FATs and the directory entry's size — a rebuilt
loader is smaller, so the tail clusters are freed).

Two false starts are worth recording because both cost a boot:

- the ESP is a **virtual FAT** (`-drive file=fat:<dir>`) and grows to hold the
  250 MB test initramfs, so it enumerates as `vda` at ~500 MB and **the installed
  disk becomes `vdb`** — mounting `vda2` silently finds the wrong thing;
- that virtual FAT also reached QEMU's 504 MiB ceiling, which is what the kernel
  reported as "`vda, 1032192 sectors`" — a size that has nothing to do with the
  2 GiB image.

### What the fixed kernel showed

With `EFI/BOOT/BOOTX64.EFI` replaced by a loader built from the fixed kernel,
Chromium ran again. **The `U=0` fault is gone**: the only `PFDIAG` line of the run
is `pte=0x3fa18005 P=1 W=0 U=1` — a present, **user-accessible**, read-only page,
which is the correct shape for an `r-x` file mapping. The A2 fix holds under the
real workload, not just under its pin.

Two other things are visible and are **new information**:

- `crashpad` still logs `ptrace: I/O error (5)` **12 times**, and a temporary
  diagnostic on `ptrace`'s unknown-request arm **printed nothing** — so those
  `EIO`s do **not** come from a request this kernel fails to recognise. They come
  from somewhere else in the ptrace path (most likely `PTRACE_GETREGSET`'s iovec
  or the `may_inspect` refusal being reported with the wrong errno). That is the
  next thing to bisect, and it is now narrowed by measurement rather than assumed.
- one log line names **`/proc/self/locales` returning `EOPNOTSUPP`**. Whatever
  wants that path, this kernel does not serve it. Recorded as a lead, not yet
  chased.

  **It was chased on 2026-11-17 and deliberately left unpinned.** The obvious
  fix — overriding `ProcDir::create` to answer Linux's measured `ENOENT`
  (`build/scratch/mkerr/m.c`) instead of the default `EOPNOTSUPP` — was written,
  pinned, and then **withdrawn**: the counterfactual (restoring `Unsupported`)
  left every pin claim green, so the pin named nothing. `mkdir /proc/self/locales`
  answers `ENOENT` on the *test* boot already, which means the `EOPNOTSUPP` in the
  Chromium run came from a state specific to that boot (a different mount or
  resolution path), not from `ProcDir::create`. A change that cannot be made red
  by reverting it is not a fix; it was reverted and no code landed.


**Chromium itself still does not render**: it takes its own `CHECK` (`trap 3`,
`IMMEDIATE_CRASH`) after the crashpad storm. The kernel-side faults on this path
are now fixed; what remains is userspace/environment plus the crashpad `EIO`.

## 3d. Finding B — FIXED: `NT_PRFPREG` was the whole of crashpad's `EIO` storm

The 12 `ptrace: I/O error (5)` lines were **not** an unknown request: a temporary
diagnostic on `ptrace`'s unknown-request arm printed nothing at all. Reading the
`GETREGSET` arm found it:

```rust
abi::PTRACE_GETREGSET | abi::PTRACE_SETREGSET => {
    if addr != abi::NT_PRSTATUS { return err::EIO; }   // <- every other note type
```

`addr` is the **note type**. `NT_PRSTATUS` (1) is the general registers;
**`NT_PRFPREG` (2) is the x87/SSE file**, which a crash handler wants because the
SSE control word and the vector registers are what a floating-point crash is
about. This kernel served only the first and answered `EIO` — "I do not know this
request" — for the second, which tells a tracer the whole mechanism is absent
rather than that one set is.

`PTRACE_GETFPREGS`/`SETFPREGS` beside it already carried the same bytes, so the
two spellings now answer the same thing.

**Verified on the image, not only in a pin**: with a loader built from the fixed
kernel,

| | before | after |
|---|---|---|
| `crashpad ... ptracer.cc:46] ptrace: I/O error (5)` | **12** | **0** |

`t-ptrace-fpregset` (9 claims, host and both arms) pins the contract: the call is
served, the **iovec's length is rewritten** to the bytes moved (512 on x86_64,
528 on aarch64), `SETREGSET` round-trips a value the next `GETREGSET` reads, an
unknown note type is **still** refused, and `NT_PRSTATUS` still works — the
regression this change could have caused by taking the wrong branch.

## 3e. What actually blocks Chromium now — measured state of the live process tree

After the two kernel fixes (A2 `mremap`, B `NT_PRFPREG`), the picture changed
enough that the earlier "it takes its own CHECK and dies" reading no longer holds.
A harness that starts Chromium in the **background** and lets the guest shell
print 45 seconds later (`build/imgcmd/bg.sh`, `wait.sh`) settles it:

```
  chromium pid=715; sleeping 45s
  === AFTER 45s: still running? ===   alive
  === output so far ===               (0 bytes)
```

`ps` at 30 seconds shows a **complete, live process tree**:

```
715  chromium                         {sandbox_ipc_thr}
718  chrome_crashpad_handler          --monitor-self ...
720  chromium --type=zygote --no-zygote-sandbox --no-sandbox
721  chromium --type=zygote --no-sandbox
```

and the main thread's `/proc/715/stat` is `S` (sleeping), with **no**
`mpos: syscall ... -> EOPNOTSUPP/ENOSYS/EACCES` diagnostics anywhere in the run.

So the browser is **not crashing out**; it is **stuck waiting onIPC** after the
zygotes start, before a renderer produces any DOM (`/tmp/cwo.txt` stays at 0
bytes). The `trap 3` lines in earlier runs are child threads crashing while the
main process kept waiting — which is why a foreground `timeout` appeared to hang
and why no CHECK text was ever captured: there was no CHECK-driven exit.

The `PFDIAG` lines in this state are **not** the kernel bug any more: e.g.
`pte=0x3fc19025 P=1 W=0 U=1` with `vma=... r-xu` is a correct read-only executable
mapping whose demand fault resolved (`B9=0`). Several `MemoryInfra`/`inotify_reader`
threads take such faults and keep running.

**This is the precise starting point for the next investigation** — it is no
longer "find a crashing syscall". It is: what IPC is the zygote/sandbox channel
waiting for, and why does it never complete? A `futex`/`poll`/`socketpair` state
inspector, or a `strace`-style trace of the zygote's fd traffic, is the next
instrument. No code change is claimed here.

## 3f. The live process, sampled over time — DOM once, then a stable stall on resources

Sampling `/proc/<pid>/syscall` three times across ~44 s (`build/imgcmd/ipc2.sh`)
produced **9096 bytes of output in one run** — the only time DOM-like output has
ever appeared — but a repeat with a longer wait and **no sampling** produced 0
bytes after 90 s (`build/imgcmd/dom2.sh`). So the success was not stable, and the
sampling itself may have changed timing. Do not claim "Chromium renders" from
one run.

What the repeat *did* give is the first **stderr**, which names two concrete,
different problems:

```
[crashpad] sched_getscheduler: Function not implemented (38)
[crashpad] open /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq: ENOENT
[769] Failed to load /proc/self/chrome_100_percent.pak
[769] Failed to load /proc/self/resources.pak
```

- `sched_getscheduler` is **served** by this kernel (nr 145 →
  `handle_sched_getscheduler` returns `SCHED_OTHER`), so crashpad's ENOSYS is
  from a *different* scheduler probe; the exact number was not captured, so it
  is not yet a gap to fix.
- `/proc/self/exe` was checked in the guest and is **correct**:
  `readlink /proc/self/exe` returned `/usr/bin/readlink` for that reader, and
  `/proc/<pid>/exe` comes from `task(slot)?.exe` in `procfs.rs`, so it does not
  follow the observer. Chromium's `/proc/self/*.pak` paths are therefore its own
  resource-path logic, **not** a wrong `exe` link.

The main process stays alive throughout, with ~51 open fds; zygotes exit (`Z`)
in the run that produced output. The stall after that is waiting on resource
loading, not on a crashing syscall.

## 3g. Negative results (2026-11-17) — what did NOT reproduce, so nothing was landed

Several follow-ups were tried and explicitly did **not** yield a landable fix.
They are recorded so a later reader does not re-litigate them.

- **An ENOSYS note naming crashpad's missing call**: the kernel prints
  `mpos: syscall <nr> -> ENOSYS` once per number, so a fresh Chromium run should
  name the exact call behind crashpad's "Function not implemented (38)". A
  dedicated run (`build/imgcmd/eno.sh`) produced **no ENOSYS notes at all** —
  and, like several repeats this session, **0 bytes of output** with the main
  process still alive. So the earlier stderr that named `sched_getscheduler` was
  from a *different*, non-reproducing run. There is no measured syscall gap to
  fix right now.
- **`fchroot` (472)** was checked as a candidate census item: the host is
  non-root, so `fchroot(-1)` → `EINVAL`, a bad fd → `EBADF`, a regular file →
  `EINVAL`, but the **successful** directory path could not be measured. A door
  whose success half cannot be measured is not implemented here.
- Repeats of the DOM run remain **unstable** (one 9096-byte run, several 0-byte
  runs with no sampling). Any claim of "Chromium renders" would be reading a
  single lucky run as a contract.

**Conclusion of this pass**: the honest state is "no new measured, pin-able
kernel defect". No code changed. The remaining Chromium blocker is in a flaky
userspace/resource-loading path, and instrumenting it further needs a run that
reliably reaches the same point — which the current guest setup does not.

## 3h. FIXED — `/proc/<pid>/exe` reported the caller's raw argument, not an absolute path

Chromium's stderr named `/proc/self/chrome_100_percent.pak` and
`/proc/self/resources.pak` as resource paths it could not load. A resource loader
locates itself with `dirname(readlink("/proc/self/exe"))`, so a **relative** link
sends it to the wrong directory. That was the lead, and it turned out to be a real
kernel defect with a clean measurement on both sides.

**Measured on Linux 7.3.4** (`build/scratch/rex.c`), invoked as `./rexprobe` from
`/tmp`:

```
exe=/tmp/rexprobe      <- absolute, though argv0 was ./rexprobe
```

and the same absolute path when invoked absolutely. Linux canonicalises.

**Measured in this kernel, before the fix** (`build/probe/rel/serial.log`), with a
purpose-built boot whose `/init` `cd`s to `/bin` and execs `./probe-exepath`:

```
exe=./probe-exepath    <- the caller's raw argument
argv0=./probe-exepath
cwd=Ok("/bin")
```

**The cause**: `handle_execve` passed the raw `cstr_arg` string into `exec_inode`,
which stored it as `Task::exe` unchanged.

**The fix**: `exec_inode` normalises it once — absolute paths through
`vfs::path::normalize`, relative ones joined to `sched::current_cwd_path()`. Both
exec doors share the body, so both are covered; the `execveat` `AT_EMPTY_PATH` arm
hands over the descriptor's own label, which is absolute by construction.

**After**, the same relative exec reports `exe=/bin/probe-exepath` while `argv0` is
still `./probe-exepath` — correct, because Linux leaves `/proc/self/cmdline` alone.
The pin checks the link only; `argv[0]` is a different question.

`t-proc-exe-path` pins it (4 claims): the child re-execs itself by a bare relative
name from its own directory and reports its link back through a pipe, and the
parent compares it against its own absolute path. **Counterfactual**: reverting only
the normalisation reddens exactly two claims with `./t-proc-exe-path` — the defect
as first observed — and restoring it is green on both arms.

**A harness fact that cost three attempts**: `one-test-boot.sh` rewrites
`stage/init` from its own template on every run, so a custom init is silently
overwritten. The measurement only worked once the boot script was written
separately (`.workbuddy/relative-exec-boot.sh`), and the earlier "the relative case
was never measured" conclusion in this file was a consequence of that, not of the
kernel.

## 3i. The `exe` fix verified on the image — and the next blocker is the GPU process

With a loader carrying the `/proc/<pid>/exe` fix built into the image's ESP, the
Chromium run changed in a measurable way:

| signal | before | after |
|---|---|---|
| `Failed to load /proc/self/chrome_100_percent.pak` | present | **0 occurrences** |
| `Failed to load /proc/self/resources.pak` | present | **0 occurrences** |

The fix did what it was supposed to: a resource loader that finds itself with
`dirname(readlink("/proc/self/exe"))` now lands in the right directory.

**The next blocker is named and different**:

```
6x ERROR:content/browser/gpu/gpu_process_host.cc:1036] GPU process launch failed
 1x FATAL:content/browser/gpu/gpu_data_manager_impl_private.cc:385]
    GPU process isn't usable. Goodbye.
```

Chromium launches a GPU process, it fails six times, and the browser gives up with
a **FATAL** — a decided exit, not a hang. That is progress: the failure moved from
"resources cannot be found" to "the GPU process cannot start".

Two caveats stated plainly:

- the guest stays **flaky** — repeats of the same command sometimes produce this
  GPU FATAL and sometimes no stderr at all, with DOM output at 0 bytes either way;
- the `PFDIAG` lines that appear alongside (`libdbus`, `libfontconfig`,
  `pte=…025 P=1 W=0 U=1`) are **not** a leftover from an old kernel — `pf_diag`
  lives in `arch/src/arch/x86_64/idt.rs` and runs *after* `resolve_anon_fault`, so
  `P=1` is the **resolved** state and the faulting access was genuinely a
  not-present page that the handler then filled.

A single-threaded probe that maps `ld-musl` `r-x` and walks all 64 pages with data
reads plus fetches through `ret` pages completed with **no fault**, so the
`error 14` seen under Chromium needs concurrency to reproduce — consistent with
the §6.181 note in `resolve_anon_fault` (a block transfer parks the task and the
borrow is what suffers). Nothing was changed on that basis.

## 3j. The stall is real and stable: output freezes at ~100 s and never moves

Earlier passes called this "flaky" because short runs differed. A long,
uninterrupted run with progress sampled every 25 s (`build/imgcmd/long.sh`)
settles it - and the answer is **not** flakiness:

```
  t=25s  alive=Y dom=0 err=0
  t=50s  alive=Y dom=0 err=5736
  t=75s  alive=Y dom=0 err=12472
  t=100s alive=Y dom=0 err=12785
  t=125s alive=Y dom=0 err=12785
  ...
  t=300s alive=Y dom=0 err=12785      <- unchanged for 200 s
```

Chromium **does** make progress for the first ~100 s (crashpad, policy loader,
variations, dbus name lookups) and then **stops dead**, process alive, `dom` at 0
bytes forever. That is a permanent block, not a slow machine - a 296 MB binary
under TCG slows *progress*, it does not freeze a byte count.

The last stderr lines repeat:

```
WARNING:crashpad ... sched_getscheduler: Function not implemented (38)
```

**That warning is not from a kernel gap**: the kernel serves `sched_getscheduler`
(nr 145 -> `SCHED_OTHER`), and a run this long would have printed the kernel's
once-per-number `mpos: syscall <nr> -> ENOSYS` note - **no such note appears
anywhere in the log**. So whatever crashpad probes, the kernel answers it without
`ENOSYS`.

**The open question is now narrow**: after ~100 s, which thread is parked on what?
Sample `/proc/<pid>/syscall` **after** the freeze, not before it.

## 3k. Post-freeze sample: the main process is GONE, zygotes are zombies

Section 3j said output freezes at ~100 s. Sampling `/proc` **after** the freeze
(`build/imgcmd/frozen.sh`, t=130 s) shows why, and it is not a deadlock:

```
  pid=717 comm=chrome_crashpad   tid=717 st=S   tid=743 st=S
  pid=719 comm=chromium          tid=719 st=Z      <- zombie
  pid=720 comm=chromium          tid=720 st=Z      <- zombie
  pid=742 comm=chrome_crashpad   tid=742 st=S
```

- the **main browser process is not in the list at all** - it exited;
- both **zygotes are zombies** (`Z`), dead and uncollected;
- only the **crashpad handlers** remain, sleeping (`S`).

So the freeze is the parent shell waiting on a `--dump-dom` whose process already
died, with the orphaned crashpad handler keeping the wait alive. The last records
before it are the two zygotes' `int3` (`IMMEDIATE_CRASH` - their own CHECK), and
`VideoCaptureThr` taking an `error 14` fetch fault on `libfontconfig` first.

**What that narrows to**: the browser dies because its zygotes crash, and the
zygote crash is preceded by a fetch fault on a private `r-x` file mapping. A
single-threaded walk of 64 such pages faulted nowhere (§3i), so the fault needs
**concurrency** - the same shape `resolve_anon_fault`'s own §6.181 note records (a
block transfer parks the task; the borrow is what suffers).

**Next instrument, named**: a multi-threaded probe that hammers `r-x` file pages
from several threads at once, to reproduce `error 14` under concurrency without
Chromium's 100-second startup. That is the shortest path from here to a landable
fix, and this pass did not attempt it.

## 3l. The concurrency probe reproduced nothing — and that names the untested half

Section 3k said the `error 14` needs concurrency. A 6-thread probe that maps a
512 KiB `r-x` file privately per thread and touches every page 40 times
(`probe-mtfetch`) completed with **30720 page touches and no fault at all**.

So concurrency alone is not the trigger. What the probe is missing is the one
thing the note in `resolve_anon_fault` names: **the file it mapped lives on the
initramfs (ramfs), where `frame_at` never parks.** Chromium maps its libraries
from the **ext4 disk**, where the block transfer parks the faulting task - and
that park, while a borrow is meant not to be held, is exactly the §6.181 shape.

**That is the untested half, and it is now the only remaining hypothesis with a
named mechanism**: a multi-threaded probe over an `r-x` file on the ext4 root (a
file the test creates on the disk, not one shipped in the initramfs), so
`frame_at` really parks. Everything else about the fault is measured:

- `ip == fault address` and `error 14` -> an instruction fetch from a not-present page;
- `PFDIAG` prints `P=1` because it runs *after* `resolve_anon_fault` resolved it;
- the main process exits and leaves zygote zombies (§3k).

No code was changed in this pass; the probe was removed after its negative result.

## 3m. The ext4 concurrency probe also faulted nowhere — so the trigger is neither concurrency nor parking

Section 3l named the last hypothesis with a mechanism: a multi-threaded probe over
an `r-x` file **on the ext4 disk**, so `frame_at` really parks. That probe was
written and run (`probe-ext4mt`: 8 threads, each mapping its own private `r-x`
view of the mounted ext4 fixture and walking 64 cold pages 8 times, with the SATA
disk attached through AHCI exactly as the gate wires it):

```
  target size=67108864
  4096 ext4 page touches across 8 threads, no fault
```

**4096 cold ext4 page touches, 8 threads, concurrent parking through the block
layer - and no fault.** Combined with §3l's ramfs probe (30720 touches, no fault),
both candidate triggers are refuted by direct measurement:

| hypothesis | probe | result |
|---|---|---|
| concurrency on a ramfs `r-x` mapping | 6 threads, 30720 touches | no fault |
| concurrency **and** block-layer parking on ext4 | 8 threads, 4096 cold touches | no fault |

So the `error 14` under Chromium needs something neither probe reproduces. The
remaining difference is the hardest to fake in a small fixture: in Chromium the
mapping is a **shared object being concurrently executed** while the page is still
cold - the faulting instruction *is* the page's own first instruction, and other
threads enter the same library at that moment. A probe that reads bytes never asks
the CPU to *execute* an unfetched page, which is precisely what `ip == fault
address` says happened.

**Named as the next instrument**: a probe that maps a real ELF `r-x` segment from
ext4 and *calls into it* from several threads at once, so the first instruction of
a cold page is a fetch. That is a different probe from both above, and it is where
the remaining evidence points. No code changed in this pass.

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
