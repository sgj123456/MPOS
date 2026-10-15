# §8 Phase 8: Security Hardening (Low Priority)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 8: Security Hardening (Low Priority)

### 8.0 A userspace process could panic the kernel — **fixed: a file is not one buffer**

`dd if=/dev/zero of=/tmp/x bs=1M count=700` used to end in a kernel panic and
QEMU exit `35`. The cause is now read off the machine's own report rather than
inferred, and it was neither of the two things first written here:

    KERNEL PANIC: memory allocation of 134217728 bytes failed
      last 8 syscalls: nr 1 (write) ...

Not ramfs being unbounded, and not the ext4 block cache (fixed at 512 preallocated
slots). It was a **ramfs file's own storage**: one `Vec<u8>`, so growing a file
means a `realloc` for one *contiguous* run - doubling to 128 MiB - which is not a
shortage of memory but an order the buddy allocator never serves. The machine died
with bytes still in hand.

**Fixed** by the shape Linux uses: `Chunked`, a file as a list of 64 KiB chunks,
so growth allocates 64 KiB at a time however large the file gets
(`vfs/ramfs.rs`, `a_file_is_chunks_and_not_one_contiguous_run`). The invariant the
first version got wrong is worth stating because nothing else on the page says it:
**every chunk but the last is whole**, because a chunk that stops being last has
bytes after it now - a 5-byte interior chunk makes `len()` lie about everything
that follows, which three existing file tests caught at once.

Also landed, as the tmpfs behaviour this kernel genuinely lacked: a per-mount byte
budget, charged *before* allocating and answered with `ENOSPC`
(`Usage`, `a_full_mount_answers_enospc_instead_of_growing`), with room returned on
truncate-down and on the last reference dropping rather than on `unlink` - a
deleted-but-open file still occupies its bytes. The budget guard checks what the
machine could actually hand over: free frames **plus the heap's own cached
bytes**, since the heap keeps what it has taken and `free` never rises again.

`t-fill-ramfs` covers the end-to-end property (refused, not dead) and one
non-obvious trap it found: `ftruncate(0)` leaves the descriptor's offset where it
was, so the next write asks for a hundreds-of-megabytes hole and is refused
*correctly* - a test that forgets to `lseek` back measures the kernel's manners,
not its memory.

**Still open, and it is the rest of the original item**: the global allocator's
failure path is still `panic!`, so any *other* unbounded user-driven growth - a
`Vec` in another filesystem path, the page tables a process that touches enough of
what it mapped will need - still ends the machine. Anonymous demand paging
(§5.1/§5.2) took the cheapest half of that away: mapping too much is now a
statement about addresses rather than a claim on memory, so the
map-a-tebibyte-and-never-touch-it shape this item was written around costs one
VMA and nothing else. What remains is the work the old plan named: make
user-facing allocation fallible end to end (`ENOMEM` out of the syscall, not a
panic) and give a process that runs the memory out something to lose besides the
kernel.

### 8.1 ASLR — **DONE** (four landmarks; `ET_EXEC`'s own address deliberately not one of them)

Measured on Linux before writing any of it, over 64 runs of the same program
each time: a position-independent binary's code base, its heap anchor and its
`mmap` page all move by amounts of the order of a tebibyte, and a process's
stack top moves too. The same program linked `-no-pie` kept its code address at
`0x401156` in every single run while its stack and its `mmap` page went on
moving. That last row is the shape of this feature: **a fixed-address image must
not be moved**, because the absolute addresses its own instructions carry were
chosen at link time, and "randomising" it produces a program that cannot start
rather than one that is hard to attack.

What is drawn per `exec`, in `Layout` (`src/sched/runtime.rs`):

| Landmark | Range | Notes |
|---|---|---|
| `ET_DYN` image load bias | a tebibyte's worth, page-aligned | `map_image` places every segment at `bias + vaddr`, and `AT_PHDR`/`AT_ENTRY`/`brk` are the *placed* addresses |
| The interpreter's base | the same, stacked below `mmap` | This is `libc` on this userland - a fixed base here is a supply of return addresses identical in every process |
| Stack top | 8 MiB | Linux bounds this by memory and rlimit, so the number is not portable; 8 MiB is what keeps the whole region clear of the image below |
| The `mmap` region's start | a tebibyte's worth | `mmap_cursor` starts here and grows down as before |

The regions are **stacked rather than drawn independently** - `mmap`'s start,
then the interpreter below it, then the image below that, with a fresh draw
between each pair - so no draw can land one region on top of another, and the
top of a placed image is always below `MMAP_BASE`, which is what keeps a bias
from walking a program's pages up into the kernel half. `fork` draws nothing: a
child that inherited its parent's pages at addresses of its own had would not
have inherited its parent's pages.

