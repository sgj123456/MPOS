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
  is not yet a gap to fix. **Resolved in §3q**: there is no probe at all — musl's
  wrapper is a stub returning `ENOSYS` without issuing the syscall.
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

## 3n. Calling into cold ext4 pages also faulted nowhere — three probes, three refutations

Section 3m's next instrument was built and run (`probe-callfetch`): the test
creates a file **on the mounted ext4 disk** filled with `ret` (0xc3) one byte per
page, maps it `PROT_READ|PROT_EXEC` privately in 8 threads, and **calls** into a
different page per thread — so each call's first instruction is a fetch of a cold
page, and two threads race on the same page by construction.

```
  wrote 64 pages of ret into /mnt/cf/fetch.bin
  512 cold-page CALLS across 8 threads, no fault
```

All three probes now agree:

| hypothesis | probe | result |
|---|---|---|
| concurrency on a ramfs `r-x` mapping | 6 threads, 30720 byte touches | no fault |
| concurrency + block-layer parking on ext4 | 8 threads, 4096 cold byte touches | no fault |
| **cold-page instruction FETCH from ext4, 8 threads racing** | 512 calls | **no fault** |

**What that establishes**: this kernel's demand-fetch path for a private `r-x`
file mapping is correct under concurrency, under block-layer parking, and when the
very first instruction of a cold page is the faulting one. The `error 14` Chromium
hits is therefore **not** a general defect in that path, and three separate
attempts to reproduce it with a small fixture have failed.

**The honest conclusion for this line of investigation**: the remaining difference
is not something a synthetic probe has captured yet — it is Chromium's own
`SIGSEGV` handler interaction, or the trace showing `ip == addr` is being read
from a *different* task than the fault. Continuing to guess at the mechanism
without a reproducing fixture is no longer productive; the useful next step is
a **reproducing Chromium run with `ptrace` attached to the zygote**, so the fault
can be inspected live rather than reconstructed from a serial log. No code
changed in this pass.

## 3o. The whole sched family is correct — crashpad's ENOSYS warning is not this kernel's

