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