Relocation had to be settled to get here, and the answer is Linux's: **the
kernel relocates only an image that has no interpreter.** A dynamic one is
relocated by that interpreter, which finds the program's load address from
`AT_PHDR` - so a kernel that also applied the table would add the load bias to
every pointer twice. That pass used to run for both kinds, which was invisible
only while the bias was always zero, because writing `addend` twice and writing
`addend + 0` twice land in the same place.

`t-aslr` pins the parts a running program can see: it forks, `execve`s *itself*
in a reporting mode, and compares what two fresh runs say about themselves - the
stack and the `mmap` page differ, the code address and the heap anchor do not.
Self-exec is why it can do that without a shell, a fixture, or a runner that
passes arguments it does not pass to anything else. What it cannot cover is the
load bias, because every program in that initramfs is `ET_EXEC` and none of them
has an address the kernel is allowed to choose; that half is covered by the
desktop image, where every binary is a dynamic `ET_DYN` and a bias applied to
the mappings but not to `AT_PHDR` means the machine never reaches a login
prompt.

**Not done, and each one is a decision rather than an oversight**: no `vdso`
(there is none to move); the stack's range is our bound, not Linux's
memory-sized one; and the draws are 28 bits of page granularity where Linux
gives an `ET_DYN` base rather more. Adding `listen`/`accept`-shaped inbound TCP
(§3.3) and a `MAP_FIXED`-free `mmap` hole review would both want this layout
re-examined, which is worth knowing before either is attempted.

### 8.2 seccomp

| Syscall | Nr | Description | Status |
|---------|-----|-------------|--------|
| `seccomp` | 317 | Secure computing mode | Done (STRICT) |
| `prctl(PR_SET_SECCOMP/PR_GET_SECCOMP)` | 157 | Legacy latch spelling | Done (STRICT) |

- `SECCOMP_MODE_STRICT` — **done**. Latched via `seccomp(2)` (`op=0, flags=0,
  args=NULL`) or `prctl(PR_SET_SECCOMP, 1)`, and read back with
  `prctl(PR_GET_SECCOMP)` (0 unconfined, 1 strict). It is the real Linux
  allow-list, not a block-list: the thread may make only `read`, `write`,
  `exit` and `sigreturn`; every other syscall is fatal (`SIGKILL`), matching
  Linux's `do_exit(SIGKILL)` on a strict violation. The latch is per-task,
  cannot be cleared, survives `exec` and is inherited across `fork`/`clone`.
- `SECCOMP_MODE_FILTER` — **done, wired, and measured action by action**. The
  program, its validation and its evaluation are `src/seccomp.rs` (classic BPF
  with the packet idioms left out, one description of the instruction set shared
  by the validator and the interpreter, forward-only jumps that must land inside
  the program, an out-of-range load ending the program with `KILL_THREAD`, and
  the stacked action order compared as a *signed* field so `KILL_PROCESS`
  outranks `ALLOW`); the plumbing is `Task::seccomp_filters` — a cumulative
  per-thread list inherited across `fork` and kept across `exec` — reached
  through one `install_seccomp_filter` from both `seccomp(2)`'s op 1 and
  `prctl(PR_SET_SECCOMP, 2)`, which copies the program out of the caller and
  validates it *in full* before anything is stored (`EINVAL` malformed, `EFAULT`
  unreadable, `EACCES` for a caller that is not root, `EINVAL` for any flag), a
  machine-wide armed flag that keeps an unfiltered syscall to one relaxed load,
  and a gate that acts on the answer *before* the table runs.

**The answers were then measured**, one child per action and a control
beside every table, because four of them were not what this kernel did. The
three `TRACE` rows are one action met three ways — no tracer, a tracer that
asked for nothing, and a tracer that asked for the event:

| Action | What Linux does (measured, 7.2.7) | Before | Now |
|---|---|---|---|
| `ALLOW`, `LOG` | the call proceeds | same | same |
| `ERRNO\|d` | returns `-d` — and `d == 0` is a *successful* 0 | same | same |
| `TRAP` | no handler: killed by `SIGSYS`. With one: the handler runs, the call never happens, and the caller reads its own `rax` back (`getppid` answered 110, `getpid` 39 — the number, not a value) | `SIGSYS`, then `-ENOSYS` | `SIGSYS`, then `nr` |
| `KILL_THREAD` | killed by **`SIGSYS`**, not `SIGKILL` | `SIGKILL` | `SIGSYS` |
| `KILL_PROCESS` | killed by **`SIGSYS`** | `SIGKILL` | `SIGSYS` |
| `TRACE`, nobody tracing | the call fails with **`ENOSYS`** | allowed | `ENOSYS` |
| `TRACE`, tracer set nothing | the call fails with **`ENOSYS`** | allowed | `ENOSYS` |
| `TRACE`, tracer set `TRACESECCOMP` | a seccomp stop first — `SIGTRAP`, event **7**, the answer's data half in `PTRACE_GETEVENTMSG` — then the call runs | bit refused `EINVAL`; call allowed | the stop, then the call |
| `USER_NOTIF`, no listener | the call fails with `ENOSYS` | `ENOSYS` | same |
| anything unrecognised | killed by `SIGSYS` — the man page's "treated as `KILL_PROCESS`", confirmed here | `-ENOSYS` returned | killed |

