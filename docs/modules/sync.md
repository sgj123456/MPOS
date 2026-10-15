# sync

**Home:** `src/sync/` · **Owns:** the locking doctrine (`critical_section`, never `spin`)

## What it owns

`src/sync/mod.rs` is unusual among these pages: it is almost entirely a *written contract*, not a
body of code. The module holds no types of its own. What it owns is the doctrine the rest of the tree
follows, and the value of the page is that the doctrine has one home a reader and a reviewer can
point at.

The doctrine is short, and each clause is a direct consequence of the machine. **This kernel is
uniprocessor, so disabling interrupts *is* the mutual-exclusion mechanism** - there is no second core
to contend with, and a lock that spins on a holder this same core just preempted is a deadlock with
the machine stopped on it. So the universal pattern is `critical_section::Mutex<RefCell<T>>`: the
mutex disables interrupts on acquire and restores them on release, and the `RefCell` gives interior
mutability without `unsafe`. `spin::Mutex` is deliberately never used. The one exception is
`spin::Once`, for one-time hardware initialisation that must happen exactly once - the module comment
names `arch::x86_64::gdt` and `arch::x86_64::idt`.

Every lock acquisition in the tree goes through `critical_section::with`, whose implementation is the
machine's. On bare metal each port supplies one: `arch/src/arch/x86_64/critical.rs` masks RFLAGS'
interrupt bit with `cli` and reads whether it was already set, and `arch/src/arch/aarch64/critical.rs`
does the same on `DAIF` with `msr daifset, #2`. Both read the flag *before* masking it, so a nested
section records "interrupts were off" correctly and only the outermost section re-enables them. On the
host test target the standard library provides the implementation, which is what lets the host suite
run the same `critical_section::with` call sites the machine does.

What this module deliberately does **not** own is the *blocking* half of concurrency. `sched` owns
`block_until` and `block_until_deadline`, and the discipline that goes with them - *lock, poll, try,
unlock, park* - is stated in `src/net/mod.rs` and `docs/ARCHITECTURE.md` §7 rather than here, because
a sleeping wait is the scheduler's business and this module is the lock's. What the two share is the
one rule the repo paid for: **never sleep holding a lock.** A section that parks while holding a
`critical_section::Mutex` has stopped the only core that could ever release it.

The comments in the source are the authoritative statements here, and two of them have drifted from
the tree. The module header says `spin::Once` is used "in exactly two places", but a grep of the tree
also finds `src/board/x86_64.rs`'s `DATA_DEV: spin::Once<Arc<dyn mpos::blk::BlockDev>>` - a third use.
And it names `arch::x86_64::critical` as *the* bare-metal implementation, while aarch64 now has its
own `arch/src/arch/aarch64/critical.rs`. Both are observations from reading the tree, not claims the
code makes; neither changes the doctrine, but a reader following the header alone would be misled, so
they are named here rather than silently corrected.

## Key files

| File | What lives there |
|---|---|
| `src/sync/mod.rs` | the doctrine in prose: uniprocessor mutual exclusion, `critical_section::Mutex<RefCell<T>>`, never `spin::Mutex`, `spin::Once` for one-time init |
| `arch/src/arch/x86_64/critical.rs` | the x86_64 `critical_section::Impl` (`cli`/`sti`, RFLAGS read before masking) |
| `arch/src/arch/aarch64/critical.rs` | the aarch64 `critical_section::Impl` (`msr daifset`/`daifclr`, `DAIF` read before masking) |
| `docs/ARCHITECTURE.md` §7 | the concurrency model, including the *lock, poll, try, unlock, park* pattern and where it is exemplified |
| `src/net/mod.rs` | the working copy of that pattern (the file the doctrine points at) |

The tree-wide picture is not a file: `critical_section::with` and `critical_section::Mutex` appear
throughout `src/` - the VFS node tables, the scheduler's task/waiter lists, the virtio device state,
`drivers/drm/`, `net/*`, `uevent`, `rand`, and `src/console.rs`'s log rings are all built on them.
`spin::Mutex` appears nowhere outside the module's own prohibition (grep).

## Contracts

The contract is a set of rules rather than a callable API, and a new call site keeps all of them:

- Shared state is `critical_section::Mutex<RefCell<T>>`; access is only inside `critical_section::with`.
- `spin::Mutex` is never used. `spin::Once` is reserved for one-time initialisation.
- Never sleep (or park, or call anything that may park) holding a lock; the pattern is *lock, poll,
  try, unlock, park* (see `src/net/mod.rs`, `docs/ARCHITECTURE.md` §7).
- Interrupt context does none of: sleep, allocate, or walk an unbounded structure (`AGENTS.md` §3).
- The idle halt belongs to the scheduler's loop, not to a task that stays runnable.

The edges - who may call this module and whom it may call - are `docs/ARCHITECTURE.md` §2/§3, and the
concurrency invariant of the shape above is §7. There is no trait or entry point here that another
module links against; the "contract" is the invariant each caller of `critical_section::with` keeps.

## What userspace sees

Nothing. This section is empty on purpose: `sync` is a purely kernel-internal concern, and a lock
bug in it surfaces to userspace as a hang, a lost wakeup, or a stall rather than as any errno. There
is no row here for `docs/uapi/` to own.

## Pins

There is no host test in `src/sync/mod.rs` (grep: no `#[cfg(test)]`), and no `t-*` program in
`user/tests/src/bin` or `user/abitest` names a lock - the doctrine is not pinned by a test of its
own. That is worth stating plainly rather than papering over: the rule is enforced by review and by
the fact that a `spin::Mutex` would be visible in a diff, not by a check that goes red. The same is
true of the two `critical_section::Impl`s, which are exercised only indirectly - every one of the
many host tests that uses `critical_section::with` runs the host implementation, and the machine
implementations are exercised by every boot.

What is pinned is the *behaviour that depends on the doctrine*: the blocking primitives this module
defers to are covered by `user/tests`' `t-futex`, `t-epoll`, `t-tty-eintr`, `t-tty-vtime`,
`t-eventfd` and `t-pipe2`, and the console's transmit-ring parking path (which exists precisely so a
writer does not spin for room) is covered by `t-tty-*` and the boot's own serial checks. Those tests
would fail if a wait deadlocked, but they are tests of `sched`/`console`, not of `src/sync` - so the
pin list here is genuinely an indirect one, and no test would fail if the module's *documentation*
stopped matching the tree.

## Evidence and open work

The idle-halt-cost measurement and the "a wake that nobody re-reads is a wait that never ends"
reasoning are `docs/notes/` §10.2 and §12.4a; the concurrency model, including the *lock, poll,
try, unlock, park* pattern, is `docs/ARCHITECTURE.md` §7. The rules a change to this area must carry
are `AGENTS.md` §3 ("Concurrency" and the seven extended rules), and `docs/ROADMAP.md` owns the
remaining work of the phase that keeps the graph honest. If this module ever grows a
project-specific lock wrapper, the place it would be documented is here, in the section the module
header currently marks with a `TODO` about higher-level synchronization.
