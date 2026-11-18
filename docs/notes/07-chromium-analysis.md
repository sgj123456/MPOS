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

## 4d. FIXED — `GETREGSET`'s `iov_len`, and a probe that refuted its own first reading

With `LISTEN` in and the image carrying the current kernel (§3s), the crashpad
stderr changed shape:

```
ptracer.cc:482  Unexpected registers size 0 != 216
ptracer.cc:569  ptrace: No such process (3)
```

The first line is exact and was the real defect: `PTRACE_GETREGSET` answered `0`
(served) while leaving the caller's `iov_len` at **0**, so a tracer read a
successful call as an empty register file. Measured on Linux 7.3.4
(`build/scratch/regsz/q.c`) - and the two register sets **disagree**, which is
why each row was taken rather than generalised:

| offered | Linux |
|---|---|
| `NT_PRSTATUS` 216 (exact) | `0`, `iov_len` 216 |
| `NT_PRSTATUS` 1024 (roomy) | `0`, `iov_len` **216** - the size moved |
| `NT_PRSTATUS` 100 (short) | `EINVAL` (22), buffer untouched |
| `NT_PRSTATUS` 0 | `0`, `iov_len` 0 - a query answered with nothing |
| `NT_PRFPREG` 1024 (roomy) | `0`, `iov_len` **512** |
| `NT_PRFPREG` 128 (short) | `0`, `iov_len` 128 - **truncated**, not refused |
| an unknown note type | `EINVAL` |

The kernel now writes the moved size back on both sets, truncates `NT_PRFPREG`
to what the caller offered, refuses a short `NT_PRSTATUS` with `EINVAL` (it had
been `EIO`), and answers an unknown note type with `EINVAL` rather than `EIO` -
`EIO` being the answer a tracer reads as "this kernel has no ptrace at all".
Pinned by four new claims in `t-ptrace-fpregset`; counterfactual restores
`moved = 0` and reddens exactly the two `NT_PRSTATUS` claims (`len 0`,
`EIO`) and leaves the `NT_PRFPREG` ones green.

**After the fix, on the image**: `Unexpected registers size` is gone from the
stderr entirely, and so is every `ptracer.cc` line except two `ESRCH`s for a
process that had already exited.

**And then the probe refuted its own first reading, which is the part worth
recording.** The next crashpad line was

```
directory_reader_posix.cc:43  opendir /proc/730/task: No such file or directory (2)
```

`730` was the `inotify_reader` thread that had just died in `libdbus`
(`inotify_reader[730]: segfault …`). A shell fixture appeared to show that this
kernel drops a killed child's `/proc/<pid>` **at exit**, which would have been a
real divergence from Linux's `__unhash_process` (it unhooks a task at exit
*unless it leads the group*, so a zombie keeps its directory until it is
collected). It does not: **the shell was reaping its own background child**, so
every reading was of a *collected* process - `ENOENT` on Linux too. A probe that
never calls `waitpid` (`build/scratch/zprobe/{z,y}.c`) shows the truth:

- a killed, unreaped child keeps `/proc/<pid>/stat` at `state=Z`, and its
  `/proc/<pid>/task` stays readable;
- the same holds when the reader is a **different process** (the crashpad
  shape), with the dead group's tids still listed;
- and both disappear at the `waitpid`, which is the other side of the rule.

So the `ENOENT` crashpad logs is about a process that had already been *reaped*
by the time the handler looked - Linux answers `ENOENT` there too. No kernel
defect, and the claims that say so are now pinned in `t-tid-dir` (four of them:
the zombie's state, its directory, its `task/` listing, and both going away at
the reap).

## 4e. An observed defect that is **not** Chromium's: a static glibc binary cannot return from `main`

Chasing §3s's `CHECK` down, a smaller and much sharper fact fell out, and it is
written down here because it changes what "the kernel's part of this chain is
clean" can mean.

**Reproduction, five boots, identical every time.** A statically linked glibc
program whose `main` simply returns dies with `SIGSEGV`:

```c
int main(void){ return 0; }          /* cc -static -O0 */
```
```
m[717]: segfault at 7dcde46043f0 ip 00007dcde46043f0 sp 000000003fc53a08 error 14
rc=139
```

- the same program calling `_exit(0)` exits **0**, and one calling
  `syscall(SYS_exit_group, 0)` exits **0** - so the kernel's exit path is fine;
- `atexit` handlers **do** run before the fault (a handler that writes
  `HANDLER RAN` prints, then the crash);
- it is not the binary: `gcc -static -O0/-O2` and a bare `int main(void){return 0;}`
  all crash, at `rip == fault address` with the page reported `NOT MAPPED`;
- the fault is a **`call *%rax`** in glibc's `__run_exit_handlers`
  (`0x406552` in the fixture), whose `%rax` was just unmangled by
  `ror $0x11` over `__pointer_chk_guard_local` - glibc's PTR_MANGLE pair;
- that guard is derived from the `AT_RANDOM` auxv bytes, and `AT_RANDOM` itself
  is **correct here**: measured in the guest, it reads `0x3fe87ff0` for
  `sp = 0x3fe87a08`, i.e. exactly 16 bytes below the stack top, inside the
  argument block, holding fresh random bytes.

So the defect is **downstream of `AT_RANDOM`** and upstream of the exit: the
mangled pointer does not survive its round trip. That much is measured; *why* is
not, and no fix is claimed here. The two candidates a later pass should separate
are the `fs:`-relative read of the guard versus the static global glibc actually
uses, and whether `__libc_start_main`'s `%fs:0x28` canary and the guard are being
written from the same 8 bytes when glibc expects two different offsets.

**Why this matters beyond the fixture**: every statically linked C program on
this machine ends in this fault - which includes anything a boot or a test would
build to be self-contained - and the failure is a `SIGSEGV` at exit, i.e. a
*successful* program reported as killed. It is also very possibly what a real
Chromium child was doing in §3s: the `#BP` there is `IMMEDIATE_CRASH`, but a
child that segfaults on its way out needs no `CHECK` at all to look like one.

**No pin yet, deliberately.** The claim would have to compile C inside the
guest, and the installed image has no compiler; a checked-in static binary would
pin this suite's build rather than the libc's exit path. The honest state is a
named gap with a five-run reproduction, recorded here, and the next step is a
kernel-side instrument rather than another userland probe - the guard's value at
`__libc_start_main` and at the unmangle, printed from a build that can see both.

## 4f. `PTRACE_SINGLESTEP` was a synonym for `SYSCALL`, and the static-exit bug is not PTR_MANGLE

§4e left two things open: why a static glibc program segfaults on its way out of
`main`, and how to inspect it. The instrument came first, and it was itself the
defect.

**`PTRACE_SINGLESTEP` ran no instruction.** `handle_ptrace` answered it exactly as
`PTRACE_SYSCALL` - the comment said so plainly ("this kernel has no single-step
trap, and a tracer that asked for one gets stops at the next boundaries rather
than none at all"). That is not the same contract: a tracee sitting on a
`syscall` instruction never advances, so a probe built on it reported the **same
`rip` for 300 000 steps** - the first trace produced
`rip range [0x41c509..0x41c509]`, one address, and every conclusion drawn from it
was worthless. It is worth stating exactly why that mattered: I had already
written down `_dl_random == 0` as a finding before noticing the probe could not
move.

**Now it steps, and the shape is the machine's.** `Contexts::STEP_BIT` (a `hal`
associated constant, so both ports answer and neither arch is named in the
kernel) carries x86_64's `(EFLAGS slot, 1<<8)` and aarch64's `(SPSR slot, 1<<21)`;
`resume_traced_with` sets or clears that bit on whichever register file the stop
saved; a `#DB` vector was installed (`trap_db`, signed `SIGTRAP` through a new
`FaultKind::Breakpoint` row); and `rust_trap` turns vector 1 into the tracer's
stop through a new `Kernel::step_trap` seam rather than into a signal. A resume
that is **not** a step clears the bit, or a tracee would trap for ever.

**What the working instrument then showed** (`build/scratch/zprobe/bp{7,8,9}.c`,
frame converted through `capture_trap_regs`/`restore_trap_regs` - the `TrapRegs`
block is *not* the ABI register file, its word 15 is `rip` where `uregs` keeps
`orig_rax`, and the first cast-based version faulted on step two):

- the step trace is real: 107 steps to `_dl_aux_init`, then on to the faulting
  call at `0x406552`;
- `__pointer_chk_guard_local` is **0 all the way**, and so are the four words of
  the `__exit_funcs` node the call dispatches through
  (`__exit_funcs = 0x4bb360` is inside `.bss`, `0x4baa40`, which is NOBITS -
  everything there starts zero and glibc fills it at run time);