Chromium's crash handler repeats `sched_getscheduler: Function not implemented
(38)`, and three passes treated it as a possible kernel gap. It is not, and the
measurement is direct rather than an inference.

**On Linux 7.3.4** (`build/scratch/schedcmp/s.c`) and **in the guest**
(`probe-sched`, `probe-sched2`), every call in the family answers the same:

| call | Linux | this kernel |
|---|---|---|
| `sched_getscheduler` | `0` (SCHED_OTHER) | `0` |
| `sched_getparam` | `0` | `0` |
| `sched_getattr(48)` | `0`, size 48 / nice 0 / policy 0 | identical fields |
| `sched_getaffinity(128)` | `8` | `8` |
| `sched_rr_get_interval` | `0` | `0` |
| `prio_max/min(SCHED_OTHER)` | `0` / `0` | `0` / `0` |
| `prio_max/min(SCHED_FIFO)` | `99` / `1` | `99` / `1` |
| `prio_max(SCHED_RR)` | `99` | `99` |
| `prio_max(SCHED_BATCH)` | `0` | `0` |

**No run of this kernel has ever printed its own `mpos: syscall <nr> -> ENOSYS`
note** - grep of every saved serial log and gate log finds none. The kernel prints
that note exactly once per number the first time it answers `ENOSYS`, so a
`sched_getscheduler` that returned `ENOSYS` would have left a line. There is none,
and the guest answers `rc=0`.

So the warning is **crashpad's own**, not the kernel's answer: the runner is either
reading a stale cached capability or attributing an unrelated failure to the
scheduler query. Nothing to fix here, and this closes the last syscall-shaped
hypothesis for the Chromium stall.

The family is already pinned - `t-sched-policy` covers the priority bounds,
`t-sched-attr` the `getattr` shapes and error order, `t-sched-getaffinity`,
`t-sched-pid` and `sched-order` the rest - so no new pin was needed for a
confirmation that is already a contract.

**Postscript (2026-11-17): the warning's source is named, not argued.** It is
musl's own stub: `src/sched/sched_getscheduler.c` is
`return __syscall_ret(-ENOSYS);` (its `sched_setscheduler.c` sibling is the
same), so the wrapper answers before any syscall exists - which is why no
`mpos: syscall 145 -> ENOSYS` line can appear and why the warning repeats once
per thread. The table above stands as the kernel's answer to the *raw* syscall;
a program using musl's wrapper gets the stub's `ENOSYS` on this machine exactly
as it would on any musl system. §3q carries the correction in place.

## 3p. FD flags and descriptor handover are correct — and one probe of mine was wrong

Two measurements this pass, one of which is a correction of my own.

### The CLOEXEC family: correct, and my first probe was wrong

A probe that checked `FD_CLOEXEC` after each door that claims to set it reported
`eventfd2(EFD_CLOEXEC)` as a MISMATCH. **The probe was wrong, not the kernel**: it
passed `1`, which is `EFD_SEMAPHORE`, not `EFD_CLOEXEC` (`0x80000`). The kernel
correctly did *not* set `CLOEXEC` for a flag word that never asked for it.

Corrected and re-measured on Linux 7.3.4 and in the guest, all thirteen agree:

| door | `F_GETFD` CLOEXEC |
|---|---|
| `open(O_CLOEXEC)` / `open(0)` | `1` / `0` |
| `pipe2(O_CLOEXEC)` / `pipe2(0)` | `1` / `0` |
| `dup3(O_CLOEXEC)` / `dup3(0)` | `1` / `0` |
| `eventfd2(EFD_CLOEXEC)` / `(EFD_SEMAPHORE)` / `(both)` | `1` / `0` / `1` |
| `inotify_init1(IN_CLOEXEC)` | `1` |
| `F_DUPFD_CLOEXEC` | `1` |
| `F_SETFD` set / clear then get | `1` / `0` |

**The lesson is the one this repo already states**: a probe's own constants are
part of the measurement, and a mismatch is as likely to be the instrument as the
target. Had this been reported without re-reading the flag word, it would have
been a fabricated kernel bug.

### Descriptor handover: also correct

Chromium's GPU child dies with `Crashing due to FD ownership violation:` (no name
after the colon), which is `ScopedFD`'s bookkeeping DCHECK. `probe-fdsem` measured
the primitives that bookkeeping rests on, on Linux and in the guest:

```
  open a=3, dup(a)=4
  after close(a): fcntl(b)=0        <- the copy survives closing the original
  after close(b): fcntl(b)=-1       <- and the close is real
  closed 3 then opened 3            <- lowest-free reuse
  sendmsg(SCM_RIGHTS) -> 1
  sender fd 5 still open? fcntl=0
  received fd=5, read 10 bytes      <- the received descriptor is USABLE
