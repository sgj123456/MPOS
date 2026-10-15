# sched

**Home:** `src/sched/` · **Owns:** tasks — the task table and their stacks, which task runs next, blocking/sleep deadlines, per-process signal state, futexes, and the exit-hook registry

## What it owns

`sched` owns the *task*: the thing the kernel multiplexes the one CPU between. It is split along the
same line every other module is, and the split is chosen for host testability: `policy.rs` decides
which task runs next and "has no idea what a register is" — a set of ready queues and a sleep queue
driven by a tick count the caller owns — so it is unit-tested on the host. `runtime.rs` below it
owns the task table, the stacks and everything that must touch the machine, and calls into
`arch::context` to move between tasks. Between them sit three pieces of *vocabulary* kept out on
their own so they can be pinned without a scheduler: `signal.rs` (numbers, pending-bit arithmetic,
dispositions, the real-time queue), `futex.rs` (the address→task wait table and its ordering) and
`exit.rs` (the registry that inverts per-subsystem cleanup). `mod.rs` is the facade: it re-exports
`policy`, declares `runtime` as `#[cfg(target_os = "none")] mod runtime;`, and supplies the
one-process host stand-ins for the calls the host build still needs to answer.

Two invariants shape the runtime, both stated in `runtime.rs`'s own header and both about aliasing.
First, a task switch happens *inside* the interrupts-off window that guards the table, so no borrow
of the table is ever alive across `context::switch`; every access is either inside a `with_table`
closure (which cannot switch) or a single statement at one of the two switch sites. Second, a
`Context` must never move, because a suspended task's saved stack pointer lives in it — which is why
the table is a fixed-size array rather than a `Vec`, and why the count is a hard limit (`MAX_TASKS`,
64) rather than a grown collection. Third, and this is the "why" the module opens with: a task that
gives up the CPU switches to the *scheduler task*, not directly to its successor, because a task
cannot free the stack it is standing on — the exiting task hands over, and the scheduler buries it
from a stack of its own.

What deliberately does **not** live here is other subsystems' per-process cleanup. `exit.rs`'s
header explains the inversion: `sched` owns tasks, not sockets, watches, evdev clients or record
locks, and it used to name every one of them on the exit path, which is what made `sched` unnameable
without `net`, `vfs` and `drivers` (the finding recorded as V-3 in `docs/ARCHITECTURE.md` §9). Now a
subsystem registers a hook (`register`/`register_cloexec`), and the teardown walks the list without
knowing what is on it. There are two lists, not one, because descriptors close on two different
events — `do_exit` ends the process and tells every per-file owner; `execve` closes only the
`FD_CLOEXEC` ones and the process lives on — and a single list with a mode flag would hide that
distinction in a boolean.

**Host-compilation, and the one file that is not.** `src/lib.rs` gates `sched` to "the host, or bare
metal on either arch" rather than to an architecture, precisely because its policy is the host
suite's to pin. Inside the module, `policy`, `signal`, `futex` and `exit` are declared
unconditionally and so are host-compiled (the consequential fact — `sched`'s pure half is
host-tested); `runtime` is the single file behind `#[cfg(target_os = "none")]`, because it owns
machine state and names `hal::Context`/`ContextOps`/`UserFrame`. That is the layering rule working
as intended rather than a violation of it: the machine-touching body is gated, and the pure logic is
not — the opposite of the shape `src/mm/heap.rs` has. `runtime.rs` therefore has **no host tests at
all**; its contract is pinned only by the `t-*` programs below, which is the reason the host suite
and the bare-metal sweep are both load-bearing here.