- and **no `.init_array` constructor ever ran**: across the whole trace the
  instruction pointer never left `[0x402e40..0x41c560]`, so the table at
  `0x4b5160` was never entered.

So §4e's PTR_MANGLE reading was **a symptom, not the cause**: with the
constructors skipped, the guard and the exit-func list are still zero, and the
unmangled "pointer" is whatever those zeroes produce. The real question is why
`__libc_start_main` never reaches its `.init_array` loop - and that is where the
next pass starts, with an instrument that now works.

**Unpinned, and named**: `PTRACE_SINGLESTEP` has no claim yet. A pin needs a
tracee whose instruction stream the test can predict - a three-instruction
fixture, stepping and reading `rip` after each - and that fixture has to run on
both machines, whose step bits differ. The mechanism is in and the full gate is
green with it; the contract is not yet asserted.

## 4g. Two probe bugs, one real finding, and one still-open stop

§4f said "no `.init_array` constructor ever runs" from a trace, and §4e before it
said the pointer guard was zero. **Both readings were artifacts of the probes**,
and this section is the correction, because in this repository a wrong reading
written down is worse than no reading.

1. **The loop's break condition was wrong.** `bp9`/`bp10` broke on
   `rip >= 0x406552`, but `_dl_aux_init` lives at `0x41c560` - **above** that
   address - so the "we reached the faulting call" line fired on the *first*
   entry into `_dl_aux_init`, after 107 steps, and every "visited: all false"
   reading was the probe stopping early rather than the program skipping code.
2. **The reads happened before the instruction executed.** A single-step stop
   lands *on* the instruction, so reading `_dl_random` at `0x41c636` gives the
   value from before the store. Read at the *next* instruction (`0x41c63d`) it
   is right:

```
i=345 at _dl_aux_init store 0x41c636: rdx=0x3ff92ff0  _dl_random(after)=0
i=346 AFTER the store (0x41c63d): _dl_random=0x3ff92ff0
```

So **`_dl_random` is written correctly**, with `AT_RANDOM`'s own address, and
§4e's "the guard never got filled" was measuring nothing. What survives from
§4e is the *observation* - a static glibc program segfaults returning from
`main` (five boots, `rc=139`, `int main(void){return 0;}` is enough) - and the
`__exit_funcs` node reading four zero words, which is real but is a
consequence to be explained, not a cause.

**A real defect the probing did find** (and this one is in the kernel): a
`ptrace` stop whose `Stop::delivering` is `None` leaves the machine's pending
`SIGTRAP` in the task's signal set, so the tracer's *next* resume delivers it and
the tracee dies where the tracer meant to continue it. Measured as every probe
ending `ended sig=5` on its first `PTRACE_CONT`. `step_trap` now sets
`delivering: Some(SIGTRAP)`; the **exec** stop after `PTRACE_TRACEME` still does
not, which is why the same symptom survives for a tracer that plants a
breakpoint in a freshly-exec'd child. That is the next thing to fix, and it has
nothing to do with Chromium.

**Where the static-exit question stands**: the instrument now works (single-step
is real, and reads must be taken *after* the instruction), `_dl_random` is
confirmed correct, and the faulting `call *%rax` in `__run_exit_handlers`
dispatches through an `__exit_funcs` node that is still zero at that moment. The
next pass should read the guard and the node **at `0x406552` with after-execution
timing**, which the two bugs above prevented every earlier attempt from doing.

## 4h. A planted breakpoint is the tracer's stop, and the exit fault is located

Two more `ptrace` defects fell out of making the instrument honest, and with both
fixed the static-exit question is finally narrowed to a **specific instruction
range** rather than a guess.

**1. A planted `int3` killed the tracee.** `PTRACE_POKETEXT` writes `0xcc` over an
instruction; the tracee executes it; this kernel signed it `SIGTRAP` and the
**default disposition killed the process** - so a tracer could plant a breakpoint
and then watch its subject die on the first resume. Linux instead parks the
tracee and reports the stop to the tracer, and it rewinds `rip` by one so
`PTRACE_GETREGS` answers the byte the tracer wrote (which is what lets a tracer
restore the original instruction and continue). Both halves are now there.

**The rewind is the tracer's, not the program's** - and getting that wrong cost a
gate run: applying it unconditionally put a program's *own* `int3` back on itself
for ever, and `t-trap-at-trap` hung at
`traps: … breakpoint ip:43abc4` until its budget ran out (338/338 → `FAIL
t-trap-at-trap timeout=120s`). It is gated on `tracing_self()` now, and the gate
is green again. The lesson is the repository's own: a change to how a trap is
*signed* is a change to every program that handles `SIGTRAP`, not only to the
tracer's path.

**2. Where the static-exit fault actually is.** With breakpoints that work, three
stops bracket the exit (`build/scratch/zprobe/near.c`,
`build/scratch/zprobe/final.c`):

```
planted; guard at start = 0
AT 406552 (the faulting `call *%rax`): guard=0x9740cec rax=0x402f80
  __exit_funcs=0x4bb360  node: [0]=0 [1]=0x1 [2]=0x4 [3]=0x6dfcce880000
HIT after call_fini at 0x406554
HIT loop head       at 0x40645b
DIED sig=11
```

Read what that overturns: at `0x406552` the guard is **non-zero**
(`0x9740cec`), `rax` is **`0x402f80 = call_fini`** - a real function, unmangled
correctly - and the `__exit_funcs` node **has content**. So §4e's "the guard is
never filled" and §4f's "no constructor runs" are both dead: the machinery works
up to and including `call_fini`. The fault is **after** it: `call_fini` returns
cleanly, the loop head at `0x40645b` runs, and the `jne 0x4063d0` at `0x406465`
takes a **second trip** through the handler loop - because `__new_exitfn_called`
changed, i.e. something registered an atexit function during `call_fini` - and
that second trip dispatches a pointer that faults.

That is the next measurement, and it is a narrow one: read the node the second
trip dispatches from, at its own `call *%rax`, with the timing lesson of §4g
applied (read *after* the instruction).

## 4i. The second trip dispatches `ef_at`, and the probe's entry arithmetic was wrong

§4h ended with a narrow question: read the node that the second trip through
`__run_exit_handlers` dispatches from. Measured
(`build/scratch/zprobe/{trip2,rd}.c`, breakpoints at `0x4063ff` and `0x406427`):

```
DISPATCH trip 1: r15(node)=0x4bb360 rax(count)=0x2
  after shl/add: rax=0x4bb380 (index*32 + node)
  entry=0x4bb380  [0]=0 [1]=0 type(+0x10)=0x4 fn(+0x18)=0x998f35090046cc7d
DIED sig=11
```

Two facts, and one of them is mine, not the kernel's:

1. **The dispatched entry is flavor `4`** - `ef_at`, an `atexit`-registered
   function - and it is dispatched through `0x406520`, which takes `+0x18`, does
   `ror $0x11` over the guard and calls it. That is the branch that faults.
2. **The value at `+0x18` is not a pointer.** `0x998f35090046cc7d` unmangles to
   `0x663eccc793f08ccf` under the guard read at that moment, and its low 24 bits
   (`0x46cc7d`) land inside `.text` - in `__mpn_impn_sqr_n_basecase`, a GMP
   routine with nothing to do with exiting. That is the signature of reading a
   **struct field that is not the one I think it is**, not of a corrupted pointer.

**And that is what it was.** glibc's `struct exit_function` is
`{ flavor; fn; ... }` laid out from `fl_entry`, and the entries of one
`exit_function_list` begin at `node + 0x20` - while the dispatch arithmetic is
`node + index*0x20`. With `count = 2` the code's last index is 1, so it reads
`node + 0x20`, **entry 0**. The probe read `+0x10`/`+0x18` off *that* address
believing it was entry 1's `flavor`/`fn` pair, so it reported entry 0's
neighbouring words as if they were a mangled function pointer. A wrong field
offset in the instrument, for the fourth time in this investigation.

**So the honest state of the static-exit question**: the machinery up to and
including `call_fini` is confirmed working (guard non-zero and correct there,
`rax = call_fini`), the second trip is confirmed to dispatch a flavor-`4` entry
through the guard-mangling path, and **the value it actually dispatches has not
yet been read correctly**. The next probe must compute the entry address the way
glibc does (`node + index*0x20`, where index is `count - 1` and entry 0 lives at
`node + 0x20`) and read `flavor`/`fn` from *that*, or better, read the pointer
out of `%rax` at `0x406552` after the `ror`/`xor` pair has run - the register,
not a guessed struct offset.

**What this section is for**: four rounds of this investigation have now produced
a wrong reading from a probe (the `>=` break condition, reads taken before the
instruction, and twice a wrong struct offset). The pattern is worth naming: every
one was found by asking *what would this probe print if my assumption about the
layout were wrong*, and every one cost a round. The instrument is the thing to
verify first.