```

Every line matches Linux. So neither the flag family nor `dup`/`close`/`SCM_RIGHTS`
is the cause, and the GPU child's DCHECK fires for a reason downstream of both -
its own `ScopedFD` lifecycle, not a syscall shape this kernel gets wrong.

`t-unix-scm` already pins the handover, `t-unix-peek`/`t-unix-dgram`/`t-unix-close`
the rest of the unix-socket surface, so no new pin was warranted for a confirmation
that is already a contract.

## 3q. The stderr tail, finally — and what it rules out

Every earlier pass lost Chromium's stderr because the guest's `/tmp` died with
it. A run whose shell prints `tail -30 /tmp/te.txt` **after** the browser is
killed finally closed that gap (`build/imgcmd/tail.sh`). The last three lines:

```
[..:WARNING:crashpad..process_reader_linux.cc:95] sched_getscheduler: Function not implemented (38)   x27
[..:ERROR:crashpad..file_io_posix.cc:145] open /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq: ENOENT
[..:ERROR:crashpad..file_io_posix.cc:145] open /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq: ENOENT
[..:ERROR:crashpad..exception_handler_server.cc:131] tgkill: No such process (3)
```

Four things that follow, each checked rather than assumed:

1. **`sched_getscheduler: Function not implemented (38)` never reaches this
   kernel.** The image is Alpine, so the libc is musl, and musl does not
   implement this call: its `src/sched/sched_getscheduler.c` is
   `return __syscall_ret(-ENOSYS);` and `sched_setscheduler.c` is the same, so
   the wrapper answers with `ENOSYS` **before issuing a syscall**. That is why
   the kernel printed none of its once-per-number `mpos: syscall <nr> -> ENOSYS`
   notes (`grep -c` = 0) and why the line repeats once per thread (`x27`)
   rather than once: no `ptrace`-visible syscall happened. A probe that calls
   the *raw* syscall answers `0` (SCHED_OTHER), so the kernel's side is served;
   the warning is musl's stub, not a kernel refusal, and not "crashpad's
   inference" either — crashpad is reporting the errno musl handed it. The
   sibling stub is worth carrying: a program using musl's `sched_setscheduler`
   gets `ENOSYS` on this machine as it would on any musl system.
2. **The `cpufreq` reads are a missing sysfs file**, not a syscall gap — this
   kernel has no cpufreq driver, and `ENOENT` is the honest answer for a file
   that does not exist.
3. **The last line before the freeze is `tgkill: No such process`** — crashpad's
   exception handler signalling a thread that has already exited. `ESRCH` is
   Linux's answer there too, so the kernel is right; crashpad gives up on that
   client and the browser stops making progress.
4. **The threads it was signalling die by their own `CHECK`, not by a fault.**
   `CacheThread_Blo` appears with three different tids (727, 728, 741) — the
   thread is recreated — and each one takes a `trap 3` with `0f 0b` at `rip`:
   Chromium's `IMMUTABLE_CRASH`. Its registers at that point are all plausible
   (`rdi = 0x395400004a20`, `r10 = 0x39500011bc80`, `rbx` a live stack address),
   so it is a **failed predicate**, not a wild pointer.

**So no measured defect is left on the kernel's side of this chain**: the
`libnss3` fetch faults are resolved (`PTE P=1` afterwards, `run=` growing),
`ESRCH` is the correct answer, and no syscall was refused. This is not "the
kernel is clean" — a failed predicate can also be a wrong *successful* answer —
it is "nothing here is measured wrong". What remains is Chromium's own decision
to abort a worker, which is where the next instrument belongs — a run with
`--v=1` and `--enable-logging=stderr` that captures the `CHECK` message the
`ud2` follows, rather than the crashpad chatter that precedes it.

## 3r. The CHECK text: not on this channel, and the trap is flaky

The instrument §3q named was run - headless Chromium with
`--v=1 --enable-logging=stderr --in-process-gpu` on the installed image, stderr
captured to a file and printed after the browser was killed - plus a sampler that
copied `/proc/<pid>/maps` for every `chromium` process every two seconds. Five
boots; what they settled:

1. **No `CHECK`/`FATAL` line ever appears on that stderr.** `grep -aE
   "CHECK|FATAL|Check failed"` matched nothing in any run, and the byte count was
   4 524 in one and **20 562 in three**, i.e. the child's log is not reaching this
   file at all. The crash is in a child (the trapping pids 737/738 are forked
   after the zygote start, not the browser 715), and a child's `CHECK` is not on
   the parent's stderr: the `--enable-logging=stderr` switch routes the process
   that owns the flag, not its zygote children. So "capture the CHECK text this
   way" was the wrong instrument, and the run says so rather than nearly working.
2. **The `#BP` itself is flaky.** It appeared in two of five runs, always in a
   forked child, at the same instruction class (`rip` on a `ud2` whose preceding
   byte is `int3`); the other three runs ran to the kill with no trap. Two
   consecutive runs without it ended the probe, per the rule the Chromium plan
   states.