Three drafts of that measurement were wrong in ways that produced plausible
numbers, which is why the control exists and why it is written down: judging
*every* syscall with the action measures libc's collapse rather than the kernel
(every line read `SIGSEGV`); `BPF_LD|BPF_W|BPF_ABS` is **0x20** and not 0x00, and
writing 0x00 makes it `ld #0` — after which every conditional filter quietly
judged nothing; and a parent that reads its child's pipe to EOF before waiting
hangs for ever when that child is *stopped* holding the write end. The control —
filtering `getpid` to `EPERM` must fail `getpid` with errno 1 while `gettid`
answers normally — caught each in turn.

- `SECCOMP_GET_ACTION_AVAIL` — **done, and it answers the truth**. Supported
  ⇒ 0, with the caller's own word left alone (Linux's return value carries the
  answer; there is nothing to write back); not implemented — today only
  `USER_NOTIF`, whose purpose is to hand the call to a userspace supervisor this
  machine cannot summon — or carrying *any* data bits, however supported the
  action in them is (`ERRNO|1` is refused, measured), ⇒ `EOPNOTSUPP`; a `flags`
  it would have to ignore ⇒ `EINVAL`; an unreadable pointer ⇒ `EFAULT`. Before
  this it answered success and wrote 0 for every question, so a caller that
  asked learned nothing at all — and since the probe exists precisely because
  Linux kills the process for an action it does not know, answering "no" to
  everything was at least survivable, while answering "yes" to something not
  implemented would not have been.
- **Pins.** `t-seccomp` is new and holds one line per fact: the probe's table
  including the data-bits refusal, one child per fatal action (each is killed by
  signal 31 and each must run before this process installs anything, because a
  filter is inherited and the more restrictive of several decides), the two
  refusals that leave the caller alive answering `ENOSYS`, `LOG` letting the call
  through, then install + the `ERRNO` answer + `PR_GET_SECCOMP` reading 2 + a
  second filter judging another number (the list is cumulative) + inheritance in
  a child that installs nothing + stacking, proved by a `TRAP` installed on top
  of an inherited `ERRNO` winning the decision. The `TRACE`-with-a-tracer case is
  pinned in the same file, before this process installs anything of its own: the
  parent traces a child, sets `PTRACE_O_TRACESECCOMP`, and reads the child's
  `RET_TRACE|0x1234` stop — status `0x7057f` (`WIFSTOPPED`, `SIGTRAP`, event 7),
  `PTRACE_GETEVENTMSG` answering `0x1234`, and the call then running. `t-posix`'s probe line — it used
  to assert the empty-set answer — now expects `EOPNOTSUPP` for a word naming no
  action at all.