## 4j. `call_fini` returns cleanly; the fault is one instruction into the next dispatch

§4i's probe misread the entry, so this pass read the **register** instead of a
guessed struct offset, and then walked the whole exit with breakpoints.

**Everything through `call_fini` is correct** (`build/scratch/zprobe/{reg,reg2,fin,ret}.c`):

```
CALL #1: rax=0x402f80 (guard non-zero)          <- call_fini, unmangled correctly
HIT fini loop call      at 0x402fb0  (rbx=0x2 r12=0x4b5168 rax=0x4b5170)
HIT __do_global_dtors_aux at 0x402f00
HIT deregister_tm_clones  at 0x402e90
HIT __deregister_frame_info at 0x47cb00
HIT _fini               at 0x47f630
AT _fini's ret: rsp=0x3fea6a08  [rsp]=0x406554      <- the right return address
  after the ret: rip=0x406554                        <- and it lands there
```

So the whole `.fini_array` walk runs, `__deregister_frame_info` is called with
its `.eh_frame` argument, `_fini` returns to exactly `0x406554`, and `.fini_array`
holds two valid code pointers (`0x402f00`, `0x402dc0`). Nothing here is broken.

**The fault is the next dispatch, and it is bracketed to one instruction.** From
`0x406554` the single-step trace runs cleanly to `0x4063f8` and stops:

```
step  9: rip=0x4063e9  rax=0x1     <- sub $1
step 10: rip=0x4063ed  rax=0       <- count is now 0
step 11: rip=0x4063f4
step 12: rip=0x4063f8  rax=0       <- shl $0x5 of 0
   (no step 13: the process dies here)
```

`shl $0x5` of 0 is 0, `add %r15` makes the entry address **`0x4bb360` - the node's
own header**, not an entry - and `0x4063ff` then reads `+0x10` (`0x4bb370`, inside
`.bss`, which is mapped and reads zero). A zero flavor matches none of the three
`cmp`s, so `0x40641b`'s `jne` sends control back to `0x4063dc`, which reads
`0x8(%r15)` - the count - and loops. **That is where the run ends**, and the next
pass must read the count and the entry address at `0x4063dc`/`0x4063e9` on the
*second* visit with breakpoints (not single-step, which is far too slow here).

**The question to ask next** is therefore narrow and testable: is `count` at
`node + 0x8` actually decrementing, or is `exited`/`ef_free` re-arming it? A node
whose count never reaches zero would walk its header for ever, and each pass
reads a "flavor" out of whatever neighbours `+0x10` - which is exactly the shape
of the fault.

**What is now ruled out, with readings**: the guard (correct, non-zero, at the
call), pointer mangling (unmangles to a real function), `AT_RANDOM`
(`_dl_random` = its address), the `.fini_array` walk and `_fini`'s return, and
and `__exit_funcs` being empty (the shape of one node holds a flavor-`4` entry;
its `count`/`flavor` were read with the wrong offsets in §4i, so what that node
*contains* is not yet claimed). §4e's and §4f's earlier stories are fully
retracted.

## 4k. The dispatches point at `call_fini` twice, and the second one never returns

§4j left one question: is the node's `count` decrementing, and what does each
dispatch actually call. Read from the register, at the mangle pair itself
(`build/scratch/zprobe/{same,first,dump,cnt}.c`):

```
mangle site: rax(mangled)=0x1b185b1a3aed5ed4 guard=0xaf6a0d8c2dcd32f6
             -> unmangled=0x402f80     in .text? YES
   after ror+xor: rax=0x402f80
```

**`0x402f80` is `call_fini` again.** So the entry the exit loop dispatches is
glibc's own `__cxa_atexit(call_fini, dso, …)` registration - the one that runs
the *finalisers* - and what happens is:

```
LOOP count=0x2
  CALL call_fini #1
  DIED (the RET is never reached)
```

**The node's layout, now read whole rather than inferred** (`dump.c`, twelve words
from the node):

```
node=0x4bb360  count=0x1  rax(entry)=0x4bb380
  +0x08 = 0x1            count
  +0x10 = 0x4            entry 0 flavor  (ef_at)
  +0x18 = 0xbd39...2594  entry 0 fn      (mangled: unmangles to call_fini)
  +0x20 = 0
  +0x28 = 0
  +0x30 = 0x4            entry 1 flavor
  +0x38 = 0xbd39...2594  entry 1 fn      (call_fini too)
```

So the header is `next@0x0`, `count@0x8`, and the entries begin at **`+0x10`**,
each `0x20` bytes - the dispatch arithmetic `node + index*0x20 + 0x10` is
**correct**, and §4i's "the probe misread the entry" was right about the probe and
wrong about the code. Both flavours are `4` and both functions are `call_fini`.

**That is a recursion, and it is the fault.** `call_fini` walks `.fini_array` →
`__do_global_dtors_aux` → `deregister_tm_clones` → `__deregister_frame_info` →
`_fini` → returns into `__run_exit_handlers`, which dispatches the *next* entry -
also `call_fini` - and re-enters it. On Linux this terminates; here the second
call never returns, and the process dies with `SIGSEGV` at the depth the machine
can still reach.

**What is NOT yet established** is which half is wrong on this machine: whether
`call_fini` should be registered twice at all (a `DT_NEEDED`/`__cxa_finalize`
question), or whether the *first* invocation does not reach the state that makes
the second a no-op. The instrument to tell them apart is a breakpoint on
`0x406554` (the return) with the call site left **un**patched, which the last
probe could not do because it had patched both - the run above `DIED` with the
`RET` breakpoint never hit, and an `int3` planted over the `call *%rax` is itself
part of that picture.

**One more probe defect, recorded so it is not repeated**: patching both the call
site and its return address makes "did it return" unanswerable, because the call
site's `int3` replaces the call. Every breakpoint experiment that asks a
*control-flow* question needs one patch at a time.

## 4l. `call_fini` returns, the guard byte works, and the fault is inside the first `dtors` call

§4k's last probe patched a call site *and* its return, which makes "did it return"
unanswerable. With **one patch at a time** (`build/scratch/zprobe/{one,w,two}.c`)
the answers are clean:

```
RETURN #1 from call_fini, sp=0x3ffd5a10  count now=0x1
DIED sig=11 after 1 return(s)
```

So `call_fini` **does** return, and the count is 1 by then - the entry was
consumed before the call, as `0x4063f4` stores it. The node is decrementing
correctly.

**The `completed.1` guard works, and §4k's recursion story is wrong.** Read
immediately after its store:

```
AFTER the movb: completed.1=0x1
after pop/ret:  completed.1=0x1
```

The byte is set and it persists, so a second `call_fini` would find it and take
`__do_global_dtors_aux`'s early `ret` at `0x402f38`. A node whose both entries are
`call_fini` is therefore **not** an infinite recursion - and §4k's model of it is
retracted here.

**Where it actually dies.** Breakpoints at `call_fini`'s entry, the
`.fini_array` call, and `__do_global_dtors_aux`'s entry:

```
call_fini ENTRY          sp=0x3f948a08 rbx=0x2 completed.1=0
fini_array call          sp=0x3f9489f0 rbx=0x2 completed.1=0
__do_global_dtors_aux    sp=0x3f9489e8 rbx=0x1 completed.1=0
DIED sig=11
```

Three hits, then death - inside `__do_global_dtors_aux`'s **first** invocation,
after `completed.1` was read as 0 (correct for the first pass). What that body
does, in order: `cmpb completed.1` (0, so fall through) → `deregister_tm_clones`
(measured safe: `0x4baa28 == 0x4baa28` so it takes its `ret` immediately) →
`cmpq $0, 0x4b8f50` → **if non-zero**, `mov $0x49c178,%edi` and
`call __deregister_frame_info`. That function reads `[0x49c178]` (inside
`.eh_frame`) and, when the word is non-zero, tail-jumps into
`__deregister_frame_info_bases` - the one remaining branch that has not been
measured.

**So the next measurement is one read**: `[0x49c178]` at that moment, and then
whether `__deregister_frame_info_bases` is entered. Both are a single breakpoint
each (`0x47cb09` and `0x47bf70`), and the answer decides between "the frame-info
list is malformed" and "the call itself is refused".

**Probe discipline, now written into the method**: one patch per question. Every
run in this section that mixed a call-site patch with a return-site patch could
not answer the control-flow question it was built for, and one of them produced
the wrong "it never returns" reading that §4k was written on.

## 4m. The frame-info registration is called and does not land - the defect is named

§4l asked two questions and both are answered, which closes the search for the
static-exit fault's *location*:

```
AT the [rdi] read: rdi=0x49c178  [rdi]=0x5c0000001c
ENTERED __deregister_frame_info_bases (rdi=0x49c178)
```

So the `.eh_frame` word is a well-formed CIE length (`0x5c0000001c`), not a
zeroed hole, and the deregister path **is** entered. Inside it, the first act is
`btree_remove(&registered_objects, 0x49c178)` at `0x47bf83`, and
`registered_objects` lives at `0x4c0270` - inside `.bss`, which starts all-zero.

**And that is the defect, measured:**

```
at exec stop: GOT[0x4b8fc8] = 0x47c990
frame_dummy:  GOT[0x4b8fc8] = 0x47c990  -> WILL REGISTER
after frame_dummy: registered_objects head = 0
```

`frame_dummy` is `.init_array`'s first entry and its whole job is
`__register_frame_info(0x49c178, &registered_objects_node)`; its gate is the GOT
slot at `0x4b8fc8`, which holds the function's address (`0x47c990`), so the branch
is **taken** and the call **is made**. Yet the tree head is still zero afterwards.

So the chain is: the registration call runs and leaves the btree empty → the
finaliser's `btree_remove` walks a tree that was never populated → `get_pc_range`
on a null/absent node → `SIGSEGV` in `__deregister_frame_info_bases`. The
breakpoint at `0x47bf70` proved the entry; the tree being empty is what the
death follows from.

**Why `__register_frame_info` does not land is now the single open question**, and
it is narrow: whether the call is entered at all (a breakpoint at `0x47c990`
answers it), and whether its internal `btree_insert` is reached. `0x47c990`'s
first acts are a NULL/zero-length guard on `%rdi` (`test %rdi,%rdi`, `mov (%rdi)`
= `0x5c0000001c`, so non-zero - it does not take the early `ret`) and then the
node initialisation into `(%rsi)`. A breakpoint on the insert call is the next
measurement.

**What is now established beyond doubt about this defect**: it is not `AT_RANDOM`,
not pointer mangling, not `_dl_random`, not `.fini_array` contents, not `_fini`'s
return, not infinite recursion, and not the `.eh_frame` data being malformed. It
is a **registration that does not register**, and every earlier story in §4e-§4k
was a symptom read one layer too high.

**And it is very plausibly not a libc problem at all.** A `.bss` write that does
not appear to stick is the shape this repository has been bitten by before
(`docs/notes/` §9.1: a `static` with a zero initializer and no writer lands in
`.rodata`, and a write to it faults or is lost). `registered_objects` is
`.bss`-resident and starts zero, and `0x4c0270` sits **past** the point where the
loader's `.bss` mapping is known to be correct for this image - the exact
question a static binary with a 0x58e8-byte `.bss` at the end of its last LOAD
would ask of a kernel that maps `.memsz` correctly or does not.

That is the next thing to test, and it is a kernel measurement, not a libc one:
write a byte into the last page of this image's `.bss` from userland and read it
back.

## 4n. `.bss` writes are fine, the registration works, and the fault is the second `btree_insert`

Three hypotheses from §4m fell in one pass, each to a reading.

**`.bss` is not the problem.** A probe that writes every page of a 1 MiB `.bss`
and reads it back (`build/scratch/zprobe/bss.c`):

```
__bss_start=0x4bb000 _end=0x5c1888 span=1075336 bytes
page writes: 263 pages, 0 did not stick, first at +-1
last byte 0x5c1887: wrote 0xa5, read 0xa5
```

263 pages written and read back, the last byte included. The kernel maps
`p_memsz` correctly and a `.bss` write sticks. §4m's "`.bss` write that does not
stick" is retracted.

**The frame registration does work.** §4m read `registered_objects` after
`frame_dummy` and saw zero; that was a **timing artifact of 60 single steps**,
which do not reach the insert. With breakpoints at the insert itself:

```
register_pc_range_for_object  head=0
btree_insert                  head=0
after btree_insert            head=0x4c2160      <- the tree IS populated
```

**And `get_pc_range` returns a correct range.** Breaking it and its caller:

```
get_pc_range ENTRY rdi=0x4baa60
after get_pc_range: [rbp-0x20]=0x4010c0 [rbp-0x18]=0x47f62d
taking the registered_frames insert path
DIED sig=11
```

`0x4010c0..0x47f62d` is exactly this image's `.text`. So the unwind tables are
read correctly, the object is registered in the btree, and the pc-range is
extracted.

**The fault is the second `btree_insert`** - the one into `registered_frames` at
`0x47c057`, which runs only when the pc-range is non-empty (it is). That is the
last unmeasured step, and it is one breakpoint away: the entry is `0x47aab0` with
`%rdi = 0x4c0290`, and its first act is
`version_lock_lock_exclusive(&tree->lock)` on `0x4c02a0`, whose slow path falls
back to the global `version_lock_mutex` at `0x4c0300`.

**What is left is a lock question, and it is testable in one run**: whether
`registered_frames`' lock word is already held when that insert begins. Two
`btree_insert`s are made against **different** trees (`0x4c0270` and `0x4c0290`,
locks at `0x4c0280`/`0x4c02a0`), so a self-deadlock would mean they are not
different after all - or that the first insert leaves the *global* mutex held.
A single read of `[0x4c02a0]` and `[0x4c0300]` at `0x47aab0` answers it.

**Honest tally of this investigation**: six wrong readings have now been found
and retracted in the course of it (a `>=` break condition, reads taken before the
instruction, three wrong struct offsets, and a 60-step "registration does not
land"), plus one wrong model (infinite recursion). Every one was corrected by a
reading, not by argument, and each correction is recorded at the point it was
overturned rather than edited away. That is the only way this file stays usable
to whoever picks it up next.

## 4o. The last `.bss` page is filled with `0xff` at the moment of death

§4n ended on a lock question. The locks are not the fault, and the answer is
stranger: **a page of the process's own `.bss` is overwritten with `0xff` as it
dies.**

Measured with a sentinel, a value no code in this program writes
(`build/scratch/zprobe/{lock,ff,sentinel,lastlive,alive}.c`):

```
exec:              obj.root=0 obj.lock=0 frm.root=0
insert entry rdi=0x4c0270
after insert #1:   obj.root=0x4c2160 obj.lock=0x4 frm.root=0     <- all correct
DIED sig=11
death: obj.root=0xffffffffffffffff obj.lock=0xffffffffffffffff frm.root=0xffffffffffffffff
```

The btree insert **works** the first time: root set to a real node, lock word
`0x4` (a version, unheld - exactly what `version_lock_lock_exclusive` leaves). The
second insert at `0x47c057` is where it dies, and by then three separate globals -
`registered_objects` at `0x4c0270`, `registered_frames` at `0x4c0290`, the global
mutex at `0x4c0300` - are **all** `0xffffffffffffffff`.

**A sentinel proves it is an overwrite, not a stale read.** Stamping
`0x5e571e1100000001` into that page through `PTRACE_POKEDATA` and sampling at
every stop:

```
stamped [0x4c0300]=0x5e571e1100000001 (read back 0x5e571e1100000001)
stop 0: alive=1 sig=5 sentinel=0x5e571e1100000001
stop 1: alive=1 sig=5 sentinel=0x5e571e1100000001
stop 2: alive=0 sig=-1 sentinel=0xffffffffffffffff
```

Two live stops see the sentinel intact; the stop that reports the death sees the
whole page as `0xff`. And the same value is read **after the reap**, so it is not
a reading of a torn-down address space either (`lastlive.c`).

**Why this matters**: `0x4c0270..0x4c0328` is the **last page** of this image's
`.bss` (`_end = 0x4c0328`, page base `0x4c0000`), i.e. the tail of the last
`PT_LOAD`'s `p_memsz` - the part of the image with no file bytes behind it, which
the loader has to create by zero-filling. Everything the exit path needs lives on
that one page. A kernel that either mis-sizes the last page's frame, or reuses a
frame that is still mapped, would produce exactly this: correct for the whole run,
then the content of one page replaced by another owner's `0xff` fill at the end.

**`.bss` writes themselves are fine** (§4n: 263 pages written and read back, the
last byte included), so this is not a general mapping failure - it is specific to
what happens to that page around the death.

**The next measurement is a kernel one, and it is small**: whether a byte written
to the last page of the image's `.bss` survives a `SIGSEGV`-triggered death at all
when *no* tracer is attached - i.e. is `0xff` the *fault path's* doing (a frame
handed back and re-used while still mapped) or a property of this process only.
The fixture is a static binary that stamps its own `.bss` tail, faults
deliberately, and a parent with a core-dump-free `waitpid` that reads the page
through `/proc/<pid>/mem` before reaping. If the sentinel survives there and is
destroyed only under `ptrace`, the story is about stops; if it is destroyed in
both, it is about death.