3. **The registers at the trap are a real datum** (`rax 0`, `rbx 2`,
   `r12 0xffffff00`, `rdi 0`, `rsi` a module pointer), and they are now printed
   with the module clause (below). What they do **not** establish is a kernel
   defect: no syscall failed, and the values are a child's own computation.
4. **Outside-in address arithmetic cannot close it.** The obvious move - derive
   the image's load base from `rsi - <string offset>` and read the instruction's
   file offset - does not survive this kernel's exec image: every segment is
   pre-populated private anonymous memory, so `maps` prints file offset `0` for
   all of them (§4b, fact 2), and the ELF's `p_vaddr - p_offset` differs per
   segment (`readelf -lW`: the executable LOAD is `offset 0x1e64304`, `vaddr
   0x1e65304`). Two alignments fit the trap word, disagreeing by `0x2f49000`,
   one landing in string data and one in code. **No claim is made** about which
   function crashed; a "probably `pa_context_get_state`" from one of the two was
   discarded rather than written down.

**Instrument added instead.** The kernel's `traps:` line now carries the same
`in <module>[<offset>,<base>+<len>]` clause the `segfault` line has
(`src/kernel_services.rs`). The kernel laid the image, so it can say which
mapping the `ip` is in and at what offset - the two facts an outside reader
cannot recover - and every future user trap is self-locating. The next Chromium
run that reproduces the trap reads the module and offset straight out of the
line; if it does not reproduce, the next instrument is a live tracer
(`PTRACE_SEIZE` is now faithful, §4) rather than another stderr capture.

## 4. Finding B — FIXED: `PTRACE_SEIZE`/`PTRACE_INTERRUPT` were missing, and crashpad needs them

This was the first real, nameable gap in the trace:

```
[crashpad] ptrace: I/O error (5)   x12
```

`EIO` from `ptrace` is Linux's own answer for **a request the kernel does not know**
(`src/syscall/linux/proc.rs`, the `_ => err::EIO` arm), and crashpad probes with it.
What this kernel served then, and what crashpad asks for:

| request | number | before this fix |
|---|---|---|
| `PTRACE_TRACEME` | 0 | served |
| `PTRACE_ATTACH` | 16 | served |
| `PTRACE_GETREGSET` | 0x4204 | served |
| `PTRACE_GETSIGINFO` | 0x4202 | served |
| `PTRACE_CONT` | 7 | served |
| `PTRACE_SEIZE` | 0x4206 | **not defined → `EIO`** |
| `PTRACE_INTERRUPT` | 0x4207 | **not defined → `EIO`** |
| `PTRACE_LISTEN` | 0x4208 | not defined → `EIO` (still; see below) |

Crashpad is Chromium's crash handler; since Linux 3.4 it prefers the
`SEIZE`/`INTERRUPT` pair over `ATTACH`, because `SEIZE` attaches **without** stopping
the tracee and `INTERRUPT` then asks for a stop explicitly. Every probe of it here
got `EIO`, crashpad concluded it could not inspect the crashing process, and
`thread ID 719 not found in process` followed.

**Landed 2026-11-16 (`57a9529`), and re-measured 2026-11-17 against Linux 7.3.4
(`build/scratch/seize2/{p,q,r,t}.c`).** `SEIZE` writes the link and takes the
`PTRACE_O_*` word in its `data`; `INTERRUPT` records a request the tracee honours at
its next syscall boundary. Four answers separate it from `ATTACH`, and all four are
pinned by `t-ptrace-seize`:

| question | Linux | this kernel |
|---|---|---|
| `SEIZE(child, 0)` then `waitpid(WNOHANG)` | `0` — still running | `0` |
| `INTERRUPT` on a seized, running tracee | `status >> 8 == 0x8005` (`SIGTRAP` + `EVENT_STOP`) | same |
| `INTERRUPT` on an `ATTACH`ed tracee | `EIO` (5) | `EIO` |
| `SEIZE(child, 0x200)` (unknown option bit) | `EIO`, no link written | `EIO`, no link written |
| a child auto-attached from a seized parent | born stopped with `SIGTRAP` + `EVENT_STOP` | same |

That last row was measured separately (`seize2/t.c`): the fork event stop is
`SIGTRAP` + `PTRACE_EVENT_FORK` with `GETEVENTMSG` naming the child, and the child's
own initial stop is `SIGTRAP` + `PTRACE_EVENT_STOP` — not the `SIGSTOP` an
`ATTACH`-era child gets.

**`PTRACE_LISTEN` — landed 2026-11-18**, with the whole shape measured first
(`build/scratch/listen/l{1,2,3,4,5,6}.c`, Linux 7.3.4). The earlier note here had
two rows wrong, and the probes that produced it are kept because the correction is
the interesting part:

| question | Linux | this kernel |
|---|---|---|
| `LISTEN` on a **running** seized tracee | `ESRCH` | `ESRCH` |
| `LISTEN` on a `SEIZE`+`INTERRUPT` stop | **`0`** | `0` |
| `LISTEN` on an `ATTACH`ed tracee | `EIO` | `EIO` |
| `LISTEN` on a **group-stopped** seized tracee | `0` | `0` |
| a stop signal **while listening** | **held**, `waitpid(WNOHANG)` stays 0 | held |
| `SIGCONT` while listening | ends it: tracer sees `0x8005`, tracee **still `t`** | same |
| the seized group stop itself | `0x8013` = `SIGSTOP` + `PTRACE_EVENT_STOP` | same |
| an `ATTACH`ed tracee's group stop | plain `0x13` | same |

**Three readings were wrong on the way to this table, and each was corrected by
a probe rather than by re-reading the code** - which is the point of writing the
table down at all:

1. the first draft said a stop signal arriving during a listen is *reported*;
   three `SIGSTOP`s and a `SIGTERM` all leave `waitpid(WNOHANG)` at 0 and
   `/proc/<pid>/stat` at `t` (`l6.c`, `l10.c`). It is **held**.
2. it said `SIGCONT` *wakes* the listening tracee; the tracer is told
   (`0x8005`) and the tracee stays `t` (`l10.c`).
3. it said `LISTEN` on an `INTERRUPT` stop is `EIO`, and that a running tracee's
   is `EIO` too. Both wrong: the interrupt stop is **served** and a running one
   is `ESRCH` (`l7.c`, `l8.c`). Only an `ATTACH`ed tracee is `EIO`.

One shape is still **not measured, and is named rather than guessed**: after a
`LISTEN` on an *interrupt* stop, a later `SIGSTOP` never arrives at the tracer at
all (`l9.c` blocks indefinitely where the group-stop case reports at once), and
neither does a second `LISTEN` answer cleanly (`ESRCH` after `SIGSTOP`). What
Linux is doing there needs a probe that does not block to find out, and this
kernel's answer - the stop simply stays a listening stop - is the shape the
measured rows require. Closing it is its own item.

What this needed in the kernel, and it is small because the machinery was there:

- `Stop` grew `listening`, and a seized tracee's **group stop** reports
  `PTRACE_EVENT_STOP` (the delivery stop that precedes it stays a plain `0x13` -
  the distinction is "the program was stopped" versus "the tracer asked to be
  told", and only the second is the pair's own event stop).
- `sched::listen_traced` marks the stop; the three refusals are answered there
  because they are questions about two records the syscall layer already holds
  (the link's `seized` bit and the stop's `group` bit).
- `SIGCONT` on a listening stop does **not** clear it: it rewrites the stop as an
  event stop and wakes the tracer, which is exactly what `l6.c` shows.

Pinned by five new claims in `t-ptrace-seize` (the running-`EIO`, the delivery
stop, the group stop's `0x8013`, the held-while-listening signal, and `SIGCONT`'s
`0x8005`), green on both arms. Counterfactual: making the request answer `EIO`
unconditionally reddens exactly `LISTEN of a group-stopped seized tracee is
served (-1)` and leaves the other claims green.

**Why the item was first**: Chromium treats its crash handler as load-bearing. A
browser whose crashpad cannot attach is not required to abort, but it logs a storm
and loses its crash reporting; and on a kernel where the *renderer* crashes the
crashpad failure is what turns a recoverable renderer death into an unexplained
hang. What the fix does **not** do is make Chromium render — the stall in §3j-§3q
survived it, and that is why the next instrument is the `CHECK` text.

## 3s. The trap is now self-locating — and it lands in Chromium's own crash stub

§3r added the `in <module>[<offset>,<base>+<len>]` clause to the `traps:` line and
said the next reproduction would read straight out of it. It did, on the first
boot: the image's ESP was refreshed with the current loader/kernel first (§7.9v's
trap), and the run produced

```
traps: CacheThread_Blo[741] breakpoint ip:7e0db94f96cc sp:7ea183f71580 error:0 \
       in /usr/lib/chromium/chromium[8c4f6cc,7e0db08aa000+ee39000]
```

That offset is a **mapping** offset, and the mapping is the executable segment's
range. With the file in hand (`debugfs -R dump` of the image, then `readelf -lW`:
the executable LOAD is `p_offset 0x1e64304`, `p_vaddr 0x1e65304`, aligned down to
`0x1e64000`) the trap's file offset is `0x1e64000 + 0x8c4f6cc = 0xaab36cc`, and
the bytes there are **`cc 0f 0b`** - exactly Chromium's `IMMEDIATE_CRASH()`.

So the crash is Chromium's own `CHECK`, confirmed at the instruction rather than
inferred from registers, and the register reading in §3q was right. The block
around it is the shape a `LOG`-then-crash takes: a call, a branch on its answer,
then `mov ecx, 2` / another call / `jmp` back / `int3` / `ud2`.

**The kernel's part is now measured, not argued**: the fault is a breakpoint at a
mapped, present, executable page of the program's own image. Nothing was refused,
no address was wrong, and the module attribution the crash handler needs works -
which is what §4c's columns bought.

**Two things this needed that are worth recording**:

1. The image had to carry the current kernel at all. `cargo xtask run` needs root
   and the loader embeds the kernel, so the installed image was booting the kernel
   it was built with. A root-free path now exists: the loader is built with
   `MPOS_KERNEL_ELF`, a 32 MiB FAT is made by `mkfs.fat`, and the repository's own
   FAT32 writer fills it (`xtask/src/bin/fat-populate.rs`) - then 262 144 sectors
   are `dd`'d over partition 1. The ESP carries exactly three files: the loader
   (kernel inside), `initramfs.tar` (the **installed** one,
   `build/initramfs.tar`, 8.4 MiB - not the 230 MiB test archive) and
   `mpos.cmdline` (`root=/dev/vda2 rootfstype=ext4 rootflags=rw console=ttyS0`).
   Verified by the wire rather than by mtime: the boot prints the 1216 KiB
   compressed / 5137 KiB loaded payload and reaches the disk's own Alpine
   userland.
2. The offset in the clause is worth reading against the **page-aligned**
   `p_offset`, not the raw one: the loader reports the range's own start, which
   is aligned out, so `p_offset` and `p_vaddr` differ by `0x1000` and only the
   aligned pair lands on the `int3`.

What is still **not** answered: *which* `CHECK` it is. The message is not in the
binary at that site - it is a `LOG`/`CHECK` with a runtime-built stream - so the
next instrument is a run whose crash handler can report the stream contents, i.e.
exactly what crashpad does and what `PTRACE_LISTEN`/the seized group-stop path
(§4) still stands in the way of.

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