- **The third `TRACE` case — done.** The `PTRACE_EVENT_SECCOMP` stop now has a
  producer (`sched::seccomp_trap`, reached from the gate's `RET_TRACE` arm
  through `proc::seccomp_trace_stop`): a task whose tracer set
  `PTRACE_O_TRACESECCOMP` stops at the filter's `TRACE` decision, *before* the
  call, with event **7** and the answer's **data half** in
  `PTRACE_GETEVENTMSG` (measured: `RET_TRACE|0x1234` on `getpid` reported
  `0x1234`; the stop word was `0x7057f`). The call is then made normally — the
  stop is a notice, not a refusal. `PTRACE_SETOPTIONS` now accepts the bit
  instead of refusing it `EINVAL`, and the *other* third case — a tracer present
  but silent, which this kernel used to let through — is now measured to fail the
  call `ENOSYS`, Linux's answer. The stop's `syscall` record is the in-flight
  entry one, so `PTRACE_GET_SYSCALL_INFO` describes the call.
  - **A rewrite at the stop changes the call — done (2026-10-16).** A
    `PTRACE_SETREGS` at the `PTRACE_EVENT_SECCOMP` stop redirects the syscall,
    which is the mechanism a syscall-table sandbox is built from. Measured on
    this developer's Linux first (a throwaway x86_64 host probe, kernel 7.2): the
    judged `getpid` stopped with `orig_rax = 39`, and rewriting that field to 110
    made the tracee's call answer its parent's pid (10621) instead of its own
    (10622).
    Two halves were missing. `dispatch`'s `RET_TRACE` arm now reads the number and
    the arguments out of the frame *after* the stop rather than reusing the ones
    it arrived with, through `hal::Gate`'s `syscall_number`/`syscall_args`. And
    the redirect has to reach that frame: on x86_64 `restore_user_regs` does not
    carry `orig_rax` — the number and the result share `rax`, and the result is
    the gate's to write after the handler — so `proc::seccomp_trace_stop` writes
    the tracer's `uregs::NUMBER` slot back through `hal::set_syscall_number`. On
    aarch64 the number is `x8`, which the frame conversion already carried; that
    write is the same value a second time. Pinned in `t-seccomp`'s
    `seccomp_stop_reaches_a_tracer`: the tracer reads the registers, rewrites the
    number slot to `getppid`, and the tracee exits 0 only if the call that ran
    answered its parent's pid — which the `getpid` it asked for cannot do. The
    pin is counterfactual: with the re-read removed, `t-seccomp` is the one red in
    the sweep (`pass=172 fail=1`); with it, x86_64 is `pass=173 fail=0` with
    `mpos-kunit 32 passed, 0 failed` and exit 33, aarch64 `pass=172 fail=0`.
    - **And the same rewrite at an ordinary `PTRACE_SYSCALL` entry stop — done
      (2026-10-16).** `proc::syscall_entry_stop` used to answer only the
      *arguments* a tracer may have rewritten; the number it was handed was the
      number `dispatch` made, which is why the two halves of the tracer's
      interface disagreed - the arguments came from the frame, the call did not.
      It now answers both, through the same `number_after_stop` the seccomp stop
      uses, and `linux_dispatch` rebinds `nr` to it so the exit stop's report and
      the signal delivery's restart question are about the call that runs rather
      than the one that was asked for.
      Linux honours this at that stop, measured on this developer's kernel first
      (7.2, `.workbuddy/probe-ptrace-nr.c`): a judged `getpid` stopped with
      `orig_rax = 39`, rewriting it to 110 made the tracee's call answer its
      parent's pid (16190) rather than its own (16155), and the stop after it
      read the *new* number back out of the same slot (`orig_rax = 110`). One
      trap in the probe, worth recording: without `PTRACE_O_TRACESYSGOOD` a
      syscall stop reports a plain `SIGTRAP` and is indistinguishable from a
      signal-delivery stop, which is the whole reason that option exists.
      Pinned in `t-ptrace`'s `entry_stop_rewrite_changes_the_call` - a tracer's
      own version of `t-seccomp`'s claim, with no filter and no event, just the
      syscall boundary any tracer asks for. The pin is counterfactual and the two
      kernels differ by one number: the exit stop answers the parent's pid (14)
      with the re-read and the tracee's own (15) without it, which is
      `pass=173 fail=0` with `mpos-kunit 32 passed, 0 failed` and exit 33
      against `pass=172 fail=1` with `t-ptrace` the one red; aarch64 is
      `pass=172 fail=0`, where the slot reads 172 and the answer is the parent's
      10.
      - Not measured, and recorded rather than guessed: a rewrite at either stop
        that *cancels* the call - Linux's `nr = -1` sentinel - rather than
        replacing it with another number. Nothing in the sweep asks for it.
- **Not measured, and recorded rather than guessed**: strict mode's own violation
  signal. `seccomp(SECCOMP_SET_MODE_STRICT)` answers `EINVAL` on this machine for
  every argument shape tried, including after `PR_SET_NO_NEW_PRIVS` and with
  `args == NULL`, which is what an outer sandbox filtering `seccomp(op = 0)`
  looks like. Strict therefore keeps `SIGKILL`, the documented answer, and
  `t-posix`'s pin of it stands unchanged.

### 8.3 User Stack Canary — **DONE** (kernel side)

`AT_RANDOM` is now filled from the kernel ChaCha20 pool (RDRAND + boot-epoch +
tick seeded) at every exec — **16 random, per-process, per-boot bytes** — which
is exactly what musl/glibc derive `__stack_chk_guard` from, so a stack-protector
canary is now unpredictable (it was a fixed constant before, defeating the point).
The fail path (`__stack_chk_fail` → `abort()` → `SIGABRT`) is musl's own and
needs no kernel work; `-fstack-protector-strong` is a user-build flag, not the
kernel's. Kernel-side stack protection is therefore complete.

---


### R.8 (moved from ROADMAP's status board) — the allocator failure path, still open

8.0: **a userspace process could panic the kernel** — fixed for the case that was reproducing (a ramfs file grew as one `Vec`, and a 128 MiB contiguous `realloc` is an order the buddy allocator never serves), and tmpfs now has a byte budget answering `ENOSPC`; demand paging also took "maps too much" off the list, since a mapping is now an address space and backs nothing until it is touched. Still open: the global allocator's failure path is `panic!`, so any other unbounded user-driven growth still ends the machine (§8.0).