`runtime.rs` also carries the sizeable host stand-in layer in `mod.rs`: on a host build there is one
process, so credentials (`HOST_CREDENTIALS`), the creation mask (`HOST_UMASK`), the seccomp latch and
filter list (`HOST_SECCOMP_STRICT`, `HOST_SECCOMP_FILTERS`), the UTS namespace (`HOST_UTS`), `comm`
(`HOST_COMM`) and the robust-list head (`HOST_ROBUST_HEAD`) each reduce to one value, and calls like
`current_owner`/`current_real_owner`/`may_inspect`/`current_groups`/`membarrier_registrations`/
`descriptor_numbers` answer the honest "no task table" value. `mod.rs`'s tests pin the one behaviour
that is observable on such a build (`unshare_uts`'s copy semantics).

The PID namespace's two translations are host stand-ins for a different reason: with no task table
and no namespaces, every task is in the *init* namespace, whose numbers are the slots - so
`pid_number_of` and `resolve_pid` are the identity there, which is exactly what they are on a
machine that never unshares. That is what lets `/proc`'s rendering and `signalfd`'s record call the
one rule instead of carrying a `#[cfg]` pair each (the `crates/pidns` leaf holds the tables
themselves, and its own seven cases run under `cargo test`).

The registers are deliberately *not* `sched`'s. A stopped task's register file is carried by value
as `hal::AbiRegs`, sized by `hal::Contexts::ABI_REG_COUNT`, and the only code here that looks inside
one does so through the machine's `to_user_regs`/`from_user_regs` — the same code is right on a
machine whose file is 34 words as on one whose is 27. The `ptrace` protocol words themselves
(`PTRACE_EVENT_*`, `PTRACE_O_*`, `SYSCALL_TRAP`) come from the arch-neutral `crate::abi` leaf, which
is what took `sched → syscall` to zero (`docs/notes/` §13.2).

## Key files

| File | What lives there | Host-compiled? |
|---|---|---|
| `src/sched/mod.rs` | The facade: re-exports of `policy` and (on bare metal) `runtime`'s large export list, the `runtime` gate, the `#[cfg(not(target_os = "none"))]` one-process stand-ins, and the `unshare_uts` copy test | yes — `#[cfg(test)] mod tests` |
| `src/sched/policy.rs` | `Scheduler` (ready queues + sleep heap), `TaskId`, `Priority` and its four bands, `Sleeper`'s wake-ordering rule, and `ARGV_MAX`/`ARGS_BYTES_MAX` | yes — `mod tests` |
| `src/sched/runtime/` | The task table (`Table`, `Task`, `Shared`), `Context`/`Stack`, the `Mm` address-space carrier, `Layout`/ASLR, `spawn`/`fork`/`clone`/`vfork`/`exec_replace`, `exit`/`exit_group`/reaping, `block_until*` and `wake`, signal delivery, ptrace stops, timers and resource limits, and the user-memory and mmap accessors. One 9 099-line `runtime.rs` split by responsibility: `task.rs` (the table, `Task`, `Context`/`Stack`, `Layout`, the scheduler loop, timers and limits), `child_mm.rs` (the `Mm` carrier and the mmap/user-memory accessors), `child_signal.rs` (delivery, stops, the wait queues), `child_exit.rs` (`spawn`/`fork`/`clone`/`vfork`/`exec_replace` and the exit/reap path), `load.rs` (image loading, the `#!` chain and the pid-1 handoff), `trace.rs` (ptrace) and `procfs.rs` (the snapshots `/proc` reads) | **no** — `#[cfg(target_os = "none")]`, and no host tests |
| `src/sched/signal.rs` | Signal numbers and `MAX_SIGNAL`, `Action`/`Actions`, `DefaultAction` and `default_action`, `SigInfo`/`SI_*`, `RtQueue`, `Pending` and the bit helpers, `catchable`, `AltStack` | yes — `mod tests` |
| `src/sched/futex.rs` | The `WAITERS` table, `register`/`unregister`/`is_woken`, `mark_woken_bitset`, and the `wait`/`wait_bitset`/`wake`/`wake_bitset` entry points | yes — `mod tests` |
| `src/sched/exit.rs` | The two hook registries (`ExitHook`/`HOOKS`, `CloexecHook`/`CLOEXEC_HOOKS`), `register`/`register_cloexec`, and the snapshot-then-walk `run`/`run_cloexec` | yes — `#[cfg(test)] mod tests` |

## Contracts

The *edges* (who may call `sched`, and which direction) are `docs/ARCHITECTURE.md` §2/§3 plus the
measured-edge tables in §6 and §9; this page does not copy them. The invariants each entry point
keeps are worth naming.

**Policy (`src/sched/policy.rs`).** `Scheduler` is the host-testable decision: `pick` drains the
bands top-down (strict priority) and round-robins within a band by popping the front and appending
to the back; `sleep`/`advance` implement a deadline queue (`Sleeper`'s `Ord` *is* the wake rule —
earliest deadline, then highest priority, then lowest id, written out so wake order is deterministic
rather than a function of heap internals); `wake` pulls a sleeper back to ready; `retire` removes a
task from every queue so a disposed slot is never dereferenced. `Priority::new` clamps rather than
rejects. `ARGV_MAX`/`ARGS_BYTES_MAX` are here rather than in `runtime` because the syscall layer
checks them on the host too (`runtime.rs` says so beside its import).

**Task lifecycle (`src/sched/runtime/`).** `current`, `spawn`/`spawn_user`/`spawn_init`/
`try_spawn_user`, `fork`/`vfork`/`clone`/`spawn_thread`/`exec_replace`, `exit`/`exit_group`,
`wait_any_child`/`poll_any_child`/`block_until_child_event` and the `WaitEvent`/`WaitPark`/`WaitEnd`
vocabulary. The invariant that matters across all of them is the aliasing one from the header: no
borrow outlives a switch. `block_until_deadline`/`block_until_interruptible` are where a waiting task
parks; `wake` is the "you have a message" half that a condition's owner calls after making the
condition true.

**Signals (`src/sched/signal.rs` + `runtime.rs`'s delivery).** `signal.rs` is the vocabulary and the
bit arithmetic (`Pending`, `bit`/`add`/`take`, `from_wire`/`to_wire`, `RtQueue`); the runtime owns
delivery. The rules the vocabulary encodes: standard signals coalesce, real-time signals
(`is_rt`) queue every copy in `RtQueue` (bounded, dropping silently on overflow — a named
divergence from Linux's per-user `RLIMIT_SIGPENDING`, stated in the file); `catchable` refuses 0,
`_NSIG`+1, `SIGKILL` and `SIGSTOP`; and `default_action` is a table rather than a single "fatal"
answer, because `SIGSTOP` must stop and `SIGCONT` must resume. The pending set uses Linux's
canonical layout (signal *n* at bit *n-1*), so the wire conversion is the identity — which is why
the ABI tests, not host tests, pin the wire half by number.

**Futexes (`src/sched/futex.rs`).** The correctness argument is the *order*: a waiter is registered
before the value is re-checked, so a wake that lands between the check and the sleep is not lost.
`wait_bitset` returns the three answers a condvar/mutex/`thread::park_timeout` must tell apart
(`0`, `EAGAIN`, `ETIMEDOUT`, plus `EINTR` for a pending catchable signal); `wake_bitset` honours the
bitset contract. The `word_at` read is unaligned and through the direct map on bare metal, reading a
caller address that is not mapped as zero.

**Exit hooks (`src/sched/exit.rs`).** `register`/`register_cloexec` are append-only and
boot-registered; the *order is registration order and is load-bearing*, because most of the
resources involved identify ownership by reference count and the count stops saying who owns what
the moment the batch is released — a hook must run *before* the descriptors are dropped. `run`/
`run_cloexec` snapshot the list under the lock and walk it with the lock released, so a hook taking
its own lock cannot create a lock-order dependency.

## What userspace sees

`sched` owns **no dispatch row** of its own in `docs/uapi/`. The surface it is the engine behind —
`clone`, `fork`, `vfork`, `execve`, `exit`/`exit_group`, `wait4`/`waitid`, `kill`/`tgkill`/`sigqueue`,
`rt_sigaction`, `rt_sigprocmask`, `rt_sigtimedwait`, `sigaltstack`, `futex`, `nanosleep`,
`sched_*`, the POSIX timers, `set_robust_list`, `set_tid_address`, `ptrace`, `setrlimit`/`prlimit64`
— is dispatched in `src/syscall/linux/proc.rs` and `src/syscall/linux/posix.rs`, and the inventory's
rows belong to the syscall module (docs/uapi/ §1, the `clone`/`sched_*` rows and the closed
vocabularies in §3, and the `struct sched_attr` and `struct rlimit` layout rows in §4). The ABI facts
`clone`'s flag bits and the resource-limit model are the `mpos-abi` leaf's (`crate::abi`, re-exported
by `syscall::linux::abi`), not `sched`'s; `runtime.rs` carries a per-task *copy* of the limit table
and reads and writes it through the leaf's `default_rlimits`/`rlimit_admit`. `sched` supplies the
semantics those rows promise: which task a signal lands on, in what order, and what a blocked task
does when it wakes.

## Pins

| Pin | Kind | What it would fail on |
|---|---|---|
| `src/sched/policy.rs` `mod tests` — `round_robin_cycles_within_a_band`, `a_higher_band_runs_before_a_lower_one`, `a_task_wakes_on_the_tick_it_asked_for`, `wake_order_follows_deadlines`, `a_tie_breaks_on_priority_then_id`, `sleeping_zero_ticks_still_waits_for_the_clock`, `ready_and_empty_answer_different_questions`, `waking_a_sleeper_pulls_it_out_of_the_sleep_queue`, `a_woken_sleeper_rejoins_its_own_band`, `waking_a_task_that_is_not_sleeping_reports_failure`, `sleep_saturates_so_u64_max_means_until_woken` | host | The band/round-robin policy, the sleep/wake ordering and its saturation rule |
| `src/sched/signal.rs` `mod tests` — `numbers_are_pinned`, `the_default_action_table_matches_signal_7`, `only_uncatchable_signals_are_refused_a_disposition`, `take_returns_the_lowest_signal_first`, `adding_twice_still_takes_once`, `siginfo_codes_are_pinned`, `is_rt_spans_only_the_real_time_range`, `rt_queue_delivers_every_copy_in_fifo_order_with_its_own_value`, `rt_queue_keeps_numbers_independent_and_ordered`, `rt_queue_drops_and_reports_when_full`, `rt_queue_starts_empty` | host | The signal numbers, the default-action table, `catchable`, and the real-time queue's FIFO/independence/bound |
| `src/sched/futex.rs` `mod tests` — `a_wake_reaches_only_the_address_it_names_and_only_as_many_as_asked`, `a_bitset_wake_reaches_only_intersecting_sleepers`, `a_wake_that_arrives_before_the_sleep_is_not_lost`, `a_task_that_never_waited_is_never_woken` | host | The address-scoped, count-bounded and bitset-filtered wake, and the register-before-recheck ordering |
| `src/sched/exit.rs` `mod tests` — `hooks_run_in_registration_order_and_an_empty_list_is_not_a_special_case` | host | Registration order, the empty-list no-op, and the exit/cloexec list separation |
| `src/sched/mod.rs` `mod tests` — `unshare_gives_a_copy_of_the_names_and_not_the_names_themselves` | host | The one-process `unshare(CLONE_NEWUTS)` copy semantics |
| `user/tests/src/bin/` — `t-futex.rs`, `t-clone.rs`, `t-fork.rs`, `t-exit-group.rs`, `t-wait4.rs`, `t-waitid.rs`, `t-wait-eintr.rs`, `t-kill.rs`, `t-sigaltstack.rs`, `t-signal-delivery.rs`, `t-signal-frame.rs`, `t-signalfd.rs`, `t-sa-restart.rs`, `t-rt-sigaction.rs`, `t-rt-signal-queue.rs`, `t-rt-sigprocmask.rs`, `t-rt-sigtimedwait.rs`, `t-segv.rs`, `t-set-robust-list.rs`, `t-set-tid-address.rs`, `t-nanosleep.rs`, `t-priority.rs`, `t-sched-getaffinity.rs`, `t-timer-create.rs`, `t-timerfd.rs`, `t-ptrace.rs`, `t-ptrace-attach.rs`, `t-times.rs`, `t-fpu-across-switch.rs`, `t-regs-across-syscall.rs`, `t-getpid.rs`, `t-ns-uts.rs`, `t-caps.rs` | `t-*` (std) | The runtime's task, signal, futex, wait, timer and ptrace behaviour end to end on a booted machine — the only pins `runtime.rs` has, since it is machine-gated |
| `user/abitest/src/bin/integration/t-forkcost.rs` | `t-*` (no_std) | Not a contract pin — a `slow`-group instrument whose numbers are `docs/notes/` §10.2's subject |

There is no `t-*` program under `user/abitest` that pins `sched`'s contract: the `no_std` suite's
live programs are `src/bin/integration/*`, and the closest, `t-forkcost`, is a measurement rather
than an assertion (`tests.toml`'s `slow` group says so). So `sched`'s runtime is pinned entirely by
the std-side programs above, and its pure half entirely by the host tests.

## Evidence and open work

The reasoning and the measurements live elsewhere and are not restated here. The task-switch/fork
cost and the "woken task must be made runnable / idle halt belongs to the scheduler's loop" lesson:
`docs/notes/` §10.2 and `docs/ARCHITECTURE.md` §7. The exit-hook inversion that closed the
`sched → net`/`vfs`/`drivers` edges: `docs/notes/` §11.1.3 and `docs/ARCHITECTURE.md` §9 (V-3).
The `ptrace` protocol words' move to the `abi` leaf (`sched → syscall` = 0): `docs/notes/` §13.2.
The four ASLR landmarks and the deliberately-not-randomised `ET_EXEC` address: `docs/notes/` §8.1.
The heap-anchor randomisation gap `t-aslr` names (this kernel's `brk` anchor is not drawn where
Linux draws it): `docs/ROADMAP.md` and `docs/notes/` §15.11 (batch 35). Feature status (signals,
ptrace, resource limits, namespaces) is on the board in `docs/ROADMAP.md` (Phases 2 and 6), and the
userspace verdicts are `docs/uapi/`'s rows. `runtime.rs`'s own named divergences — among them the
dropped real-time record on a full `RtQueue` and the `signalfd` double-record for one child death
attributed to `queue_signal` — are recorded in docs/uapi/ §5.2 and `docs/notes/`, not here.