## 4c. FIXED — the exec image reports the file it came from, and `.bss` is its own line

§4b's fact 2 said a fix needed "file-backed VMAs for `exec` images (a real
memory-management change) or an acceptance that `maps`-based unwinders cannot
resolve this kernel's images". The first reading was too strong, and measuring
before designing is what showed it.

**What Linux actually provides** (`build/scratch/execmap/{probe6,s1,t1,t2}.c`):
a program's segments are printed with **their own file offset, the file's device
and its inode**, and a segment whose `p_memsz` exceeds its `p_filesz` gets a
**second line** for the zero-filled tail - `rw-p 00000000 00:00 0 ` with a blank
name. The pages themselves are demand-paged, but that is not what the columns
describe: they describe **where the bytes came from**. That question has an
answer here too, and it does not require the paging to change.

So the fix is the columns, not the pager:

- `mm::elf::Source` grew `identity()` - default `None` (a `&[u8]` really has no
  file), and `vfs::InodeSource` answers its inode's device/inode through the
  same door the mapping objects use, so `maps` and `fstat` cannot disagree.
- `MappedRange` carries `file: Option<RangeFile>` (`offset`, `dev`, `inode`).
  Two runs merge only where the file offsets continue each other **and** the
  identity matches - otherwise a merged line would report an offset that is
  wrong for half of it, and a `.bss` tail would claim to be the file's.
- `segment_ranges` splits each segment at `p_filesz`: file bytes under the
  path, tail under no name and the zero triple.
- The private-copy `mmap` arm records the same three numbers for the range it
  fills, so a `MAP_PRIVATE` mapping of a file reports the file - Linux does the
  same (`pmap` and a crash reporter both read it that way).

**Pinned** by four new claims in `t-maps-identity`: the exec image has named
lines, every one reports its file's device and inode, at least one segment
offset is non-zero, and an unnamed range reports no file. Green on both arms
(x86_64 `pass=338`, aarch64 `pass=334`, kunit 42/0 each). Host tests pin the
merge rule in `mm::vma` (offsets continue / a gap / a file range beside a
file-less one).

**Counterfactual**: restoring `let source = None` reddens exactly the two
claims about the image's columns (`every exec-image line reports the file's
device and inode`, `and its segment offsets are the file's own (non-zero:
false)`) and leaves the rest green - so the pin measures this change and not a
neighbour.

**What is still not file-backed, and is now named rather than assumed**: the
pages are still filled at load time, so `mprotect(PROT_WRITE)` on the text does
not change the file, `MAP_SHARED` semantics do not apply to an image, and
`ETXTBSY` - which Linux raises for a write-open of a running executable
(`build/scratch/execmap/t1.c`: `O_WRONLY` and `O_RDWR` are `26 (Text file
busy)` while a copy runs, `truncate` the same, `unlink` succeeds) - is not
implemented here. That is the smaller half of Linux's answer and it is what a
boot loader's "replace the running binary" step would need; it is not on
Chromium's path, which is why it is named and not built.

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

1. **`PTRACE_SEIZE` / `PTRACE_INTERRUPT`** (Finding B) — **landed**, and re-measured
   on 2026-11-17: the option word, the two different refusals (`EIO` for an attached
   tracee, `ESRCH` for an untraced one) and `PTRACE_EVENT_STOP` are pinned by
   `t-ptrace-seize`. What remains of this item is `PTRACE_LISTEN`, which needs the
   seized group-stop path first (§4).
2. **`st_dev` in `Metadata`, and the maps device column** — **done** (§4b, defect 1):
   the column is the mount's `st_dev` now and `t-maps-identity` pins it. Finding C's
   *fact 2* is **done too** (§4c): the exec image's lines report their file's offset,
   device and inode, and its `.bss` tail is a separate file-less line. What remains of
   the family is the paging itself (the pages are still placed, not demand-paged) and
   `ETXTBSY`, both named in §4c and neither on Chromium's path.
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
