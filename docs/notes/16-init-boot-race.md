# A lane whose `/init` dies before any program runs — an observation, and a rule it broke

## What was seen

During a full `cargo xtask test` on 2026-11-17 (both arms in one run), **one** of
the six x86_64 lanes never ran a program. Its serial log ended:

```
mpos: /init is up
ip: RTNETLINK answers: File exists
ip: RTNETLINK answers: File exists
sh[1]: segfault at 7da9be10b430 ip 00007da9151c8e03 sp 000000003fbd4790 error 4 \
        in /lib/ld-musl-x86_64.so.1[16e03,7da9151b2000+59000]
PFDIAG page=0x7da9be10b000 NOT MAPPED task=1 rip=0x7da9151c8e03 \
        mapped=none around the page bytes=[49,8b,54,cd,00,48,85,d2,0f,84,13,09,00,00,8b,42]
PFDIAG vma=none - no mapping of this process covers the page
INIT_EXITED: the system has no init left
```

The other five lanes were green, and `=== SUMMARY pass=N fail=0` lines were
present for all five — so this is one machine's init dying, not a sweep-wide fault.

## What the dump says

- `error 4` is a **read**, not a fetch: the faulting address `0x7da9be10b430` is
  2.8 GB away from `rip`, so this is a data access made from code in `ld-musl`.
- `PFDIAG ... NOT MAPPED ... mapped=none around the page` is explicit that **no
  mapping covers the page** — the diagnostic's own comment says an empty run is
  "a userland pointer into memory that is gone", and it prints "none around the
  page" precisely so it is not read as a one-page hole in a live mapping.
- The instruction bytes decode as `mov rdx, [r13 + rcx*8]` / `test rdx, rdx` /
  `je` — **a load of a pointer from an array**, so the base or index is bad, not
  the pointer being loaded. In `ld-musl` that shape is a libc init-table walk.

So `/init` was executing libc code that indexes a global table, and the page
holding that table was absent. That is a relocation/initialisation-order shape,
not a bad pointer from the program's own data.

## Frequency, and what was tried

**Not reproducible in five later gates**: three `--arch x86_64 --no-all-targets`
sweeps and two full dual-arm sweeps, all green with six summaries each. Nothing
was changed in the tree between the failing run and those five.

Recorded as an **observation, not a diagnosis** — the same standing this
repository gives the aarch64 EL0 boot-check death (`AGENTS.md` §5). What differs
is that its signature is *new*: the earlier `INIT_EXITED` recorded in
`06-process-management.md` was a `trap 13` **general protection fault** `#GP` in
`sh`, where this one is a `#PF` read (`error 4`) from libc's own table.

## The rule this broke

The failure was first reported as "a flake, re-ran green, not a regression". That
conclusion was reached from the *re-run* alone, and it is not what this repository
asks for. `AGENTS.md` §5's entries for this class all say the same thing: quote
the log's own summary line, record the observation with when it happened and what
its dump said — and **a boot-stage death is not a timeout**, so the "an observed
hang" entry does not cover it. A re-run that goes green proves the *rate* is low;
it does not prove the cause is benign, and it is not a substitute for writing the
signature down.

Two things would have been cheaper than the five re-runs that followed:

1. **Preserve the log before re-running.** The lane's serial log is overwritten by
   the next run, so the only record of the failing boot is the excerpt quoted in
   a transcript. `build/probe/fail-<n>.log` is where it should have gone.
2. **Check whether an identical signature is already known.** It was one `grep`
   away — `06-process-management.md` records an `INIT_EXITED` whose cause was a
   guard that could panic, and `src/syscall/linux/fs/ioctl.rs` carries the
   comment. That is a different trigger but the same terminal state, which makes
   the family worth knowing about when the next one appears.

## If it recurs

The decisive next measurement is **the init's own address space at the moment of
the fault**: whether the mapping that should hold the libc table exists but is
absent from the VMA list (a bookkeeping loss), or was never created (an exec /
loader ordering fault). `PFDIAG` already prints the run of mapped pages around the
fault, so a recurrence should be read from `mapped=[lo..hi)` first: if a
neighbouring page is mapped and this one is not, it is a hole; `mapped=none` says
nothing near it was mapped, which is the loader case.

## The exec path was audited against that dump, and is sound

The dump points at the initial-stack and auxv construction, so those were read
line by line against the fault. Each is **not** the cause:

1. **`initial_stack_rsp` cannot underflow.** It refuses a block bigger than
   `USER_STACK_PAGES * PAGE_SIZE` (16 pages = 64 KiB). The worst case its own two
   checks allow is `argc = 512` with 1-byte strings plus a minimal environment:
   `words_bytes = (1 + 512 + 1 + 1 + 1 + 40) * 8 = 4448`, `total = 4985` bytes —
   **4.9 KiB of the 64 KiB budget**. `stack_top` is 1 GiB
   (`USER_STACK_TOP`), so `stack_top - total` cannot wrap.