**Honest status**: this is the first reading in the whole investigation that
points at the **kernel's own page lifetime** rather than at libc or at a probe.
It is not yet a diagnosis - the `0xff` fill has not been found in this kernel's
source (there is no `0xff` poison in `mm` or `child_exit`), so either it comes
from a path not yet read, or from a frame that was freed and re-filled by an
allocator this tree does share. That is what the next run decides.

## 4p. The `0xff` was the probe's own error return, and TLS is fine

§4o's headline - "the last `.bss` page is filled with `0xff` at the moment of
death" - is **wrong, and it was the probe.** Two readings settle it.

**`ptrace` returns `-1` with an errno when it cannot read, and my probe cast that
to an unsigned word.** With the return value honoured
(`build/scratch/zprobe/{strict,pre,notrace}.c`):

```
stop 0: alive=0 peek_ok=0 val=0 errno=3
```

The stop that reports the death is not a stop at all - the task is gone - and
`PTRACE_PEEKDATA` answers `ESRCH`, whose `-1` is `0xffffffffffffffff`. Every
`0xff...ff` in §4o is that sentinel read as data. Read through `/proc/<pid>/mem`
instead, the same page answers `0x5e571e1100000001` while alive and `EIO` at
death, never `0xff`.

**And `ESRCH` is Linux's own answer there**: measured on the host
(`build/scratch/zprobe/lx.c`), `open("/proc/<pid>/mem")` on an unreaped zombie is
`-1 ESRCH` on Linux 7.3.4 too.

**TLS is not the defect either.** §4p's momentary suspect was `%fs` being zero,
because `PTRACE_GETREGS` reported `fs_base=0`. That field is `uregs::FS_BASE`
(`src/syscall/linux/abi.rs:1273`), which **nothing in this kernel ever writes** -
`ARCH_SET_FS` stores the base on the task and calls `hal::set_thread_pointer`,
the register file's slot stays zero. A probe reading it reads a constant. The
kernel's TLS works:

```
tlsvar = 0x1234
after write = 0x5678
```

So `%fs` is live, per-thread variables have their own copy, and glibc's
`__libc_setup_tls` reaches `ARCH_SET_FS` successfully.

**Where that leaves the defect - and it is a real one.** With one patch at a time
and the return values honoured:

```
AT 47c057 (2nd insert): obj.root ok=1 0x4c2160  frm.root ok=1 0
btree_allocate_node: rdi(tree)=0x4c0270 rsi=0  tree+8(freelist)=0
  after malloc: rip=0x40b127 rax=0
```

`btree_allocate_node` calls `__libc_malloc(0x100)` and gets **NULL**, then writes
through it at `0x47a983` with no null check (`movq $0x1,(%rax)`) - a store to
address 0, which is the `SIGSEGV`. And `malloc(0x100)` is not exhausted:

```
allocated 4000 blocks of 0x100 and filled them
malloc(64MiB) = 0x7edf27a80010 errno=0
filled 64MiB ok
```

So the open question is now sharp and entirely inside `__libc_malloc`: why it
returns NULL for 0x100 bytes **at that moment** - during `frame_dummy`, i.e. from
inside `.init_array`, before `main` has run - when the same size succeeds
thousands of times afterwards. The next measurement is the malloc path's own
state at that call: `mp_` at `0x4b9120` and the TLS slot it dereferences at
`%fs-0x50`.

**Tally**: seven wrong probe readings retracted (a `>=` break condition, reads
taken before the instruction, three struct-offset guesses, a 60-step sampling, and
this `-1`-as-data), plus two wrong models (infinite recursion, `.bss` page
clobbered). The instrument has been the harder subject in every one of them.

## 4q. `malloc`'s arena resolves correctly, and the NULL is past the bin lookup

§4p left one question: why `__libc_malloc(0x100)` returns NULL during
`frame_dummy`, when the same size succeeds thousands of times afterwards. This
pass narrows where inside `malloc` it happens, and clears the parts that are fine.

**The `%fs`-relative arena lookup is correct.** Breaking right after the
`mov %fs:0xffffffffffffffb0,%rcx` at `0x40b154` (one patch, return values
honoured, `build/scratch/zprobe/{mp,fs2,f3}.c`):

```
fs-load gave rcx=0x486ec0  (arena) ok=1 [arena]=0  rax(sz)=0x1d
```

`0x486ec0` is a real address in this image's data, so the TLS base is live and the
arena pointer it dereferences is right. `[arena]` being 0 is the empty first bin,
which is the normal state before the first allocation carves into it.

**And `malloc`'s own globals are sane at that moment:**

```
malloc#1 size=0x6  mp_+0x78 ok=1 0x411  mp_ ok=1 0x20000  mp_+0x20 ok=1 0
malloc#2 size=0x10 mp_+0x78 ok=1 0x411  mp_ ok=1 0x20000  mp_+0x20 ok=1 0
```

`mp_` holds a real value (`0x20000`), the mmap threshold beside it is `0x411`, and
both small requests reach the size-class branch at `0x40b138` and complete. So the
heap's own bookkeeping is initialised by the time `frame_dummy` runs.

**What that leaves**: the failing `0x100` request does not die in the arena
lookup, the `mp_` reads, or the TLS base - it dies after the bin chain is walked
and a chunk is carved, or in the path taken when the bin is empty
(`0x40b196`'s early return, which the caller then dereferences). The distinction
matters: the first would be a chunk-header bug in glibc's view of this
machine's memory, the second would be `malloc` legitimately returning NULL - and
only the second is consistent with "it works later".

**One probe note, so the next pass does not repeat it**: reading `%rsi` one
instruction after `0x40b15d` returned `0x4844fe` on every call - a code address,
not a bin index - because the single-step window landed past the `lea` that sets
it. A stop's register set is only valid for the instruction the stop is *on*;
sampling one instruction later reads a different program point. That is the same
class as §4g's timing lesson, one layer finer.

**Tally**: eight wrong probe readings now, all from the instrument. The single
kernel-side fact that has survived every retraction is still §4p's: a static glibc
image reaches `btree_allocate_node`, gets NULL from `malloc(0x100)`, and stores
through it at `0x47a983` with no null check.

## 4r. `ifunc` is fine, `_exit` is fine, and the fault is a second `call *%rax`

§4q left `malloc` returning NULL unexplained. This pass chased two suspects and
**cleared both**, leaving one clean bracket around the fault.

**`IRELATIVE` is not the kernel's to resolve, and I implemented it wrongly before
noticing.** The image carries 22 `R_X86_64_IRELATIVE` records - glibc's `ifunc`
slots for `memcpy`, `strlen`, the `str*` family - and it is tempting to conclude
the loader must call the resolvers. It must not: this binary has **no
`PT_DYNAMIC`** and no dynamic section (verified with `readelf -lW`/`-dW`), so its
relocations live in `.rela.plt` and are applied by glibc's own `__libc_csu_irel`,
which is an ordinary `.init_array` constructor. A stub-based implementation was
written, measured, and **reverted** - and the measurement that proved it wrong is
the useful part:

```
at exec: GOT[0x4b8f40] ok=1 0x1000000
at main(): GOT ifunc slots:
  [0x4b8f40] = 0x4668b0      <- __wcslen_ifunc's choice
  [0x4b8f48] = 0x431950
  [0x4b8f80] = 0x40cdc0
  ...
```

Every slot is a real code address by the time `main` runs, and the value at exec
(`0x1000000`) is what the static linker pre-set. So `.init_array` **does** run -
`_IO_stdfiles_init` is its other entry and `printf` works - and `ifunc` resolves.
§4f's "no `.init_array` constructor ever runs" is dead for the second time, now
with a GOT dump instead of a step trace.

**And `_exit` is fine.** On the current kernel:

```
--- t (_exit(0)) ---        about to _exit(0)   rc=0
--- m (printf, return) ---  hello               segfault   rc=139
--- m3 (bare return) ---                        segfault   rc=139
```

A program that calls `_exit(0)` leaves cleanly; one that *returns from `main`*
crashes. That difference is entirely inside glibc's exit path, and it is the same
distinction §4e opened with - now measured on a kernel that has every ptrace fix
in this file.

**Where the fault is.** With one breakpoint at the mangle pair:

```
entry word(rax)=0x650f24e8c0d8cb72 guard=0x65b9328792344fec -> unmangled=0x402f80
  is that a code address? YES
```

The first dispatch of `__run_exit_handlers` resolves correctly to `call_fini`, so
the guard, the mangling and the node's first entry are all sound. The crash comes
**after** it, and the two runs put it in different places - `0x7e47...398` in one,
`0x3fcb4b98` (inside the user stack's page range) in another - which is the
signature of a value read from a structure being called as a function, with the
structure's contents varying run to run.

**So the bracket is now**: first dispatch correct → `call_fini` runs → something
in the second pass calls a value that is not code. The next measurement is the
same register read one dispatch later, which the `flame` probe already has the
shape for.

**Tally**: nine wrong readings or wrong turns retracted (the `>=` condition, reads
before the instruction, three struct-offset guesses, 60-step sampling, `-1` as
data, a wrong `.bss` clobber model, a wrong `IRELATIVE` implementation), and one
implementation written then reverted. The instrument - and my own structural
assumptions about it - has been the harder subject every time.

## 4s. The exit loop never advances: `count` reads 2 at every dispatch, `rsp` never moves

§4r bracketed the fault to "the second `call *%rax`". This pass reads the loop
directly, and the answer is not a bad pointer at all - it is a **loop that does
not advance**.

Two breakpoints, one before the count is decremented (`0x4063dc`) and one after
(`0x4063ff`), both printing the node's own count word at `0x4bb368`
(`build/scratch/zprobe/{cx,cnt2,deep}.c`):

```
BEFORE decrement: count=0x2 rsp=0x3ff69a10
BEFORE decrement: count=0x2 rsp=0x3ff69a10
BEFORE decrement: count=0x2 rsp=0x3ff69a10
   ... 12 times, identical
```

and, watching the dispatch itself:

```
dispatch  1: count 0x1  flavor=0x4  rsp=0x3f86aa10
dispatch  2: count 0x1  flavor=0x4  rsp=0x3f86aa10
   ... 12 times, identical
```

**Three things follow, and they rule out the previous story.**

1. **`count` is 2 before the decrement and 1 after it**, every time - so the
   store at `0x4063f4` *does* land, and by the next dispatch the word is **2
   again**. Something re-arms it between iterations. §4k's "the second call never
   returns" and §4r's "a structure value called as a function" were both reading
   the eventual death, not the cause.
2. **`rsp` is identical at every dispatch.** So this is not recursion and not
   stack growth - it is one loop spinning, which is why the crash takes as long
   as it does and why the earlier probes saw the same mangled value ten times.
3. **The same entry is dispatched every time** (`unmangled=0x402f80`, `call_fini`,
   identical mangled word and guard). The loop re-enters the *first* entry forever.

**And the word itself is writable.** A program that writes those exact fixed
addresses and reads them back gets what it wrote:

```
before: 0
after write #1: 0xdeadbeef
after write #2: 0
[0x4bb000] = 0
after write: 0x1234
rmw store: 0x1
```

So §4p's ".bss writes" and this pass's fixed-address writes both work; the count
being 2 again is not a lost store.

**What that leaves, precisely**: something between one dispatch and the next
writes 2 back into `node + 8`. The candidates are `__new_exitfn` (which is what a
`__cxa_atexit` during exit would call - and `call_fini` runs the `.fini_array`,
whose constructors could register), or the loop reading a **different node** each
time whose header happens to look identical. The next measurement distinguishes
them in one run: break at the store `0x4063f4` and at whatever writes `0x4bb368`,
and compare the two write sites' addresses.

**Tally**: ten wrong readings or wrong turns retracted. The instrument has been
the harder subject in every single one, and this pass is the first that narrowed
by *eliminating a cause* (the store lands) rather than by finding a new suspect.

## 4t. Two of §4s's three conclusions survive; the third was the probe again

§4s concluded that `count` reads 2 at every dispatch. Two of its three readings
hold up; the third was produced by a defect in the probe, and the difference
matters for where the next pass looks.

**What still holds, re-measured:**

```
after store: node=0x4bb360 count=0x1 rsp=0x3fb69a10
after store: node=0x4bb360 count=0x1 rsp=0x3fb69a10
```

The store at `0x4063f4` lands (`count` is 1 right after it), and `rsp` is the same
on both visits, so this is still **one loop, not recursion**.

**What does not hold: "the same store site is entered repeatedly" is my own
re-arming, not glibc's loop.** `0x4063f8` is not the target of any `jmp` or
conditional branch in this image - checked with `objdump` - so a repeated hit on
it cannot be the program's control flow. It is the probe: the restore path is

```c
unsigned long o; pk(c, 0x4063f8UL, &o);
ptrace(PTRACE_POKETEXT, c, (void*)0x4063f8UL, (void*)o);
r.rip = 0x4063f8UL; ptrace(PTRACE_SETREGS, c, 0, &r);
```

and `pk` returns **0 without writing `o`** when the peek fails, leaving `o`
uninitialised; `POKETEXT` then writes whatever was on the stack over the
instruction, `rip` is set back to the patched address, and the breakpoint fires
again. A breakpoint harness must treat a failed peek as fatal, not as a value.

**And the `r15`/`rax` readings in that run are therefore the probe's own state,**
not evidence about the loop.

**What the kernel side shows, and it is a real finding to keep**: `PTRACE_SETREGS`
is applied on resume - `park` ends with
`*regs = with_table(|t| t.task(id).trace.regs).unwrap_or(snapshot)`
(`src/sched/runtime/trace.rs:582`), so a tracer's edit to `rip` does reach the
frame. That is why the earlier, correctly-written probes (which restored, stepped
one instruction, and re-armed deliberately) produced sane traces, and why this one
did not.

**Where that leaves the search.** The surviving facts are: `_exit(0)` leaves
cleanly and `return` from `main` does not; the first `__run_exit_handlers`
dispatch resolves correctly to `call_fini`; and `.bss`/fixed-address writes stick.
The next probe must re-arm breakpoints by **single-stepping over the original
instruction** rather than by restoring a word whose read it did not verify - and
read the count at the `0x4063dc` head, which the §4s probe did reach and did show
as 2.

**Tally**: eleven. The instrument, not the kernel, has been the subject of ten of
them.

## 4u. A fault's signal never reaches the tracer - the gap that hid every crash from me

§4t ended with "the next probe must re-arm correctly". It can, now
(`build/scratch/zprobe/clean.c` is a harness whose peek failure is fatal and
whose re-arm is a single-step over the original instruction), and its first
reading is clean and short:

```
head 0: count ok=1 0x2  rsp=0x3f9bea10  r15=0x4bb360
DIED sig=11 after 1 heads
```

**One pass through the loop head, then death.** No repetition - so §4s's "count
reads 2 every dispatch" was indeed the probe, and the loop runs *once*. The fault
is inside `call_fini`, not in a loop.

**And the reason every attempt to look at it failed is a kernel gap, not a probe
one.** A tracer that asks for the fault never gets it:

```c
ptrace(PTRACE_CONT, c, 0, 0);          /* then wait */
/* -> ended sig=11, with NO stop reported */
```

Linux delivers a faulting signal to a tracer as a **signal-delivery-stop**: the
tracer is woken with `WIFSTOPPED` and `WSTOPSIG == SIGSEGV`, reads the registers
and `PTRACE_GETSIGINFO`, and decides whether to pass it on. This kernel's trap
path calls `send_fault_signal` and then goes straight to

```rust
if crate::hal::kernel::disposition_is_default(sig) {
    crate::hal::kernel::die_of_fatal_signal(sig);
}
```

(`arch/src/arch/x86_64/idt.rs`, the vector-agnostic tail of `rust_trap`) - no
consultation of the tracer at all. So a traced process that faults **dies without
its tracer being told**, and every reading taken after that point was a reading of
a corpse: `rip=0x100`, `rsp=0x4ba140`, `rbp=0x1080000` in one run, and `0xffff…`
from a failed peek in another. That is the mechanism behind most of this file's
retractions: **the instrument was not merely imprecise, it was being handed
state from after the event it meant to observe.**

**This is a real, nameable ptrace gap and it is the next thing to fix**: a
faulting traced task must park in a signal-delivery stop before the signal is
acted on, exactly as `trace_signal_stop` already does for an ordinary signal
(`src/sched/runtime/trace.rs`), with the fault's `siginfo_t` record available to
`PTRACE_GETSIGINFO` at that stop. Once that lands, the crash that this whole
investigation has been circling becomes readable in one run: `rip`, the register
file and the return address, at the fault, instead of after it.

**Tally**: twelve. Ten of them were the instrument, one was a wrong model, one was
an implementation written and reverted - and this pass names the reason the
instrument kept being wrong.

## 4v. FIXED: a faulting tracee stops for its tracer - and the crash is inside `call_fini`

§4u named the gap. This pass closes it, and the first register set the tracer ever
saw at a fault answers where the crash is.

**The fix** is `sched::fault_stop`, offered from **both** fatal-signal paths -
`rust_trap` (the exception vectors) and `rust_page_fault` (`#PF`, its own
`die_of_fatal_signal`). Adding it to only the first looked like it had not worked
at all, because a `SIGSEGV` here is a page fault and takes the second path: the
diagnostic never printed once. That is the same "two doors" mistake §6.90 records,
and it cost one build cycle to find.

`fault_stop` parks the tracee in a signal-delivery stop with `delivering: sig`, so
`PTRACE_GETSIGINFO` answers the fault's own record and a resume that names nothing
drops the signal - the contract `trace_signal_stop` already has for an ordinary
signal. `SIGKILL` is exempt, as it is on Linux.

**And the reading it buys, at the fault, from a live process:**

```
STOP sig=11 rip=0x3ff0fba8 rsp=0x3fef8a08 rbp=0x3fef8a60 rax=0x3ff0fba8
      rbx=0x2 rcx=0 rdx=0x4c0280 rsi=0 rdi=0 r15=0x4bb360
```

- `rax == rip` and the page is unmapped: an **instruction fetch** at an address
  nothing maps.
- `r15 = 0x4bb360` (the `__exit_funcs` node), `rbx = 2` (its count),
  `rdx = 0x4c0280` (`registered_objects.lock`): we are **inside
  `__run_exit_handlers`'s dispatch**, not lost in libc.
- and the stack names the frame: `[rsp] = 0x406554`, which is exactly the return
  address `call_fini` leaves behind, with `[rsp+80] = 0x4b5160` and
  `[rsp+56] = 0x1`, `[rsp+72] = 0x2` - the array cursor and loop counters of
  `call_fini`'s own walk.

So the fault is **inside `call_fini`, while it walks the fini/init array and calls
an entry**: the address it calls is a **stack** address (`0x3ff0fba8`, just above
`rsp`), not a code address. `.fini_array` itself holds two valid pointers
(`0x402f00`, `0x402dc0`), so the bad value is not in the array - it is what the
walk computes or what a callee leaves in a register.

**What to do with this next, precisely**: `call_fini`'s loop body is `call
*(%r12,%rbx,8)` at `0x402fb0`, with `%r12 = &fini_array - 8` and `%rbx` counting
down from 2. A breakpoint on `0x402fb0` with the register set read there names
the index and the target on the very call that faults - and the tracer can now do
that, which it could not before this pass.

**Tally**: this is the first pass in the file whose result is a kernel
*capability* rather than a retraction. The tracer can see faults now; every
earlier reading of this crash was taken from a corpse because it could not.

## 4w. `call_fini` is clean; the first `atexit` entry holds a **stack address**

With §4v's `fault_stop` in, a breakpoint at `call_fini`'s loop body and `_fini`'s
`ret` produces a complete, trustworthy trace of the finaliser walk
(`build/scratch/zprobe/{loop2,after}.c`):

```
LOOP rbx=0x2 -> *(r12+rbx*8)=[0x4b5178] ok=1 0x402dc0
LOOP rbx=0x1 -> *(r12+rbx*8)=[0x4b5170] ok=1 0x402f00
_fini ret: rsp=0x3fee9a08 [rsp]=0x406554
```

Both `.fini_array` entries are read correctly and both are code
(`0x402dc0`, `0x402f00`); `_fini` returns to exactly `0x406554`. **`call_fini` is
clean** - it is not where the bad value comes from, and §4w's "the fault is inside
`call_fini`" is narrowed to "the fault follows it".

**The node right after that return** is where the bad value is:

```
after call_fini: count 0x1 | entry0 fn 0x1086e7c3289dbf21 | entry1 fn 0x108698e7c0cdbf21
                 guard 0xdf9088434c33cfe6
```

Unmangled with the guard read in the same stop:

| slot | mangled | unmangled | verdict |
|---|---|---|---|
| `node+0x38` (entry 1) | `0x108698e7c0cdbf21` | **`0x402f80`** | `call_fini` - correct |
| `node+0x18` (entry 0) | `0x1086e7c3289dbf21` | **`0x3fd25ba8`** | a **stack address**, not code |

So the registration machinery works: both entries carry plausible mangled words
and the guard unmangles one of them to exactly the function that was registered.
The **first** entry's function pointer unmangles to a stack address - which is
precisely the shape the fault has (`rax == rip` on unmapped stack memory), and it
is the same class of value as the `0x3ff0fba8` in §4v's register dump.

**How the slot is written, read from the binary** (`__internal_atexit`,
`0x406291`-`0x4062a8`):

```
xor  __pointer_chk_guard_local,%rbx     ; rbx = fn
mov  %r13,0x10(%rax)                    ; arg
rol  $0x11,%rbx                         ; mangle
mov  %r12,0x18(%rax)                    ; <- +0x18 receives %r12, NOT the fn
mov  %rbx,0x8(%rax)                     ; <- +0x08 receives the mangled fn
movq $0x4,(%rax)                        ; flavor
```

That is glibc's `struct exit_function` as this build lays it out: `flavor@0`,
`fn@8`, `arg@0x10`, `dso@0x18` - **sixteen bytes per entry from `+0`**, not the
`+0x10`-relative layout the dispatcher's arithmetic suggested. So the question is
now sharp and one breakpoint wide: **`__internal_atexit` ran with an `%r12` that
was a stack address**, or the dispatcher is reading a different slot than
`__internal_atexit` wrote. Breaking at `0x4062a0` (the `+0x18` store) and at
`0x406520` (the `ef_at` dispatch) in one run names both.

**And a correction to §4v**: its "the fault is inside `call_fini`" was wrong - the
walk is clean, and the death follows the return. That is the thirteenth reading in
this file to need correction, and the first that was corrected *by* the capability
the pass before it added.

## 4x. FIXED: `execve` did not zero `%rdx`, and that is the whole static-exit defect

The investigation that began at §4e with "a static glibc program segfaults
returning from `main`" ends here, and it ends at **one instruction**.

**The measurement.** A 38-byte assembly `_start` that writes the eight bytes of
its incoming `%rdx` to fd 1 and exits:

```
guest:  00000000  28 76 d7 40 9c 7e 00 00      -> %rdx = 0x7e9c40d77628
Linux 7.3.4:                                    -> %rdx = 0x0
```

**The chain, and every link is now measured**:

1. Linux's `execve` contract defines exactly two registers on entry to the new
   program: `%rsp`, and `%rdx` = the dynamic loader's finaliser, **zero when there
   is no dynamic linker**.
2. glibc's `_start` does `mov %rdx,%r9` and passes it to `__libc_start_main` as
   `rtld_fini` (verified in the binary: `0x402e46`, and `0x405c5e` reads the
   saved copy before `call __cxa_atexit`).
3. `__cxa_atexit` registers it, so it becomes an **exit handler**.
4. At exit, `__run_exit_handlers` **calls** it - and what the register held is a
   leftover pointer, not a function. Watched at registration:

```
REGISTER #0: fn(rbx)=0x3fcb4ba8 dso(r12)=0 arg(r13)=0xc   <- the bad one
REGISTER #1: fn(rbx)=0x402f80 dso(r12)=0 arg(r13)=0xc     <- call_fini, correct
```

`__internal_atexit`'s caller is `0x405c6e` inside `__libc_start_main`, and it is
`__cxa_atexit(saved_r9, 0, 0)` - the `rtld_fini` slot.

**The fix** is one line in `set_exec_return` (`arch/src/arch/x86_64/syscall.rs`):
`regs.rdx = 0`. That function already rewrote the frame's `rcx` (the entry) and
`user_rsp`; `%rdx` was the register the ABI also pins, and it was left carrying
whatever the pre-exec call had in it.

**Verified on the machine**, the three programs that used to exit `139`:

```
rdx at _start: 00 00 00 00 00 00 00 00
m3 (bare return):    rc=0
m  (printf, return): hello   rc=0
d  (atexit handler): HANDLER RAN   rc=0
```

**Pinned** by `t-exec-rdx`, which builds a 38-byte ELF in `/tmp` (hand-assembled
so the guest needs no compiler), forks it with a pipe on stdout, and asserts the
eight bytes are zero. **Counterfactual**: commenting out the one line reddens
exactly that claim with `got 0x3fdcce68` and leaves the neighbour green.

**And the accounting this file owes a reader**: this took thirteen wrong readings
to reach - a `>=` break condition, reads taken before the instruction, three
struct-offset guesses, a 60-step sampling, ptrace's `-1` read as data, a wrong
`.bss` clobber model, a wrong `IRELATIVE` implementation, a wrong recursion model,
a broken breakpoint restore, and two "the fault is inside X" claims that the next
pass overturned. **All thirteen were the instrument or the model, none was the
kernel** - and the defect itself, when it came, was a single register the ABI
pins and the loader did not write.

The `fault_stop` capability §4u named, and that §4v added, is what made the last
step possible: without a tracer that can see a fault, the bad value is invisible.

## 4y. Re-measured on the fixed kernel: Chromium's stderr is clean, and the trap site is confirmed

With `%rdx` fixed (§4x), the same headless run against the installed image:

```
=== ve=4513 vo=0 ===
--- CHECK/FATAL ---   (none)
last lines:
  crashpad file_io_posix.cc:145  open /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq: ENOENT
  crashpad file_io_posix.cc:145  open /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq: ENOENT
```

That is a **new baseline**: no `CHECK`, no `FATAL`, and - for the first time in
this file - **no `ptrace` error at all**. Every crashpad complaint that earlier
passes chased (`EIO`, `no module mappings`, `Unexpected registers size`,
`No such process` storms, `Couldn't initialize main thread`) is gone. What remains
is a sysfs file this kernel does not have, which is the honest `ENOENT`.

**And the `traps:` line now names a site without any base arithmetic.** Two child
threads died at the *same* module offset:

```
traps: chromium[720] breakpoint ip:7d2b2d4f5127 ... in /usr/lib/chromium/chromium[9da2127,...]
traps: chromium[721] breakpoint ip:7ca006f74127 ... in /usr/lib/chromium/chromium[9da2127,...]
```

`0x9da2127` is a *mapping* offset, and with the executable LOAD's page-aligned
`p_offset` (`0x1e64000`) the file offset is `0xbc06127` - whose bytes are

```
0bc06123: cc          int3
0bc06124: 0f 0b       ud2
0bc06126: cc          int3
0bc06127: 0f 0b       ud2
```

**exactly Chromium's `IMMEDIATE_CRASH()` idiom**, and the same instruction class
§3s identified by hand. So the crash has not moved; what changed is that the
kernel reports it correctly on the first try.

**What this does and does not change about the Chromium question.** The kernel's
part of the chain now has *no* measurable defect left in it: the static-exit bug
that produced a storm of child deaths is fixed, crashpad's own calls all answer
correctly, and the surviving fault is the program's own `CHECK` at a named
instruction. The stderr still freezes and `--dump-dom` still yields 0 bytes, so
the browser still does not render - and the one instrument that would read *why*
is a crash handler that can inspect the CHECK's message, which is the `sized`
capability §4v's `fault_stop` now makes possible for the first time.

## 4z. FIXED: a traced task whose tracer passed the fault on now runs its own handler

Found while writing `t-ptrace-fault-stop` (§4v's pin), and it was the one shape in
the `fault_stop` family left open at §4v - named then, **closed here**. It is
recorded at this length because the section that named it stated a rule that was
never measured, and the measurement refuted the rule.

**The claim that was wrong, and how it got written.** §4z's first version said
"Linux's own answer for the handler case is that the handler does not run while the
task is traced - each pass-on is a fresh signal-delivery-stop, and only a tracer's
`PTRACE_CONT` **with signal 0** after that lets the tracee run the handler". That
came from `build/scratch/fsp`, whose handler could not repair the page: it printed,
returned, and the faulting instruction faulted again, so every resume produced a
`0x0b7f` stop and the probe read protocol where it was looking at its own fixture.
The identical-looking stops are **new faults**, not repeated delivery stops.

**The measurement that decides it** (`build/scratch/fsp2`, the same fork/trace/
fault fixture as a host Rust `std` program, Linux 7.3.4, two handler shapes):

| handler | what the tracer sees after one pass-on |
|---|---|
| prints `HANDLER`, returns without repairing | `HANDLER` printed, then a stop for **a new fault** at the same instruction, for ever |
| prints `HANDLER`, `mprotect`s the page, returns | `HANDLER` printed, then `EXITED code=7` - the child reached its own `_exit` |

So a pass-on **does** release the signal to the program's own handler, and it runs
at once, whether or not a tracer is watching. There is no second delivery stop.

**What this kernel did instead**, measured when §4z was written (diagnostics since
removed):

```
RSDIAG resumed id=7 signal=11 reported=Some(11)
FSDIAG sig=11 passed=true
PFDIAG2 sig=11 default=false
FAIL:  passing the signal on kills it (288, signaled false)
```

`passed_on` worked: the resume recorded it, `fault_stop` consumed it and returned
`false`, so the fault path was entered. It then asked
`disposition_is_default(SIGSEGV)` - `false`, the tracee has a handler - so
`die_of_fatal_signal` was skipped, and `deliver_at_trap`
(`src/syscall/linux/proc.rs`) **declined anything with a tracer**. The tracee was
therefore neither handed its handler nor ended; it re-faulted until
`fault_repeats(rip)` (`arch/src/arch/x86_64/idt.rs`) ended it at
`FAULT_REPEAT_LIMIT` with "died of SIGSEGV it could not be handed".

**The fix, and why it is this shape.** Two roads were open: let `fault_stop` build
the frame itself (it holds the `AbiRegs`), or narrow the trap door's refusal. The
second was taken, because the refusal's own reason is narrow and the fix should keep
it: a stop taken off a trap frame is a stop whose *syscall context* the tracer
cannot read (`docs/notes/` §2.13 - the aarch64 `there is no task 255` measurement),
which is why the door refuses a traced task at all. A **delivery** raises no new
stop, so it is not that question. The refusal now asks about the *signal*:

```rust
    if crate::sched::has_tracer(crate::sched::current()) {
        match crate::sched::released_fault() {
            Some(released) if crate::sched::deliverable_signal() == Some(released) => {
                // Spent here, before the frame is built: the licence is for
                // **one** delivery, and a second fault must earn its own.
                crate::sched::clear_released_fault();
            }
            _ => return false,
        }
    }
```

Four supporting pieces, each of them load-bearing:

- `Tracing::passed_on: Option<u8>` (`src/sched/runtime/task.rs`) records the number
  the tracer named at `PTRACE_CONT` - a resume is the *tracer's* syscall and the
  signal's effect belongs to the *tracee*, so the decision is recorded where it can
  be acted on. `sched::released_fault()` / `clear_released_fault()`
  (`src/sched/runtime/trace.rs`) are its readers, and `fault_stop` leaves the record
  standing when it is **this fault's own signal** (a resume that names a fault's
  signal hands the task back to the faulting instruction, so the same fault arrives
  a second time - parking again is how a tracee came to stop for ever at one
  address).
- `sched::deliverable_signal()` (`src/sched/runtime/child_signal.rs`) answers *which*
  signal is next without consuming it, so the door can ask "is the released one the
  pending one" before the frame exists. `take_deliverable_siginfo` destroys the
  answer.
- `requeue_keeping_record` replaces `queue_signal` in `resume_traced_with`: the
  fault's `si_code`/`si_addr` record is already queued by `send_fault_signal`, and
  `queue_signal` would overwrite it with `SI_USER` - telling the handler that the
  tracer's pid is the faulting address. Linux's handler sees `SEGV_MAPERR` and the
  real address.
- The licence is spent **before** the frame is built, so one delivery cannot serve
  a second fault.

**The pin** gained the handler shape as its last two claims
(`user/tests/src/bin/t-ptrace-fault-stop.rs`): a third child maps a page, starts
unmapping it inside its handler, and on one `PTRACE_CONT` with `SIGSEGV` must print
neither stop nor death but reach its own `_exit(7)` -
`ok: and one resume runs the program's own handler (…, code 7)`. Counterfactual:
restoring the flat `if has_tracer { return false; }` reddens exactly that claim.

**The second machine had the same hole at two levels** (both found while pinning
this, 2026-11-18). At first aarch64's `deliver_or_die`
(`arch/src/arch/aarch64/vectors.rs`) went straight to
`deliver_at_trap`-or-`die_of_fatal_signal` and never offered the fault to a tracer,
so the pin failed **four of its six claims** on that arm - the fault arrived as an
*exit*, `WSTOPSIG` read `-1`, the register file was unreadable and the pass-on had
nothing to pass on:

```
FAIL:  the fault arrives as a STOP, not an exit (445)
FAIL:  and its signal is SIGSEGV (-1)
FAIL:  the register file is readable there (ip None)
FAIL:  passing the signal on kills it (-1, signaled true)
```

`fault_stop` now runs **before** `deliver_at_trap` there, matching x86_64's order
(the tracer decides whether the signal is passed on at all, so asking the handler
first would run a handler for a signal the tracer had not yet released). The
delivery fix itself is machine-independent - aarch64's EL0 fault paths all funnel
through the one `deliver_or_die` - so the handler claims pass on both arms.

**Reach on Chromium's path: none measured.** crashpad wants a *stop* to inspect a
crash, which is what `fault_stop` gives it; whether it wants to pass a signal on and
run a handler afterwards is not something §4y's run exercised. This was a
correctness gap in the ptrace surface, not a named Chromium blocker - recorded
because "nearly right" there is the shape that sends a caller down a path it cannot
recover from.

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