2. **The auxv sizing has slack, not a shortfall.** `AUXV_WORDS = 40` is 20
   (type, value) pairs; `auxv()` writes **18** pairs = 36 words. Its doc comment
   says "Twenty ... pairs", which is stale by two, but the direction is safe: two
   more entries would still fit. Worth correcting, not a bug.

3. **`AT_PHDR` is the program's, not the interpreter's.**
   `at_phdr = layout.bias + image.phdr_addr`, and Linux measured the same shape
   (`build/scratch/auxv/a.c`: `AT_PHDR` outside the loader's `AT_BASE`,
   `AT_PHENT` 0x38, `AT_PHNUM` 15). This is exactly what `ld-musl` needs to find
   the program it is about to run.

4. **The argument-block write is guarded by an `assert!` that cannot fire in
   normal flow — and that is deliberate.** `apply_stack` ends with
   `assert!(write_user_of_at(root, base, &block, false), "the argument block did
   not fit")`. The `populate = false` half is documented: the block must land on
   eagerly-mapped pages because the caller already holds the task table, so the
   fault that would claim one has nothing to fault on. A violation is a **kernel**
   bug (a layout that drifted from its own sizing), so a panic is the honest
   answer there rather than `E2BIG` — which is what a *too-small* block is, and
   that is answered earlier by `ArgsTooBig` → `err::E2BIG` at all three spawn
   doors (`proc.rs:2311`).

5. **The directory-borrow rule holds on this path.** `write_user_of_at` is the
   variant that does *not* re-take the task table, precisely because the exec path
   calls it with `with_table` on the stack; its doc comment states the rule and
   says the flag "keeps the impossibility checked rather than assumed".

So the audit narrows the unknown rather than closing it: the fault is **not** in
the sizing, the auxv contents, `AT_PHDR`'s derivation, or the write's error
handling. What it leaves is the address space itself — whether the mapping that
should hold `ld-musl`'s table was created and then lost, or never created. The
`PFDIAG` line already prints `mapped=none around the page`, which is the
loader case rather than a hole; a recurrence should be read with that in mind.

## Second occurrence, 2026-11-17 — and this time the log was kept

A later full gate stalled `x86_64` lane 5 at 900 s. The lane's serial log
(`build/probe/lane5-timeout-*.log`) ends:

```
=== PASS t-seat
=== RUN  t-exit-group
  trap 13 code 0x0 rip 0x7dce97087f1d rsp 0x3fa74058 rflags 0x10216
  bytes at rip: [0f, b6, 04, 0f, 0f, b6, 14, 0e, 39, d0, 75, 0e, 48, ff, c1, 85]
traps: sh[1] general protection fault ip:7dce97087f1d sp:3fa74058 error:0
INIT_EXITED: the system has no init left
```

**This is the same terminal state as the first occurrence and a different
signature**: the first was a `#PF` read (`error 4`), this is a `trap 13` general
protection fault — and it matches the shape `06-process-management.md` records
from an unrelated cause (a guard that could panic), which is exactly why the
family is worth knowing.

### What the dump rules out

- **The bytes at `rip` are a byte-wise `memcmp`** (`movzx`/`cmp`/`jne`/`inc rcx`).
  A byte load cannot fault for alignment, so `#GP` here means a **bad base
  register** (non-canonical) rather than a misaligned access. `rflags = 0x10216`
  has bit 18 (`AC`) **clear** and bit 16 (`RF`) set, so this is not the
  alignment-check path either.
- **`exit_group` did not take init with it.** The stall begins at
  `t-exit-group`, whose subject is precisely the group-ending call — so that was
  the first suspicion. It is not: `end_group_siblings` walks the table and skips
  any task whose `tgid` differs or that has already exited, so a forked child
  calling `exit_group` cannot reach pid 1. The `#GP` **precedes** the
  `INIT_EXITED` and is the cause; `INIT_EXITED` is only what the kernel says
  after its init is gone.
- **It is not `t-exit-group`'s own logic either** — the program passes on the
  other lanes and on aarch64 in the same run (334/334 green).

### The honest state

Two occurrences, both `sh[1]` dying in user space before its lane finishes, with
different fault classes and the same terminal line. Neither reproduces on demand
(five clean gates between them, and several after). The one thing that has grown
is the evidence: the faulting instruction is now known to be a **byte-wise
compare**, which points at a corrupted base pointer in `sh` rather than at the
memory system.

**Next instrument, unchanged and now more specific**: catch the fault with the
registers dumped (`rdi`/`rsi` would show whether the base is non-canonical), which
means extending `pf_diag`'s sibling for `trap 13` — the page-fault diagnostic
already prints `rip`, `rsp` and the bytes, and the GP path prints the same but not
the general registers that would name the bad address.
