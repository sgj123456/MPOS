# MPOS Roadmap

What MPOS is building, phase by phase, and where each phase stands: the objectives,  
the checklist, the remaining work and what to do next — and nothing else.

Every measurement, gate run, lesson and trap behind a claim here lives in the  
[`notes/`](notes/index.md) evidence library, which is where a design decision's reasoning belongs.  
Read this file to know *what to do next*; read `notes/` to know *why the code is the way it is*.  
A `(§N)` pointer on a line here names the section that carries that item's evidence; routing is  
[`notes/index.md`](notes/index.md). An item whose evidence has not been written down says  
*(no evidence file yet)* — that is the roadmap's debt against the notes, not a pointer into them.  
The board states each line as a conclusion plus its pointer: what a line *does*, how it was  
measured and what it cost stays in `notes/`.

Legend: `[x]` done · `[~]` partial · `[ ]` todo · *(blocked: …)* = needs a prerequisite first

---


## Status board

One-glance status of the whole roadmap.

**Core (Phases 1–3)**

- [x] P1 Syscall completion — epoll · timerfd · 67/68 other calls (§1)
- [x] P2 Signal delivery — frame · mask · siginfo · interruptible waits (§2) — with
  `kill(-1)` every process the caller may signal, one copy per process for a group
  (§2.10, item 106), `SIGCONT`'s session door (§2.6), one handler table shared by a
  thread group under `CLONE_SIGHAND` (§2.7) and a fatal signal ending the **whole**
  group through one door (`die_of_fatal_signal`) whichever way it arrives - a kill
  from outside, a self-kill at the `kill` gate's own exit, a task's own pending
  set, or a fault the machine's handler answers itself (§6.77c, behind
  `hal::kernel`) - so a sibling parked in a futex is retired with it and
  `timeout(1)`'s `wait` answers (§6.37, §6.77b, §6.77c, item 123) - and a
  process-directed signal handed to a thread that may take it, so a copy that
  lands on a thread whose mask blocks it is not stuck there while the only
  thread that could take it stays parked (§2.12, item 197), and a stop that is a
  **record** rather than a sleep - the parking task leaves the picker's queues
  and re-checks the record after every wake, so a wake that is not a resume does
  not hand a `SIGSTOP`ped task back with the stop still installed (§6.88, item
  201); the named
  residues are the kernel-spawned program path's `SIG_DFL` start, the
  alternate-stack pin's shape, and one gap that is **closed on both machines**:
  a handled signal used to be handed over only on the return from a *system call*,
  so a task that computed in ring 3 without entering the kernel held it until it
  next called something. Both machines deliver at the trap now - aarch64 since
  2026-11-12 (its EL0 interrupt stub builds a whole `UserFrame` and the entry
  delivers from it before preempting), x86_64 since 2026-11-13 (the PIT entry is a
  hand-written stub pushing a `TrapRegs` block, the one thing the `x86-interrupt`
  ABI cannot give). `t-iret` asserts Linux's answer unconditionally: the loop leaves
  by the signal after 12 067 696 of 400 000 000 iterations on aarch64 and
  153 985 811 on x86_64, where before the fix both ran to their limit (§2.13, items
  261-262). What is left on this surface is narrower and named there: a signal handed
  over at a trap while the task is traced still goes back to the pending set.
  x86_64 keeps the gap because its `extern "x86-interrupt"` frame carries no GPRs -
  `Gate::capture_trap_regs` answers `false`, and `t-iret` pins exactly that
- [x] a signal is sent by someone who may send it — `kill`/`tkill`/`tgkill`/`rt_sigqueueinfo`  
  ask Linux's `kill_ok_by_cred` and `CAP_KILL` in the **target's** user namespace, and the record  
  door keeps the caller's `si_code`, `si_pid`, `si_uid` and `si_value` rather than overwriting  
  them (§2.6, item 91)
- [x] a number names a process or a thread, as the door says — a `kill` number names the
  **process** (a live leader, else the surviving worker, else a zombie group whose number is
  still owned, because Linux keeps a group's `struct pid` while any thread lives and
  broadcasts to it), while a `tkill`/`tgkill` number names the **thread** (a dead leader's
  number answers 0 and delivers nothing, a dead worker's is `ESRCH`) — one door for both had
  answered `ESRCH` to a leaderless group, so a program could not reap the group it had just
  orphaned (§6.77e, item 194)
- [x] a child inherits its parent's signal dispositions — `fork`/`clone` copies the
  parent's handler table and alternate stack, `clone3`'s `CLONE_CLEAR_SIGHAND` serves
  the `SIG_DFL` table instead with `CLEAR|SIGHAND` `EINVAL`, and `exec` resets only the
  caught slots, keeping `SIG_IGN` and the pending set (§2.7, item 96)
- [x] a write to a broken channel is `EPIPE`, and the `SIGPIPE` behind it — a pipe, a
  stream pair and a listener's send run the handler before the call returns (`SI_USER`),
  `MSG_NOSIGNAL` suppresses it, a blocked signal waits pending, and the measured
  exemptions hold: a seqpacket send is `EPIPE` silently, a datagram's dead peer is
  `ECONNREFUSED` once and then `ENOTCONN` (§4.1.13, item 97)
- [x] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3) — with  
  `fstatfs` naming the filesystem the descriptor was *opened on* (§3.6, item 89), every instance  
  carrying a non-zero `f_fsid` (§3.7, item 90), and the access clock moving on read under  
  `relatime`/`strictatime` with `O_NOATIME` honoured (§3.3, item 108), and a `/dev` node born  
  with devtmpfs's own number while the image's udev does the widening (§3.4, item 110), and a  
  pty slave born with its devpts mount's `mode=`/`gid=` and owned by the minting process  
  (§3.8, item 111)

**Networking (Phase 4)**

- [x] 4.1 AF_UNIX — both socket types, with per-message credentials and fd passing; the  
  abstract namespace is served, `unshare`-free lifetime and all (`t-unix`, §4.1.1) — and  
  a path ends at its first NUL, the bytes behind it being the caller's storage (§4.1.5) —  
  and the addresses answer Linux's three shapes, the peer's being the connect's record (§4.1.6) —  
  and a second `connect` moves a datagram's peer, the name being resolved before the refusal (§4.1.7) —  
  and `listen` is the named call, a connected socket not being a listener (§4.1.8) — and  
  `shutdown` is a flag on the socket rather than a handshake with a peer (§4.1.9), and the  
  "implicit rendezvous" it used to name as a divergence was never Linux's shape (§4.1.10) — and  
  `accept`'s address is the connector's, its two pointers asked separately (§4.1.11) — and a  
  peer's close is a hang-up, not silence: `POLLIN|POLLHUP` on stream and seqpacket alike,  
  read 0 after the queued messages, and the wake udev's settle used to wait 120 s for (§4.1.12) — and  
  the other input that wait reads now exists too, `/sys/kernel/uevent_seqnum` answering a counter that
  moves with the events sent (item 149, §7.9c; the boot's 120 s itself is udevd's pending queue, not
  this kernel) —  
  and the sender's half of that close is Linux's too: `EPIPE` with the `SIGPIPE` raised  
  before the call returns, `MSG_NOSIGNAL` to suppress it, and the seqpacket/datagram  
  exemptions as measured (§4.1.13, item 97)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [x] 4.3 TCP — loopback, listener, and the six options; a stream's `SO_RCVBUF` is its receive
  window, inherited by the socket a listener accepts (§4.3)
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)
- [x] 4.5 `AF_PACKET` — link layer as a socket, userland DHCP can ask; both types served —  
  `SOCK_DGRAM` (payload shape) and `SOCK_RAW` (whole frames in, the caller's own header out) —  
  and the wire format's host pin landed (`crates/link`, `fuzz/link_wire`, §4.5); the raw-IP  
  door (`IPPROTO_RAW`) is complete, its unicast half closing with a next-hop/ARP resolver  
  (`src/net/arp.rs`) now (§4.5)

**Memory (Phase 5)**

- [x] anonymous demand paging — a `mmap` is an address space; `mincore` reads the page tables (§5.1, §5.2)
- [x] 5.1 Shared anon `mmap` — aliased frames cross a plain `fork` (§5.1)
- [x] 5.2 `madvise` — `MADV_DONTNEED` releases and refills; deliberate no-ops say why (§5.2)
- [x] 5.3 `process_vm_readv`/`writev` (§5.3)

**Process (Phase 6)**

- [x] 6.1 ptrace (§6.1) · [x] 6.2 real rlimits incl. NOFILE/STACK (§6.2) · [x] 6.4 identity/cap calls, threads via `clone(CLONE_THREAD)` (§6.4)
- [x] teardown at exit, not at reap (§6, R.15)
- [x] a thread is its group's child — its parent is the group's parent, its death notifies nobody,
  a dying task hands its children to an alive task of its group, and a wait walks the caller's
  group (§15.85)
- [x] a group's death is its last thread's — a worker's `exit_group` is reaped as the leader, a
  leader's own exit is a zombie until its group ends, and the death carries the leader's byte
  (§15.86)
- [x] a machine with no room for another task refuses, it does not die — `alloc_slot` answers
  `Option`, both spawn doors return `EAGAIN` the way `clone(2)` does at Linux's own limit, and a
  fork after the crowd is collected works again (`t-fork-limit`; §6.81, item 198). One lane's
  sweep reached that limit because the table was too small for the runner itself: `busybox
  timeout -s KILL <budget>` forks a watchdog that lives for the program's whole budget, so a
  lane of ~50 programs held ~50 slots for two minutes at a time — `MAX_TASKS` is 256 now, at
  8 MiB of stacks (`t-fork-limit`'s width claim; §6.80/§6.84, item 200)
- [x] a dead child's status is kept until it is collected — the store of exit records is as wide
  as the task table (one record per slot can be owed), so forty children dying while the parent
  is not looking are all still there to be waited for, and a caller parked on one of them is
  released (`t-wait-many`; §6.82, item 199)
- [x] a pidfd names a process, not a number — `pidfd_open` records the slot **and** its tenant
  generation, so a reused pid reads "gone"; `pidfd_send_signal` keeps §2.6's permission rule and
  its four scopes, `waitid(P_PIDFD)` resolves the descriptor and honours `O_NONBLOCK`/`EAGAIN`,
  and poll/epoll see `POLLIN` at group death and `POLLHUP` only at the reap (§6.8, item 92)
- [x] a number handed to a syscall is read in the caller's namespace — inside a pid namespace
  the group is `1` and its child `2`, while the slots they occupy are global numbers far above
  both; `handle_prlimit64`'s cross-process half, `live_thread` (seven scheduling calls) and
  `setpriority`'s `PRIO_PROCESS` arm took the number as the slot, so `prlimit64(2)` answered `0`
  with a stranger's table (`cur=1024 max=1048576` where the task named 2 had `123/456`) and
  `setpriority(2, 7)` answered `0` after writing a stranger - a set and a get through one number
  were two different tasks. `getpriority` and `/proc/<n>/limits` were already right, and their
  disagreement with the broken siblings is what made it visible. Each door now resolves with
  `resolve_pid`/`pid_task` first, which changes no reading on a machine that never unshares
  (`t-nsnumbers`, §6.91, item 204)
- [x] an affinity mask is a request about a number, so the number is resolved first —
  `sched_getaffinity`/`sched_setaffinity` were the last two pid-taking calls whose handler bound
  `let (_pid, len, mask) = …` and never read the number, so a collected child, a stranger's number
  and `-1` all answered `0` (`ESRCH` on Linux) and a request about somebody was answered about the
  caller. The rest of the family was measured too: the setter takes **any** length from one byte
  (this kernel demanded a whole word), the getter's length must be a whole number of words (it
  accepted 12), the getter must **not** zero the caller's tail, and the refusal order is asymmetric
  — length before number on the getter, number first on the setter — both of which are what Linux's
  own ordering produces rather than what either direction derives. One `affinity_target` helper now
  resolves through `resolve_pid` for both, and `affinity_may_retarget` adds the authority rule —
  measured, the setter needs `CAP_SYS_NICE` only for **another user's** task (a same-uid stranger's
  child is served, so blood relation is not the rule) and the **getter asks for nothing at all**,
  which corrects an earlier claim here that it needed a ptrace check; the order is `EFAULT` →
  `ESRCH` → `EPERM` → `EINVAL`, so authority sits before the mask judgement (`t-affinity-rules`,
  §6.92, item 205)
- [x] `setpriority` asks whose task, and says so with two different errnos — no arm of
  `handle_setpriority` had an authority check, so a program running as one user could lower
  **another** user's task's nice, get `0`, and have it written (a later `getpriority` on that task
  then read the intruder's value). The doc comment said the lowering rule was "moot for this
  single-rooted kernel", which was inherited rather than measured. Linux answers **two** refusals
  to two questions: `EPERM` is ownership (a task that is not the caller's, in *either* direction —
  lowering a stranger's nice is refused just as raising is), and `EACCES` is direction (the task
  *is* the caller's but the value raises its priority, which needs `CAP_SYS_NICE`; the row that
  would let a caller without the capability raise as far as `RLIMIT_NICE` allows is carried now
  (§6.94) but not yet read here, so on this machine the capability is still the whole rule). Not blood relation: a
  scan of 484 same-uid targets on the host answered `0` for an equal-nice call on every one of
  them, parent and stranger alike. `PRIO_USER` is stricter than `PRIO_PROCESS` upstream, so both
  collection arms test ownership per member and refuse the **whole** set — nothing half-written.
  `nice_may_retarget`/`nice_may_raise` in `src/syscall/linux/posix.rs` are the two predicates, and
  the pin drops a child to `nobody` and asks about its own root parent (`t-nice-authority`, §6.93,
  item 206)
- [x] `RLIMIT_NICE` and `RLIMIT_RTPRIO` read `0/0`, the way Linux answers them — the two rows sat
  past the ones `default_rlimits()` fills, so they kept the table's infinity starting value. That
  is a promise rather than a default: `0/0` is the row `can_nice` reads to refuse a priority raise
  to a caller without `CAP_SYS_NICE`, and a caller that reads `infinity` there is told the opposite
  of what the kernel will do. `RLIMIT_RTPRIO`'s zero says the same about realtime priority. Both
  now read `0/0`; `t-prlimit64` pins the two rows (`RLIMIT_NICE reads 0/0, the row Linux answers`,
  `RLIMIT_RTPRIO reads 0/0, the row Linux answers`) and both claims are red on the kernel without
  the two lines (§6.94, item 207). The row's **reading** half — `can_nice` inside `setpriority` —
  is not implemented and is named as the open half: on this machine the row can only be lifted by
  root, which holds the capability anyway, so the check would be a line no fixture could tell apart
  from its absence until `rlimit_admit` learns `CAP_SYS_RESOURCE`
- [x] `getrusage` serves all three `who` values, and the third is the thread's own CPU — `RUSAGE_SELF`
  (0), `RUSAGE_THREAD` (1) and `RUSAGE_CHILDREN` (-1) are three constants Linux serves; this kernel
  answered `EINVAL` for the middle one, which is the answer for a malformed argument given to a valid
  constant and leaves the caller no fallback. Serving it needed a counter that did not exist: the
  group's CPU lives on the shared `Mm` (that is `RUSAGE_SELF` and `times()`), and nothing recorded a
  *thread*'s. The live `Task` now carries `cpu_user`/`cpu_system`, charged in the same `charge_cpu`
  call that adds the tick to the group, so the two doors cannot disagree about how many ticks there
  were; `t-times` pins four claims — the call succeeds, a spinner thread's CPU lands in `RUSAGE_SELF`,
  it is *not* charged to the idle main thread's own record, and that record does grow when the main
  thread itself burns — with the first two red on the kernel without the fix (§6.95, item 208)
- [x] `/proc/<pid>/stat`'s CPU fields carry the CPU the kernel charges — fields 14-17 (`utime`,
  `stime`, `cutime`, `cstime`) were a hardcoded `0` under a comment saying this kernel did not account
  CPU, a sentence overtaken twice: `wchan` had already stopped being zero and the timer interrupt has
  charged CPU since `times()` landed. Measured on a current host with a spinner thread, `/proc/<pid>/stat`
  field 14 tracks the **group's** user time (`66` USER_HZ ticks = 0.667 s) and not the reading thread's
  own (0.030 s) — Linux's `whole` flag, set for the process file and for a thread's own `/proc/<tid>/stat`,
  clear only for `/proc/<pid>/task/<tid>/stat`. The snapshot now carries the group's four numbers and
  the per-thread pair (§6.95's counters), `stat_line` takes the flag from its caller, and the fields are
  scaled to `USER_HZ` the way `times()`/`getrusage` already answer; `t-times` pins field 14 against
  `RUSAGE_SELF`, field 16 against a reaped child's folded CPU, and the `task/<tid>` spelling against the
  group's — the first two red on the kernel without the fix (§6.96, item 209)
- [x] the clock family says what it delivers, and the coarse clocks are coarse — `clock_getres`
  answered **one tick (4 ms) for every clock** under a comment saying the tick counter was the only
  time source there was, a sentence the neighbouring `clock_gettime` had already overtaken: it
  interpolates the free-running counter, and the measured `CLOCK_MONOTONIC` step was 616 ns while
  `CLOCK_MONOTONIC_COARSE` stepped 352 ns - so the resolution was four million times coarser than the
  readings and the coarse clock was not coarse at all. Measured on the host: the counter clocks resolve
  to `1 ns`, the two coarse ones to the tick, and 5000 back-to-back coarse reads move **0** times. The
  id set was also three shorter than `clock_gettime`'s, so `clock_getres(CLOCK_TAI, …)` answered
  `EINVAL` for a clock this kernel serves, and `CLOCK_BOOTTIME_ALARM` read as wall time. `Clock::
  resolution_nanos` (the counter's quantum: `1 ns` on x86_64's TSC arithmetic, `ceil(10^9/CNTFRQ)` on
  aarch64) now answers for the fine clocks and the tick for the coarse pair, and the coarse pair reads
  whole ticks; `t-clock-getres` pins all of it, six claims red without the fix, and its two
  timing-shaped claims were then re-anchored to the run they measure - an emulated aarch64 read them as
  the host's speed rather than as the clock's face (`§6.97`, item 210)
- [x] the two CPU-time clocks are CPU time, not machine uptime — `CLOCK_PROCESS_CPUTIME_ID` and
  `CLOCK_THREAD_CPUTIME_ID` sat in the monotonic list and were filled from `since_boot_nanos()`, so a
  process 1.1 s into a boot read `cpu=1113219 us` where `RUSAGE_SELF` said `4000 us`, the "CPU time"
  advanced `301038 us` across a 300 ms sleep, and two threads of one process read the same number
  (both were the machine's age). Measured on the host: the process clock equals `RUSAGE_SELF`'s
  user+system (`46409` vs `46410 us`), a 300 ms sleep moves it `9 us`, a sibling's burn lands in the
  process's clock and not the thread's, and the resolution is `1 ns` because Linux charges CPU in
  nanoseconds. Both ids now read this kernel's own counters (`utime + stime`, children out;
  the thread's own pair) scaled by `ns_per_tick()`, and `clock_getres` answers that tick for them - a
  resolution is a promise about the readings (§6.97). `t-cpuclock` pins it, four of five claims red
  without the fix, and the gate then caught `t-ns-time` bracketing this clock between two uptime
  readings - a bracket a CPU clock Linux restarts at zero in the fork cannot fall inside (parent
  `242041816 ns` against child `12794 ns`), so that row is judged by `getrusage` in the child now
  (`§6.99`, item 212)
- [x] `/proc/loadavg` and `sysinfo.loads` report the load average Linux computes — both doors printed
  zeros, and `/proc/loadavg` was a literal string (`0.00 0.00 0.00 1/<processes>`) under a
  `docs/uapi/` row saying the file was served, so a caller reading "the machine is idle" was told a
  legal number that was never computed. A load average is a decayed integral in fixed point
  (`FSHIFT 11`, `EXP_1/5/15`, `calc_load`) resampled on a five-second boundary, and it is **one**
  array: Linux's `/proc/loadavg` text is it rounded to hundredths and `sysinfo.loads` is the same
  array shifted by five bits. Measured: only the 5 s boundary moves the three averages (40 samples at
  100 ms), and `loads[0] = 127456` is `1.9449 << 16` of the `1.94` the file prints. `src/sched/
  loadavg.rs` now carries the arithmetic; `charge_cpu` - the tick hook, already under the scheduler's
  lock - samples it and feeds `ready + uninterruptible + running` (uninterruptible sleepers now say
  so), and both readers read that array; the fourth field's denominator became threads, as Linux
  counts. `t-loadavg` pins it, three claims red without the fix (`§6.98`, item 211)
- [x] `/proc/stat` is counters, and the lines a monitor parses exist — the file printed the machine's
  **uptime in seconds** as the `cpu` line's user column (`final cpu  12 0 0 0 …` twelve seconds into a
  boot, measured), with `system`/`idle` hardcoded zero, `intr 0` and `ctxt 0`, and no `btime`,
  `processes`, `procs_running` or `procs_blocked` line at all, while every monitor subtracts two
  readings of exactly those fields. `src/sched/cpustat.rs` now keeps what the kernel can honestly count
  - user/system/idle ticks from the tick, tasks made in `alloc_slot`, switches in `schedule` - and the
  file prints them in Linux's ten-column shape with `cpu0`, `ctxt`, `btime`, `processes`,
  `procs_running` and `procs_blocked`. The question the pin settled on its first boot was what "idle"
  *is*: the scheduler's own loop is the current task whenever nothing runs, so every reader now asks
  `current != SCHED_ID` - which also stopped the load average flooring an idle machine at about 0.9 and
  `procs_running` at one. **Named gaps:** the nice/iowait/irq/softirq/steal/guest columns print zero
  because this kernel keeps no such dimension (interrupt time is charged to what it interrupted; Linux
  times handlers out of system separately, which needs per-handler timing), and the `intr` line is
  absent rather than zero because there is no per-vector count - a zero would claim the machine never
  took an interrupt. Pin `t-procstat`, seven of nine claims red without the fix (§6.100, item 213)
- [x] `/proc/<pid>/stat`'s nice pair are the stored nice, not two literals - fields 18/19 read
      `20 + nice` and `nice` where they used to be the constants `20`/`0`, so a `ps` on a reniced
      process no longer prints nice 0 (measured: nice 19 reads `39`/`19` on a current host; the
      machine's own process set to 3 reads `23`/`3`, `20`/`0` without the fix — `§6.101`, item 214).
- [x] `/proc/<pid>/stat`'s four signal words are the kernel's sets - fields 31-34 (`signal`,
      `blocked`, `sigignore`, `sigcatch`) printed `0` while `Task::signals`, `Task::blocked` and the
      group's dispositions held every one of them; blocking `SIGUSR1` now moves field 32 by `1 << 9`,
      a `SIGTERM` handler puts `1 << 14` in field 34, a raised-and-blocked `SIGWINCH` puts `1 << 27`
      in field 31, and the two disposition words partition (measured on a current host first, because
      neither machine starts at a zero word: both ignore `SIGPIPE` - `§6.102`, item 215).
- [x] `vsize` and `statm`'s total are the address space, not the resident set - `/proc/<pid>/stat`'s
      field 23 and `/proc/<pid>/statm`'s first field were both formatted from `rss_pages`, so a
      process that mapped 4 MiB and touched none of it was reported as unchanged (measured on a
      current host: `statm` total `63 -> 1087` pages with `resident` fixed at `24`; the machine's
      own probe moved `total` only when the pages became present). `Mm::mapped_pages` sums the VMA
      table and the eager ranges now, and `statm`'s second field stays the resident count it was.
      **Named gap:** `statm`'s `shared`/`text`/`library`/`data`/`dirty` print zero because this
      kernel keeps no accounting for them (a plausible number is worse than an honest zero, and
      `data` used to print the resident count) - `§6.103`, item 216, pin `t-statm`.
- [x] `starttime` is the moment the task was made — field 22 was the literal `0`, so `ps` computed
  every process's age as the machine's (`btime` is served now, and the subtraction is what `ps`
  does). The field is a record: `Task::start_nanos`, written where a task is constructed, rendered
  in Linux's `USER_HZ` ticks and truncated the way `nsec_to_clock_t` truncates. Measured on the host:
  two children a second apart read counts a hundred ticks apart; on the machine `Some(150) ticks over
  Some(1510) ms`. Six claims in `t-starttime`, four red without the record (`§6.104`, item 217).
- [x] `getrusage`'s switch counters are the switches the scheduler took — `ru_nvcsw`/`ru_nivcsw`
  were zeros in every `struct rusage` this kernel wrote (both `wait` doors and all three `getrusage`
  `who` values), beside a CPU pair it has counted for years, so a program asking "did I block or was
  I preempted" was told "neither, ever". The scheduler has both facts because every switch goes
  through one function: `Task` keeps the thread's pair, `Mm` the group's own and children's columns,
  and `Table::hand_over` decides the column once, after the caller has said where the task is going —
  left the ready queue without exiting → voluntary, still runnable → involuntary, exiting → neither.
  Measured on Linux 7.2.14: 200 sleeps are `+200/+0`, 200 yields `+0/+1`, a worker's 100 sleeps move
  the process's pair and not the main thread's, and `RUSAGE_CHILDREN` folds a waited child's beside
  `cutime`. Pin `t-rusage-switch` (seven movement claims; five red without the fix), and the twelve
  page-fault/block-I/O counters before them stay zero as a **named gap** — the minor/major split
  needs an I/O signal the mapped-object seam does not report (`§6.105`, item 218).
- [x] `/proc/<pid>/status`'s mask and switch lines are the kernel's — six lines were literals
  (`SigPnd`, `ShdPnd`, `SigBlk`, `SigIgn`, `SigCgt`, and the two `*_ctxt_switches`) while `stat`'s
  fields 31–34 and `getrusage`'s pair (§6.102, §6.105) carried the same facts at another door, so
  `top` and `ps` read zeros for a task whose masks and switches were known. Measured on Linux: masks
  are sixteen lowercase hex digits with signal `n` at bit `n - 1`; the disposition words are a
  partition; `ShdPnd` is the group's set, which this kernel answers from the leader where a
  process-directed signal lands; and the switch lines are the **thread's** pairs — a worker's fifty
  sleeps read `50` in its own `task/<tid>/status` while the main thread read `16` and
  `RUSAGE_SELF` said `106`. Pin `t-status-masks` (eleven claims, seven red without the fix), plus a
  host test for the notation (`§6.106`, item 219).
- [x] `/proc/<pid>/status`'s identity lines are the kernel's — `Umask`, `TracerPid`, `NoNewPrivs`,
  `Seccomp`, `Seccomp_filters` and the five `Cap*` words were literals while the kernel held every
  fact behind them. The capability words were right for a root payload and wrong for everything
  else: a dropped program read the full set it no longer held. Measured on Linux: `Umask:` is four
  octal digits, `TracerPid:` is the tracer's number in the reader's namespace (`0` when unattached),
  `Seccomp:` is a mode with `Seccomp_filters:` its depth, and the `Cap*` words are sixteen lowercase
  hex digits off the credential masks. Pin `t-status-identity` (ten claims, a dropped-identity child
  so a constant cannot pass, `capget`/`PR_CAPBSET_READ` as the second and third doors; five red
  without the fix), plus host tests for the two notations (`§6.107`, item 220).
- [x] `/proc/<pid>/status`'s `Groups:` line lists the supplementary groups — it was a bare tab for
  every task while the identity record has carried `setgroups(2)`'s list. Linux's rendering is a
  promise about separators as much as membership (`10 20 30 `, a space after every gid including the
  last), so `groups_line` writes the trailing space and the host test pins it. Pin: three claims in
  `t-status-identity` (clear, list in order, `getgroups` agreeing; one red without the fix). The same
  commit registers two notation tests `5b28889` left without their `#[test]` attribute — the host
  count not moving was the tell (`§6.108`, item 221).
- [x] `/proc/<pid>/status`'s four `Vm` lines are four different books — they were one `rss_pages`
  number printed four times, so a 4 MiB mapping nobody had touched moved none of them and unmapping
  a touched one made the peaks "unhappen". Measured on Linux 7.2.14: map → `VmSize`/`VmPeak` 4344
  kB with `VmRSS` still 88 kB, touch → `VmRSS`/`VmHWM` 4184 kB with `VmSize` unchanged, unmap →
  `VmSize`/`VmRSS` fall back while `VmPeak`/`VmHWM` keep their records. `Mm` now records both high
  waters where its tables grow (`own()`, `vma_insert`, `record_range`, `set_vmas`), and `vm_lines`
  derives the four kilobyte values. Linux's `VmHWM` is a synced record that was measured *dipping*
  4188 → 4112 kB across a `munmap`, so the pin asserts the separation, not a strict monotonicity
  Linux does not have. Pin `t-vmstatus` (14 claims, five red without the fix) plus a host test on
  the four-input derivation. `VmData`/`RssAnon` and the other `Vm*` lines stay a **named gap** —
  the VMA table cannot yet tell writable data from the stack (`§6.109`, item 222).
- [x] `/proc/<pid>/status` counts `FDSize` and `SigQ` instead of printing `64` and `0/64` —
  `FDSize` is the fd table's 64-slot capacity step (`64 → 128 → 256 → 512` as `dup` grows it,
  never shrinking on `close`, `1024` after `dup2` to 1000), which is the `slot_count()` high water
  this kernel already kept; `SigQ` is two different books (the per-user queued RT total over the
  *read task's* `RLIMIT_SIGPENDING.cur`), so five queued copies move the numerator and lowering
  the soft limit moves the denominator while the numerator may stay larger. Both come from the
  kernel's own `queued_for_user`/rlimit charge, not a second sum. Linux's `SigQ` numerator has a
  baseline of 1 this kernel honestly lacks, so the pin asserts movement. Pin `t-status-counts`
  (9 claims, eight red without the fix) plus a host test on the two pure derivations; `Ngid`,
  `CoreDumping`, `Cpus_*`/`Mems_*` and the other `Vm*` lines stay a **named gap** (`§6.110`,
  item 223).
- [x] `/proc/<pid>/stat`'s field 25 (`rsslim`) is the task's `RLIMIT_RSS` soft row, not a literal
  `0` — Linux prints it as the bare `u64` word (`18446744073709551615` for infinity, `4194304`
  after `setrlimit(RSS,{4 MiB,max})`, back to the infinity word on restore), and a child that
  lowers *its own* ceiling to 2 MiB reads `2097152` in its own file while its parent's still reads
  `4194304`. The row is named `RLIMIT_RSS = 5` in `crates/abi` and read from `task.rlimits` in the
  same `snapshot_of` look that serves `SigQ`; the rendering is a host-testable pure function
  because `stat_line` itself is machine-only. Pin `t-stat-rsslim` (6 claims, five red without the
  fix) plus a host test; an `i64` field parser cannot hold the infinity word, which is how the
  first Linux probe's `-1` appeared (`§6.111`, item 224).
- [x] `/proc/<pid>/stat`'s field 52 (`exit_code`) is the encoded `wait4` status of the zombie, not
  a literal `0` — Linux prints `10752` (`42 << 8`) for a child that called `exit(42)`, `15` for
  `SIGTERM`, `9` for `SIGKILL` and `139` (`WTERMSIG 11 | WCOREDUMP 128`) for `SIGSEGV`, each
  identical to the word `waitpid` reports afterwards; a live task and a group whose leader died
  while a sibling still ran both read `0`. The encoding is now one definition in the `crates/abi`
  leaf (`wait_status`), shared by `wait4` and by `sched`'s `retired_status` (which reads the whole
  `Retired` record, including the `dumped` bit that a raw `code` does not carry); `stat_line` only
  prints that field, so no new `vfs → syscall` edge appears. Pin `t-stat-exitcode` (11 claims,
  eight red without the fix — every death claim) plus host tests on the leaf and the pure
  rendering (`§6.112`, item 225).
- [x] `getrusage`'s `ru_maxrss` is the resident-set high-water mark, not a literal `0` — Linux
  reports it at byte 32 in KB (`8204` after touching 8 MiB, unchanged by an untouched mmap and by
  `munmap`), `RUSAGE_THREAD` equals `SELF` in a single-threaded process, `wait4`'s rusage reports
  the child's own peak OR the largest peak among the children *it* reaped (`2060` own → `8204`
  reported), and `RUSAGE_CHILDREN` is the **MAX** of reaped children's peaks (a smaller new child
  never lowers it). §6.105 named it a gap for want of a high-water record; §6.109's `Mm::hwm_frames`
  made it closable, so `Mm` now keeps `cmaxrss` beside it, `Retired`/`WaitEvent::Exited` carry the
  peak through reap, and the three pure derivations live in the `crates/abi` leaf (the wait/rusage
  doors are machine-only). Pin `t-rusage-maxrss` (10 claims, four red without the fix — all in
  freshly forked processes, because a fresh child inherits its parent's peak and a container's own
  baseline is ~1.09 GB); `t-wait4`'s "twelve unmeasured counters" claim moved to eleven with a new
  positive-peak claim. The remaining ten counters stay an honest, named zero — the minor/major
  fault split needs `MappedObject::frame_at` to report device I/O (`§6.113`, item 226).
- [x] the seven `sched_*` doors read `pid` as Linux's signed 32-bit `pid_t` —
  a negative word is `EINVAL`, a positive number naming nobody is `ESRCH`, and
  the full words `2^32`/`2^32-1` truncate to self/`-1`. Before it,
  `live_thread`'s `pid < 0` guard was dead code: every handler passed
  `args[0] as isize` (a 64-bit register), so userspace `-1` became a huge
  positive number and all seven doors answered `ESRCH`. `sched_pid_verdict`
  now truncates and refuses in host-compiled `abi.rs`; `sched_target` resolves
  the rest. Pin `t-sched-pid` (6 claims, all seven doors each; three red
  without the fix — `[3 × 7]` for the negative words). The doors' refusal
  *ordering* (`sched_getparam` checks its buffer before the pid,
  `rr_get_interval` checks the pid first) and `rr_get_interval`'s
  configuration-dependent slice value stay named, unmeasured gaps
  (`§6.114`, item 227).
- [x] the five buffered `sched_*` doors check their buffer **before** the pid
  — Linux's shared `if (!uattr || pid < 0) return -EINVAL` runs before the
  task lookup, so a NULL buffer is `EINVAL` even for a nonexistent pid (the
  failed lookup's `ESRCH` must not win), and `setscheduler(0, FIFO, NULL)` is
  `EINVAL` rather than `EOPNOTSUPP`. §6.114 fixed the pid word but left each
  handler resolving the pid first, so MPOS answered `EFAULT`/`ESRCH` for
  those rows. `sched_buffered_pid_verdict` (host-tested in `abi.rs`) now adds
  the NULL-buffer EINVAL before the pid is returned; `sched_rr_get_interval`
  keeps §6.114's pid-first order as the deliberate exception. Pin
  `t-sched-order` (8 claims; three red without the fix — `[14 × 5]`,
  `[3 × 5]`, `[14, 95, 95, 22]`). The `sched_*attr` `size`/`flags` guard
  order, bad-pointer precedence, and `rr_get_interval`'s configured slice
  value stay named, measured gaps (`§6.115`, item 228).
- [x] the two `sched_*attr` doors negotiate a 32-bit `size`/`flags` word — Linux 7.2.14 reads
  both as `unsigned int`, bounds `usize` by 48..`PAGE_SIZE` (short/over-page `EINVAL` on
  `getattr`, **`E2BIG`** on `setattr`'s in-struct size, both *before* the pid lookup), and puts
  `flags` on opposite sides of the lookup (`getattr`: after it, so `flags=1` on a nonexistent pid
  is `ESRCH`; `setattr`: before it, so the same pair is `EINVAL`). MPOS resolved the pid first,
  compared the full 64-bit words, refused short sizes with `EINVAL`, accepted sizes past a page,
  and treated `attr.size=0` as too small instead of Linux's "use the kernel's version". The three
  verdicts live host-side in `abi.rs`; `posix.rs` shares one `sched_resolve` and gives each door
  its measured order. Pin `t-sched-attr` (12 claims; 8 red without the fix), `§6.116`, item 229.
- [x] a death belongs to the parent that had it — the record of an exit outlives the task, so it
  now carries the owning **group** (`Retired::parent`), written when the death is recorded and
  retargeted to the adopter's group when the reparenting walk moves the corpse. Before it, the four
  readers asked the *slot*, and a slot is empty precisely *because* the record outlived its task, so
  a stranger's `waitpid` collected a death it had nothing to do with and the real parent was left
  with `ECHILD` for its own child (`MY waitpid(7, WNOHANG) = 7 errno=0` where Linux answers
  `ECHILD`; `t-steal`, §6.90, item 203)
- [x] **a pid number is a promise, and here it is the task slot** — a number I held for a child I
  already collected was handed straight to the next `fork`, so `kill(<it>, SIGTERM)` ended an
  unrelated process and `/proc/<it>` read that process (`kill(7, SIGTERM) = 0 errno=0 (NOT ESRCH)` /
  `/proc/7 state=Z`, where Linux answers `ESRCH`/`ENOENT` - `build/scratch/reapwait/kslot.c`, §6.89).
  **Landed:** the init namespace now allocates numbers from a monotonic table instead of numbering by
  slot — the four `is_root()` short-circuits in `crates/pidns/src/lib.rs` are gone, and the crate's
  `the_init_namespace_numbers_by_slot` pin is replaced by two that pin the rule (numbers ascend, a
  spent number is spent, an unregistered slot has none). The machine's init is registered as the
  namespace's first number, which is what pid 1 is; the first boot without that died
  `INIT_EXITED: the system has no init left`, because its first child was handed number 1. Changing
  what a number *is* then turned the gate into a probe: `18` user programs red, then `9`, then `1`,
  then `0` as six doors that had conflated a **number** with a **slot** were fixed — a signal's
  `si_pid`, `fork`'s answer, `set_tid_address`, netlink's autobind id, `IFLA_NET_NS_PID`, and
  `cgroup.procs`/`cgroup.threads` on both the read and the write side. The seventh was the
  credential payload: `UCred` stored a slot and resolved it at the read, which answers `0` for a
  sender that has been reaped (and a stranger's number once the slot is reused — `t-udev-worker`'s
  forked worker is the measurement), so it now carries the **number captured when the peer was met**
  plus the pid namespace it was minted in and answers `pid_vnr` — the number inside that namespace,
  `0` outside it. `t-stale-pid` (8 claims, **6 red** counterfactually, `build/boot-stalepid-cf1.log`)
  pins the allocator; the existing `t-signal-frame`, `t-udev-worker` and `t-cgroupfs` pins are the
  witnesses for the doors (§6.121, item 235)
- [x] a pidfd can fetch a descriptor — `pidfd_getfd` duplicates the target's **open file
  description** into the caller's table, with Linux's refusal order (flags, pidfd, task,
  authority, target fd), `FD_CLOEXEC` unconditionally and `ESRCH` for a zombie or a leader
  that exited under a live worker (§6.9, item 93)
- [x] the door that names its own child — `clone3` (435) serves a versioned
  `struct clone_args` and `CLONE_PIDFD`, so a process and a descriptor naming it
  come back in one call (no `pidfd_open` window); `stack` is the child region's
  **base**, the size decides which fields exist, and `CLONE_DETACHED` is refused
  as Linux's own `clone3` refuses it (§6.10, item 94) — and the struct's last
  word, `cgroup`, is served now too: `CLONE_INTO_CGROUP` is `EBADF` for a node
  that is not a cgroup directory, `ENOENT` for a removed group or one outside
  the caller's cgroup namespace, `EACCES` from the common ancestor's
  `cgroup.procs`, `EOPNOTSUPP` beside `CLONE_THREAD`, and the child (and its own
  fork) is born in the group the descriptor names, with `CLONE_NEWCGROUP` rooted
  at that group (§6.26, item 105)
- [x] an anonymous descriptor has no file type — `eventfd`, `timerfd`, `signalfd`,
  `epoll`, `inotify` and `pidfd` report `st_mode` with the `S_IF*` bits clear
  (`0600`, or `0700` for a pidfd), `/proc/self/fd` reads back its own
  `anon_inode:[…]` name (`anon_inode:inotify` being the unbracketed one), and a
  zero-length read answers Linux's errno, `inotify` consulting its queue first
  (§6.11, item 99)
- [x] an eventfd's write wakes the pollers parked on it — `epoll_wait`/`poll`/
  `select` register through `poll_subscribe`, which `EventFd` did not implement, so
  a cross-thread wake through the call `async-io`/`zbus` wake their loop with was
  lost and the task woke only at its timeout (measured 3000 ms against 0-1 ms for a
  pipe/socketpair); the compositor's thirteen-second park and the frame that
  appeared at `SIGINT` were downstream of it (§3.12, item 124)
- [x] a timerfd's expiration reaches an `epoll_wait` park — its readiness is the
  clock's own work, so no task calls a wake; `poll(2)` has always kept a one-tick
  floor under every timeout and `epoll_wait` did not, so a 150 ms timer answered at
  the wait's own 3000 ms timeout (153 ms with the floor; a `signalfd` measured not
  to need it) (§3.13, item 125)
- [x] 6.3 Namespaces — **all eight of `namespaces(7)`'s kinds served** — UTS, MNT, **PID**, **NET**, **TIME**, **USER**, **IPC** and **CGROUP** landed, each with its pin  
  (`t-ns-uts`, `t-ns-mnt`, `t-ns-pid`, `t-ns-net`, `t-ns-time`, `t-ns-user`, `t-ns-ipc`, `t-ns-cgroup`): the numbers a process  
  sees are the  
  reader's namespace's,  
  both directions of the translation and `/proc` rendered per reader (§6.3); **the task a namespace  
  is spoken for by has rules of its own** — its members cannot end it (only an ancestor's  
  SIGKILL/SIGSTOP, or a signal it has a handler for), and its death takes every task it speaks for,  
  nested namespaces included (§6.3, measured), the kind's own **creation rule** is Linux's too - a  
  process makes a pid namespace **once**, and one whose init has exited takes no tasks (§6.3,  
  measured) - and `/proc/<pid>/status`'s `NStgid`/`NSpid`/`NSpgid`/`NSsid` carry the **chain** of  
  numbers the task is known by rather than the reader's one (§6.3, measured); a child cloned with  
  `CLONE_NEWNET` gets a stack of its own - its own `lo`, no wire, and a port space the machine's  
  cannot see - and the kind is served by all three calls, with **a socket remembering the stack it  
  was made in** (`net`'s handle map): a socket outlives an `unshare`, an inherited descriptor keeps  
  working in a cloned child, and a descriptor handed over by `SCM_RIGHTS` arrives intact and  
  answers the sender's address (§6.3). Beside the IP stack, **`AF_UNIX`'s names are the namespace's  
  where Linux makes them so**: an abstract name belongs to the namespace of the socket that holds  
  it (the name binds again in another, and neither namespace can dial the other's), a *path* name  
  is not namespaced at all and crosses both ways, and what keys the abstract half is the socket's  
  namespace rather than the calling task's (§6.3, measured). A device can move: `eth0` enters a  
  namespace (`ip link set eth0 netns N`, served 2026-10-16 - down and unconfigured on arrival,  
  which is Linux's shape measured, §6.3); a vsock stack stays machine-wide - which is Linux's own  
  shape, measured, with the transport question named (§6.3); and **the clocks can be moved**:  
  `unshare(CLONE_NEWTIME)` arms the caller's *children* slot and never moves the caller, the offsets  
  are written through `/proc/self/timens_offsets` and frozen by the first task to enter, a child's  
  `MONOTONIC`/`MONOTONIC_RAW`/`MONOTONIC_COARSE`/`BOOTTIME` read through them while REALTIME, TAI  
  and the cputime clocks do not, and `setns` is the second door into the same slot (`t-ns-time`  
  walks it in 46 claims, the shape measured whole before any of it was built, §6.3). The other  
  two kinds Linux has left - IPC and cgroup - have no entry at all here (§6.3, §6.6), which is  
  `vfs::ns::Entry::ALL` against `namespaces(7)`'s list of eight. **USER joined on 2026-10-16**: it  
  moves its caller like NET, the map starts empty so the caller lands on the overflow id, `uid_map`  
  takes one self-naming line and then answers `EPERM`, and the four `getuid`-family readers  
  translate the stored id through the task's namespace (`t-ns-user`, 46 claims). The same  
  translation renders what a *file* reports now: `stat`/`fstat`/`fstatat`/`statx` map the stored  
  owner through the reader's namespace, so a file created before an unshare reads the stored id  
  from the machine map and the inside id from the new one, `statx` agreeing with `stat` (§6.3,  
  measured on the host first). It renders what a *socket* reports too (2026-10-16): `SO_PEERCRED`  
  and the `SCM_CREDENTIALS` a read carries map the peer's stored ids through the reading task's  
  namespace, at the read rather than at capture (§6.3, measured on the host first) - and the same  
  struct's `pid` through the reader's *pid* namespace, `pid_vnr`'s rule, so a reader inside one  
  names a peer there and reads `0` for a peer it cannot see (2026-10-16, `t-ns-pid`). And the rung's  
  other half landed too: entering a namespace **is the  
  grant** - `unshare`/`setns` hand the maker the full effective, permitted and bounding sets,  
  measured on the host across all four doors, so a caller that drops its capabilities first reads  
  them back the instant it is in a namespace it made (§6.3). And the **child-side creation** landed  
  (2026-10-16): `clone(CLONE_NEWUSER)` makes a child born already inside a namespace of its own -  
  the overflow id until its map is written, the full set at birth, and the parent's `CLONE_NEWUSER`  
  lifting the capability gate for the other kinds in the same word (§6.3, measured on the host  
  first). And the **netlink/uevent family's own `ucred`** landed (2026-10-16): its ids render  
  through the reading task's user namespace - the kernel's synthetic zero record included - and its  
  pid is the *sender's* number in the sender's own pid namespace, resolved once at send, which is  
  also the port id a zero-`nl_pid` bind prefers (`t-uevent`, §6.3)

**ABI gaps → `ENOSYS` (remaining)**

- [x] inotify · [x] `memfd_create` (§7 Stage 3) · [x] xattrs — `user.*` is stored on the memory filesystems (`ramfs`/`tmpfs`) and charged to the mount's room, every other namespace and every filesystem with no store refused by name (items 132/139, §3.15/§3.20)
- [x] POSIX message queues — `mq_open`/`mq_unlink`/`mq_timedsend`/`mq_timedreceive`/
  `mq_notify`/`mq_getsetattr`, priority-ordered and bounded by `RLIMIT_MSGQUEUE`;
  `t-mqueue` pins 41 claims measured on the host first (§6.12) — and **System V
  shared memory** — `shmget`/`shmat`/`shmdt`/`shmctl` with Linux's slot-and-
  generation numbers, marked-not-removed `IPC_RMID`, an attachment count folded
  out of every address space (so a `fork` counts), and `/proc/sysvipc/shm`;
  `t-sysv-shm` pins 61 claims measured on the host first (§6.13, item 101) — and
  **System V semaphores** — `semget`/`semop`/`semtimedop`/`semctl` with waiting,
  `SEM_UNDO` applied at the process's death and split by
  `unshare(CLONE_SYSVSEM)`, and `/proc/sysvipc/sem`; `t-sysv-sem` pins 74 claims
  and seven host tests (§6.14, item 102) — and **System V message queues** -
  `msgget`/`msgsnd`/`msgrcv`/`msgctl` with Linux's FIFO-plus-selection rule
  (`type < 0` takes the *lowest* fitting type, not the front-most), a receive
  that waits for a message it can fit (`E2BIG` only under `IPC_NOWAIT`),
  `MSG_COPY` by queue index, the `msg_qbytes` budget with its asymmetric
  `IPC_SET` gate (a raise needs `CAP_SYS_RESOURCE`, a lowering below the
  queued bytes is accepted), a receive's `msgsz` gated by its sign (`LONG_MAX`
  accepted, `LONG_MAX + 1` `EINVAL`) with an unwritable destination losing the
  message to `EFAULT` as Linux loses it, and `/proc/sysvipc/msg`;
  `t-sysv-msg` pins 84 claims and eight host tests (§6.15, item 103).
  `CLONE_NEWIPC` **landed on 2026-10-23** - the kind moves its caller, the four
  tables key their state by it, a number restarts at 0 in a fresh namespace (so
  the pin compares objects rather than numbers), and the namespace is an `Arc`
  whose last holder - a task or a `ns/ipc` descriptor - frees those tables
  (`t-ns-ipc`, 39 claims, §6.16); `CLONE_NEWCGROUP` **landed on 2026-10-25** - the
  last kind, and the one with no objects: the cgroup tree it views is organizational
  only (controller-less, measured on a current host before it was built), the
  namespace's whole state is the view-root a `/proc/<pid>/cgroup` path renders from,
  and `t-ns-cgroup` walks the move, the fork, the ns identity and the `setns` back
  (§6.25). The tree behind those views is one machine-wide hierarchy that outlives
  every mount, so its membership entries are keyed by task *slot* and carry the
  slot's generation: a slot reused after a cgrouped task exits leaves a stale
  entry, and both readers now agree that a stale entry means **the root** - one of
  them had tested the generation with `&&` inside its arm instead, which put the
  new tenant in no group at all and hid it from the root's own `cgroup.procs`
  (fixed 2026-11-04, `t-cgroupfs`'s two new claims, §3.27, item 196)
- [x] the kernel keyring — `add_key` (248), `request_key` (249) and `keyctl` (250)
  served on a host measurement: a serial handle over a key store, a permission word
  with four classes, keyrings that hold serials, recursive `SEARCH`, the
  `REVOKED`/`INVALIDATED` states checked before permission, possession as a class
  rather than a filter, and a session ring inherited across `fork`; quota and the
  named gaps (no `request_key` upcall, possession walk without the link's `SEARCH`,
  no delayed GC of orphaned keys) are §6.30, item 113
- [x] `fanotify_init`/`fanotify_mark` (300/301; 262/263 aarch64) — the
  notification class with records on a group descriptor and one mark per inode:
  `EPERM` for a non-uid-0 caller, the content classes and every `FAN_REPORT_*`
  / `FAN_UNLIMITED_*` / `FAN_ENABLE_AUDIT` shape refused `EOPNOTSUPP` by name,
  mount/filesystem marks refused for want of inode-to-mount ancestry, and the
  24-byte record's `pid` the causing task — all measured as real root on Linux
  7.2.11 because the call needs `CAP_SYS_ADMIN` in the initial namespace (§3.9,
  item 114)
- [x] the extensible `clone` door — `clone3` (435) with a versioned argument
  struct, and `CLONE_PIDFD` delivering the child **and a descriptor naming it** in
  one call; `CLONE_DETACHED` refused as Linux's own `clone3` refuses it, the
  `stack` being the *base* of the child's region rather than `clone(2)`'s stack
  pointer (§6.10, item 94)
- [x] `process_madvise(2)` (440) — the pidfd family's last door, served on a host
  measurement: the answer is the **byte count** of the ranges advised, the refusal
  order is flags → `vlen` → the iovec array → the pidfd → the task → authority →
  vocabulary, and only four advices (`WILLNEED`/`COLD`/`PAGEOUT`/`COLLAPSE`) cross a
  process boundary (`MADV_DONTNEED` is `EINVAL` there, before `CAP_SYS_NICE` is
  asked); a hole in the caller's own range is `ENOMEM` after the walk has dropped
  what it met (§6.28, item 109); the open/getfd/send/wait doors are served
  (§6.8/§6.9, items 92–93)
- [x] `userfaultfd(2)` (323/282) — the fault becomes a message and the answer is the
  page: measured as **real root** (the call is `EPERM` for an unprivileged user), so
  `UFFDIO_API`, `REGISTER` in `MODE_MISSING`, `COPY`, `ZEROPAGE`, `WAKE`,
  `UNREGISTER` and one 32-byte record per `read(2)` are served, the faulting task
  parked until the page arrives; every `UFFD_FEATURE_*` bit, `MODE_WP`/`MODE_MINOR`,
  `WRITEPROTECT`/`CONTINUE`/`MOVE`/`POISON` are `EOPNOTSUPP`, and the registry is the
  machine's rather than the `mm`'s (a forked child's fault is delivered, not `SIGBUS`)
  (§6.33, item 116)
- [x] `rseq(2)` (334/293) — the legacy 32-byte registration, served: the id words
  name the running cpu, re-registering the same area is `EBUSY`, a different one
  `EINVAL`, a wrong signature `EPERM`, unregistering is the registered triple and
  resets the ids, a `fork` child inherits it while a thread starts with none, and
  the clock **does not preempt a task inside an armed critical section** (the tick
  is spent; a span still armed on the next tick ends the task with `SIGSEGV`, since
  this machine cannot rewind the trap frame's resume address the way Linux does, and
  that death takes the whole thread group because a fault is a **process's** death -
  it goes through the fatal-signal door like the machine's own fault handlers (§6.35,
  §6.77f, item 120)).
  The v2 area (`len > 32`) is served too, with its extra words answered honestly
  rather than copied: 64-byte aligned lengths 33+ are accepted, `node_id` is 0
  (one node), `mm_cid` is 0 (one CPU — valid under a contract that only needs
  *running* threads distinct), `slice_ctrl` is 0, and `flags` is 0 because the
  time-slice extension is not implemented where Linux's read-only bits would say
  it is available, and the `auxv` pair (`AT_RSEQ_FEATURE_SIZE` 33 /
  `AT_RSEQ_ALIGN` 64) is advertised so a libc registers the shape (§6.35, item 120)
- [x] `close_range(2)` (436) — a span of descriptors, closed or marked: measured on
  Linux 7.2.11, a span closes exactly the numbers it covers (neighbours untouched),
  `first > last` and a flag bit outside the two are `EINVAL`, `~0` means "to the end
  of the table", `CLOSE_RANGE_CLOEXEC` marks the span **without** closing it, and
  `CLOSE_RANGE_UNSHARE` hands the caller a table of its own first - so a sibling
  thread's number survives and a forked child's range leaves the parent's open. Every
  number in the span closes through `close_one`, the body `handle_close` was split
  into, so a socket's port and a DRM master die there as they do at `close(2)`
  (§3.10, item 117)
- [x] `mseal(2)` (462) — a span sealed against unmapping, resizing and
  reprotection: measured on Linux 7.2.11, `flags`/alignment are `EINVAL`, a span
  with a hole is `ENOMEM` for the whole call, sealing twice is `0`, after a seal
  `munmap`/`mprotect`/`mremap` of it and a `MAP_FIXED` over it are `EPERM` while
  a `munmap` that only abuts it still works, a destructive `madvise` is `EPERM`
  only over sealed read-only anonymous pages, and a `fork` inherits the seal. The
  bit lives on both descriptions of a mapping (VMAs *and* the eager ranges the
  program image and stack have), because sealing the text is the call's flagship
  use; a sealed stack simply does not grow (§5.5, item 118)
- [x] `futex_waitv(2)` (449) — several futex words, one sleep: up to 128 waiters,
- [x] `futex_wake(2)`/`futex_wait(2)` (454/455) — the futex2 pair: one 32-bit word, a bitset in `mask`, an absolute deadline per wait; the flag word requires `SIZE_U32`, a wide mask or wide `val` are `EINVAL`, `futex_wake`'s `int` `nr` caps the answer, and the node/policy words Linux serves are `EOPNOTSUPP` here (§6.34, item 119)
- [x] `futex_requeue(2)` (456) — move sleepers from one word to another: the same
  `waitv` descriptor pair, both flag words required to match, `WRITE` and
  requeue counts, the source word checked under the same ordering as
  `futex_wait`, and the destination never read (Linux's own shape: a moved
  sleeper whose destination is unmapped simply never wakes). The node/policy
  bits and the shared spelling are `EOPNOTSUPP` (§6.36, item 121)
  the call's own flags word must be zero, a waiter is `{ val, uaddr, flags, __reserved }`
  with `FUTEX_32`/`FUTEX_PRIVATE_FLAG` (measured; a size bit is required), the answer is
  the index of the word a wake named, and a moved word answers `EAGAIN` before a passed
  deadline; the shared spelling (no private flag) is `EOPNOTSUPP` — this kernel's wait
  table is keyed by user address, so two processes' mappings of one shared word cannot be
  matched, and a hang is not an answer (§6.29, item 112)
- [x] `open_tree(2)` (428) / `move_mount(2)` (429) / `mount_setattr(2)` (442) —
  the mount API's attachment half: a descriptor that *is* a mount handle (`/` as
  its name for a copy, the mounted path for a reference, `EBADF` on a read), a
  move that attaches either a handle or a path mount, and attribute changes
  limited to what this kernel can keep — `RDO`/`NODIRATIME` where the
  filesystem honours read-only, `EOPNOTSUPP` where it cannot (and `mount -o ro`
  on a tmpfs now says so too instead of recording a promise it cannot keep),
  `NOATIME`/`IDMAP` `EINVAL` as Linux answers them, and the superblock half
  `fsopen`/`fsconfig`/`fsmount` (430–432) served since item 135 (§3.16) —
  `fspick` (433) is the family's one remaining `ENOSYS` gap, because what it
  picks a mount for is reconfiguration and no filesystem here can be
  reconfigured (§3.11, item 122; §3.16, item 135)
- [x] `statmount(2)` (457) / `listmount(2)` (458) — the mount table reading
  itself out: Linux's 512-byte struct with its string area (offset 0 = the empty
  string), the unique mount id `listmount` walked, `STATMOUNT_BY_FD`, the mask
  reporting what was written (an unknown bit answered, not refused), truncation
  for a short fixed part vs `EOVERFLOW` for a requested string with no room,
  and `listmount`'s tree order / resume-after-id / reverse / short answers;
  the fields this kernel cannot produce stay out of `mask`/`supported_mask` and
  a foreign namespace is `EOPNOTSUPP` (§3.14, item 129)

- [x] real-time signals `32..=64` — queued with per-copy `si_value`, bounded by the target user's `RLIMIT_SIGPENDING` (a full `sigqueue` is `EAGAIN`, `kill` drops and answers 0) (§2.3)
- [x] POSIX timers 222–226 — routed, tick-scanned, musl's `SIGEV_THREAD_ID` shape works; the table is the **thread group's** now, so a worker's timer outlives its maker, any thread may arm/delete it, a process-directed expiry lands on the process, a `fork` child inherits none of it, and a zombie leader's group keeps them (§2.11, item 128)
- [x] `io_uring_setup`/`enter`/`register` (425/426/427) — the ring ABI is **served**: setup publishes Linux's measured layout (a 120-byte params struct, the three `IORING_OFF_*` mappings, `2 x sq_entries` CQ, entries rounded up to a power of two) and `enter` runs `NOP`/`READ`/`WRITE`/`READV`/`WRITEV`/`FSYNC`/`CLOSE` synchronously through the syscall handlers, so each CQE's `res` is the number the blocking call would have answered; a served request keeps working while anything outside the subset is refused **per item** with `-EOPNOTSUPP` (and an opcode no Linux has is `-EINVAL`, as measured). Named refusals: every non-zero setup flag (`SQPOLL`/`CQE32`/`SQE128`… would be promises of a poll thread and 32-byte records), `io_uring_register` for every op, and the SQ cap at 1024 entries (Linux 32768, the §8.0 memory bound) (§6.32, item 115)

**Drivers (Phase 7)**

- [ ] [~] 7.1 Display — `/dev/fb0` + DRM/KMS nodes, dumb buffers, atomic commits, PRIME both ways; weston-on-GL in software; no EDID-driven modes (§7.1, §7.4)
- [x] 7.2 Input — virtio-input evdev, capability ioctls, `/dev/input/eventN` + uevents (§7.2)
- [x] 7.3 AHCI/SATA — second disk named under its own driver, pinned by `t-ahci` (§7.3)
- [x] `/dev/uinput` — a virtual input device from userspace, with its node and `add` uevent (§7.2)
- [x] the `/dev` node set is assembled by the drivers that own it (§7.5)
- [x] configuration space over MMIO (ECAM/MMCONFIG), refused unless both mechanisms agree (§7.6a)
- [x] **the window's other 3840 bytes**: a function publishes `dev->cfg_size` — 256, or the
  full 4096 when the device answers for its extended space at 0x100 and is not aliasing its
  standard header — through `/sys/.../config` and `/proc/bus/pci`, read and written at the
  extended offsets (§7.6a, `t-pci`)
- [x] **the second machine's configuration space too**: aarch64 declares `mpos_pci_config` now
  that the device tree's ECAM window reaches the bus — the config nodes answer 256 or 4096
  bytes per device there as here, read and written through the window rather than a port pair,
  and the test machine carries one NVMe function so the PCIe half is pinned on both machines
  (§7.6c, `t-pci`)

**Wayland compositor (weston) support** — verified by `t-*`; GUI end-to-end is `cargo xtask run --mode gui`:

- [x] peer credentials + fd passing · libinput classification · device discovery · `wl_shm` buffers · framebuffer present · DRM/KMS surface (§4.1, §7.1, §7.2)
- [x] the screen has an owner — `VT_SETMODE` negotiation, revocable DRM master (§7.1, `t-seat`)
- [x] the VT mode struct is Linux's, and the private dialect that hid a seat manager's promise (plain `0x5601`/`0x5602`, `__s16` signals, `VT_AUTO`/`VT_PROCESS` the right way round; §7.1d, item 126, `t-seat`)
- [x] the switch lock and the acquire ack — `VT_LOCKSWITCH`/`VT_UNLOCKSWITCH` (machine-wide, `CAP_SYS_TTY_CONFIG`), `VT_RELDISP(VT_ACKACQ)`, and `VT_WAITUNTIL` removed from `0x0c` where Linux has `VT_UNLOCKSWITCH` (§7.1e, item 127, `t-seat`)
- [x] `VT_OPENQRY`/`VT_DISALLOCATE` — a console's allocation answers Linux's state machine (`0` no-op, `EBUSY` for the screen/a held/an unallocated console, `ENXIO` above `MAX_NR_CONSOLES`, `0` once for an allocated idle one; §7.1f, item 130, `t-seat`)
- [x] seatd survives a re-run; Mesa GBM+EGL+GLES in software; parked evdev readers woken by the service task; `EVIOCGRAB` per-owner (§7.1, §7.2, R.7)
- [x] the aarch64 GL path's EL0 traps answered — `t-el0-sysreg` (§15.36)
- [x] the session environment — `XDG_RUNTIME_DIR` ships in the shared `core/` overlay, so both machines' login shells have it (R.7's gap, closed by §15.36/§15.37)
- [x] weston's EL0 write fault above its stack — resolved by `resolve_stack_growth` on the aarch64 EL0 data-abort path; the desktop comes up whole (R.7, §15.36)
- [ ] [~] the cursor plane drives a host pointer: the card, the queue, the transfer, the read-back and the aliasing are all cleared by weston-free probes, and the image's emptiness is now **measured at its source** — an interposer logging `gbm_bo_write`'s own input reads `buf=00000000…`: weston writes 16384 zero bytes and the write faithfully copies them. `gbm_bo_write` also only fires when the sprite changes, so a session where desktop-shell never comes up shows no write at all. Controls: a magenta block bound by a weston-free probe reaches the host (`nz=3072`); write-only mappings (`PROT_WRITE` alone, which is what weston asks for) alias across processes; every handle slot works; a plain process's `gbm_bo_write` lands. Remaining: weston's own cursor image, which it writes as 16 KiB of zeros into an object Mesa does not make the plane's backing — the epoll report fix that completed desktop-shell's startup does not touch it (§7.4, R.7)
- [x] epoll reports `revents & interest`: `EPOLLRDNORM` used to be set beside every `EPOLLIN`, so `weston-desktop-shell` — which registers its timerfd for `EPOLLIN` alone — printed `unexpected timerfd events 41` (hex) for every expiry, never read its timer and never finished starting up. Fixed in `vfs::epoll::report`; pinned by a timerfd case in `t-epoll` and by three host tests whose expectations were themselves the bug (§1.1)
- [ ] [~] 3D / virgl / venus (`DRM_VIRTGPU_*`) — served and pinned, and *negotiated* on a
  `virtio-gpu-gl-pci` boot (`virgl=1 context_init=1 capsets=2 mask=0x6`). **The renderer is
  virgl now** — `eglinfo` on the GBM/device platforms and `kmscube` both report `virgl (AMD
  Radeon 780M Graphics (radeonsi, phoenix, ACO, DR...))`, EGL 1.5, Mesa 26.2.5, GLES 3.2, and
  udev/`ID_PATH` under the PCI function are in place — so the llvmpipe reading this row used
  to carry, and the devpath lead it named, were both stale. What is *not* there is
  presentation the host can see: the driver asks for the client's resource and the device
  accepts — QEMU's own command trace counts 20 `set_scanout`/`res_flush` of the client's
  frame inside one kmscube run against 40 switches back to the driver's framebuffer and
  257 console flushes, all naming scanout 0 with the mode's rect — and the seesaw is the
  render loop's frame rule, not the device's answer. Whether the host's display shows those
  20 frames is **not measured**: QMP `screendump` on this GL console reads the 2D surface
  and can only ever return the console, so the earlier pixel-identical reading measured the
  instrument. Four instruments have since been tried and none established the reading: a
  single or 2 736-back-to-back `screendump` is 2D-only by construction; `-display gtk,gl=on`
  plus an X-root `ffmpeg` capture shows a mapped window whose content is 102 non-black pixels
  of 1 024 000 (black); `-display dbus,gl=on` boots but sends nothing without a listener
  (`dbus_registered_listener` 0); and the arithmetic says a 1 Hz sample misses the ≈47 ms
  client frames ~93 % of the time, which is why the identical shots do not prove absence.
  Next: write the D-Bus listener, or record the niri output at ≥20 fps for the whole run —
  note the earlier "window capture" read the X root of a *rootless Xwayland* and could never
  have shown a composited window; `grim` + `niri msg` is the instrument that can. What *is*
  measured is that the console yields when the session declares graphics mode: with
  `KDSETMODE(KD_GRAPHICS)` first, a 14 s kmscube run shows 7 switches (all at boot) against
  the bare run's 49 alternating ones, so the seesaw was the fixture's, not the driver's (§7.4)

**Compositors beyond weston (niri, wlroots)** — analysis in §7.4:

- [x] the sysfs side of a DRM node's identity — the node's own **devpath** is a child of the  
  function its driver declared now, since libudev's parent walk follows the path and not the  
  `device` link (§7.4); the card found by its driver's declaration; the `..` in a symlink's  
  target (§7.4)
- [x] the socket a compositor binds; `GETRESOURCES`' two-call idiom; property shapes (§7.4)
- [x] DRM sync objects incl. timeline flags, measured on the real card (§7.4, `t-drm-sync`)
- [x] the lease family with `drm.h`'s request numbers; legacy KMS queries refused by name; cursor plane driven (§7.4, `t-drm-lease`, `t-drm-args`)
- [x] `/proc/<pid>/maps` — three sources, two named divergences (§7.4, R.6); and the
  *read* rule on top of its word is Linux's now: `ptrace_may_access` as
  `sched::may_inspect`, so another uid's `maps`/`smaps`, the `fd` directory and the
  `fd`/`ns`/`exe` links are `EACCES` while `status` stays world-readable (item 145,
  §3.24; what stays named is the caller's own credential transition)
- [x] niri booted end to end on the 3D card: the card negotiates its cap sets so  
  `DRM_VIRTGPU_GET_CAPS` is served rather than refused, and `niri`/`xwayland-satellite` are  
  image packages now, so the compositor's own two remaining warnings are the renderer's  
  rather than a missing file's (§7.4)
- [x] a resource is nameable by the context that names it: `EXECBUFFER` attaches what its  
  `bo_handles` name to the submitting file description's context, and each 3D transfer does the  
  same for its one resource, so the renderer's own batches survive — the boot that used to lose  
  three 852011-byte batches to `Illegal resource` under `initializing the primary renderer` now  
  prints no such line and reaches `putting output Virtual-1` with both sockets up (§7.4,  
  `t-virtgpu`'s two claims, red before it)
- [x] and the frame reaches the display the way Linux's does: a compositor's framebuffer is  
  **scanned out of the device's own copy of its resource** (`SET_SCANOUT` + `RESOURCE_FLUSH` of  
  the fb's own id, `client_scanout`/`scan_out_client`) rather than copied out of guest pages  
  that were never written — measured as 4096000 zero bytes of committed 1280x800 framebuffer  
  while niri drew into it (`.workbuddy/probe-fb-pixels`), and the predicate is the resource's  
  origin (`Resource::host_pixels`)
- [x] and the client's resources are *created* the way Linux creates them: on a card that  
  negotiated 3D every `RESOURCE_CREATE` goes through `RESOURCE_CREATE_3D` (`three_d =
  card().virgl`), because the 2D command is resource/format/width/height and cannot carry the  
  `bind`, `array_size`, `last_level`, `nr_samples` or `flags` a virgl client asks for — the  
  host's GL answered that mismatch with `GL_INVALID_OPERATION` (1282) and dropped kmscube's draws  
  (device pixels for one of its framebuffers: 0 → 4020654, and the three `vrend` lines gone)
- [x] a framebuffer is the description's object, and it has no properties: Linux's
  `GETRESOURCES` walks `file_priv->fbs`, so another client's framebuffer is not in the
  list at all, and `OBJ_GETPROPERTIES` on an fb is `EINVAL` (`width`/`height`/`format`/
  `modifier` are `struct drm_framebuffer` fields, reported by `GETFB2`) — the empty-list
  answer this card used for the device-wide list was the wrong half of both (§7.4, item 68)
- [x] the picture itself, in the window: **kmscube's cube and weston's desktop are both visible on  
  the GL card** (watched, 2026-10-14) — a client that renders into a client resource and has the  
  kernel scan it out, and a compositor whose pixels are the guest's, each through its own present  
  path (`docs/notes/` §7.4)
- [ ] [~] niri's own picture: both named leads are **measured and closed** — the buffer chain is
  consistent (`PRIME export <- handle 4 (ino=37632)`, `PRIME import -> handle 4 (… reused=true)`,
  `execbuffer #3 resources=[4, 1, 11, 8]`, `scanning out the compositor's resource 4`) and niri
  builds its primary renderer on `card0` at `/dev/dri/renderD128` as a compositor does on Linux,
  so the resource the scanout names is the one the draw names. What is left is niri's own GL work:
  its startup logs three `GL_OUT_OF_MEMORY in glBufferData` in the buffer path (`target 0`
  resources of 1 048 576 bytes), and a run with no client flushes its resource **once** and idles
  — the desktop never draws a second frame. Next: that upload, and a client that makes a window
  (`weston-terminal` wrote an empty log; the probe shell needs `NIRI_SOCKET`) (§7.4)
- [ ] [~] the property families a compositor finds by name — four built, three deliberately absent (§7.4, `t-drm-props`)
- [ ] [~] the DRM surface a wlroots pixman compositor needs — in place, never booted (§7.4)
- [x] the renderer Mesa picks: **virgl**, measured on the current tree (Mesa 26.2.5, GLES
  3.2; `eglinfo -B -p gbm` rc=0 and `kmscube` agree) — the loader identifies the card through
  the node's sysfs identity (`ID_PATH` under the PCI function), not the `virtual/…` devpath
  the old lead pointed at (§7.4)

**Security (Phase 8)**

- [ ] [~] 8.0 allocation-failure panic — reproducing case fixed, tmpfs budget landed; a demand-paged fault with no frame now answers Linux's OOM kill (`SIGKILL`, the group; a 4 MiB frame reserve pays for the death) instead of reading as `SIGSEGV` (§8.0.1, item 133); the global allocator's own `Vec` growth still panics: measured (`socket`/`pipe`/`mmap`/`fork` succeed under a 64 MiB cap by overcommit, pressure on accounted objects is a scope-wide `SIGKILL`, an accounted limit is `ENOMEM`), and the kill-from-the-allocator design rejected because its null is reached under a caller's borrow and its death path allocates; the death now prints which request it died of (`mm::exhaustion_line`, item 141, §8.0.2), and the one user-steerable store that had no bound — the VMA table — is a tree with Linux's 65 530-mapping ceiling now (item 136, §10.11), and that ceiling is `/proc/sys/vm/max_map_count`: a file the next `mmap` is answered against, its `0644` enforced on the write so an unprivileged caller is `EACCES` (item 137, §10.11a). The `chmod` that used to answer 0 on such a node is Linux's `EPERM` now (item 138, §3.17: procfs's inodes have no attributes, which is the node's answer and not a permission one, so root reads it too); the other two attribute doors followed (item 140, §3.19): a `/proc/sys` file refuses the owner with `EPERM` for root too and drops the timestamps, a pid directory refuses both, a device node and a sysfs directory drop them while an ordinary user's word is `EACCES`/`EPERM` by the reported mode; per-process ownership closed too (item 143, §3.22): a `/proc/<pid>` directory carries the process's own ids, its `status`/`fd` go back to root's for a process that made itself un-dumpable, and `status`'s `Uid:`/`Gid:` lines print the four ids; sysfs's word is a store now too (item 144, §3.23): root's `chmod`/`fchmod` writes it and the next `stat` reads it, an ordinary user's is `EPERM` and a write descriptor on a read-only attribute is `EACCES`, and the `open`-time refusal of a read-only attribute is Linux's too now (`EACCES` for root as well, because the rule is the node's write *method*; item 146, §3.25). The memory filesystem's room is its option string's now — `size=`/`nr_inodes=` as measured on Linux (`ENOSPC` at either, a name counted against the inode ceiling, the value refused at the `fsconfig` call that carries it, `size=N%` a share of memory), so the store that keeps a mount from reaching the allocator is a number the caller chose (item 139, §3.18), and that number covers everything the mount keeps — the attributes its inodes hold (item 139, §3.20) and now the names its directories hold (`ENOSPC` past it; item 140, §3.21)
- [x] 8.1 ASLR · [x] 8.3 user stack canary (§8.1, §8.3)
- [x] 8.2 seccomp — STRICT + FILTER with all eight actions measured, the `PTRACE_EVENT_SECCOMP` stop landed, and a tracer rewriting the frame now redirects the call at that stop and at an ordinary `PTRACE_SYSCALL` entry stop — the `-1` sentinel cancelling it instead, and a filter judging the number the tracer wrote (§8.2); `KILL_PROCESS` and `KILL_THREAD` are one arm each now, because the two words differ in their **survivor** and not in their signal — the process one reaps the group through the fatal-signal door, the thread one ends the calling thread (§6.77d, item 195)

**Performance (Phase 10)**

- [x] 10.1 network latency — event-driven waits + MSI-X (§10.1)
- [ ] [~] 10.2 context-switch/fork cost — lazy FPU, one TLB flush, smaller stacks; PCID recorded as a decision (§10.2)
- [ ] [~] 10.3 syscall path — itimer flag landed; fault-driven user-buffer copy not (§10.3)
- [x] 10.4 pipes + per-frame allocations — ring, handover and the seam frame pool all landed; the rate-pair measurement is recorded (§10.4)
- [x] 10.5 ext4/virtio-blk read path — cache, wide transfer and the demand run landed and measured; completions interrupt-driven and the transfer **parking its task** (the ext4 cache claims its slots around the device call; `t-blk-sleep` pins the yield on both machines); multi-request pipelining dropped; the PCI transport counted in its own sweep (`t-diskbench`'s spare-disk pass, §10.5); the counters have a program's door now — `/proc/diskstats` in Linux's twenty fields, one row per disk and per partition, a partition's transfer also the disk's (item 142, §7.10)
- [x] 10.6 serial TX ring · [x] 10.7 sub-tick sleeps — a deadline is a nanosecond one
  now, and the first machine wakes a wait *between* two clock ticks from the LAPIC's
  one-shot: fifty 1 ms sleeps are 52-59 ms instead of 200 ms, a 25 ms sleep is 25.1 ms
  instead of 28.4, and an interrupted sleep's `rem` is a nanosecond remainder. The
  second machine wakes between ticks too (2026-10-24) - its EL1 **virtual** timer is
  the one-shot, independent of the physical one the tick runs on - and `t-sleep-subtick`
  runs on both machines; the only gap left is `t-arch-prctl`, arm64's own shape (§10.7)
- [x] the gate's own clock — `cargo xtask test` measured **80.7 s → 6.5 s (12.4x)** warm: a
  stack that nothing advanced (§15.92), wait budgets that were the clock's resolution rather
  than bounds (§15.93), and a sweep whose host half runs beside its guests and whose programs
  are dealt over lanes by their measured cost (§15.94)
- [x] the gate reports itself — a live lane dashboard and an end-of-run report (stage times,
  every lane's programs/verdict/heaviest, the ten slowest programs, and the delta against the
  last green run), with the plain linesbyte-for-byte unchanged when stdout is not a terminal (§15.95)
- [x] 10.10 the 9p client's reply buffer — a mount claims one buffer instead of zeroing a
  128 KiB one per message; 20 000 reads of a 39-byte line went 249.2 µs → 239.1 µs each
  (−4.1 %, and 2 500 MiB of zeroing removed), with the round trip the wall at ≈240 µs (§10.10)

**Architectures (Phase 9)**

- [x] 9.1 aarch64 complete boot — boots real Alpine to a login prompt and every service
  ok (§13.15's `hwclock`/`modules` closed the last two), and the `tty1`..`tty4` mux — the
  last named item here — landed as the `vcmux` port (§13.16: the display half became four
  `console` seams, so the mux is both machines', and `t-vt` pins it on both arms of the gate
  rather than on the wire alone) (§9.1, §13)
- [ ] 9.2 RISC-V support (§9.2)
- [ ] [~] 13 Machine-seam purity + aarch64 parity — 13.1 and 13.3–13.31 landed; 13.2's
  caller side is complete, and what is left there is ABI-owned tables, machine identity and
  the port ledger, each with its reason in §13.2 (§13); boot-side half is Phase 15 (§15)
- [x] the boot is one flow — one `start`, one sequence, probes ask both transports (§15.8)
- [x] the kernel travels inside the loader — the ESP carries one bootable file (§15.9)
- [x] the self-test sweep is many machines' — the host half beside the guests, one lane per
  core, and a deal that follows what the last run measured (§15.94)
- [x] the boot's EL0 check can be interrupted without dying — its return path used to put
  `SP_EL1` at the *top* of the 240-byte record `enter_probe` had saved, so a trap taken between
  the gate's `eret` and the stub's first read built its frame on top of the record's own return
  address and saved `sp`: `br x9` jumped to `0`, and the dump the machine printed was the
  *second* fault of the nested pair, which is why three readings from 2026-10-25 each named an
  `ELR` inside `pl011`'s print loop that cannot fault. A clean-tree `-d int` captured the first
  fault in one boot (`Exception return … to PC 0xffffffff8021b300` then `Taking exception 5 [IRQ]
  … SPSR 0x5 … ELR 0xffffffff8021b300` then `Prefetch Abort … FAR 0x0`); the fix points `sp` at
  the record's base so frames grow away from it. Eight clean boots green, and the check now pins
  itself: it runs its program a second time with interrupts masked, the periodic compare already
  past and the program's own `I` masked, so the only `eret` that can clear `I` is the gate's
  return — the trap *cannot* miss the window. Counterfactually the next boot halts with
  `EC 0x25` / `FAR_EL1 0x221000`; with the fix both aarch64 lanes print both lines every boot
  (§13.32, item 230)

- [x] `status`'s four mapping lines are Linux's four books, and `stat`'s three image
  addresses are the image's — `VmData`/`VmStk`/`VmExe`/`VmLib` were a resident-set kB, a
  literal zero, and two more literal zeros, and `startcode`/`endcode`/`startstack` were three
  borrowed `"0"`s in the same block. All seven are one question, and its answer is in Linux's
  source rather than in either of the shapes this kernel had: `task_mem` splits the executable
  book into the *span* of the image's text (clamped to the book) and the rest, `vm_stat_account`
  books each mapping into exactly one of exec/stack/data with three ordered predicates, and
  `binfmt_elf` records the lowest executable segment's `p_vaddr` and the highest one's
  `p_vaddr + p_filesz` — **file** bytes, which a single `RWE` load with a 1 MiB `.bss` proves
  (`0x640`, not `0x100648`). `VmExe` being a span and not a sum is the measurement two one-page
  segments 1 MiB apart settle (`16 kB`, where a sum would read `8`). A zombie has no address
  space, so it reports no `Vm*` line at all and three zeros. Pinned by `t-stat-bounds` and
  `t-status-vm` (10 + 12 claims, counterfactually 6 + 7 red) (§6.117, item 231)

- [x] `statm`'s seven fields are four books, two of Linux's own zeros and one named gap —
  the file printed two counts and five zeros, and two of those zeros were the books §6.117
  had just made answerable, while the second field was a resident count where Linux puts
  `data_vm + stack_vm`. Its fourth field is the place `statm` and `status` deliberately
  *disagree*: `task_mem` clamps the text span to the executable book and `task_statm` does
  not, so one `mm` with two one-page segments 1 MiB apart reads `1281` pages here and
  `VmExe: 16 kB` there — sharing one clamped number would answer 4. Its sixth field is the
  stack's book too (`84 == VmData/4 + VmStk/4` at rest; a 1 MiB stack frame reads `310`
  with `VmData` unmoved). `shared` stays an honest `0`: no counter here splits residency
  into file-backed/shmem pages. Pinned by `t-statm` (9 claims, counterfactually 5 red)
  (§6.118, item 232)

- [x] `mlock` and its family pin a page, or say why not — every well-formed request
  answered `0` and pinned nothing, and the three readers that would have shown it
  (`VmLck`, `smaps`'s `Locked:`, the refusal a destructive `madvise` gets) were zeros
  too, so the no-op was invisible. `AGENTS.md` §3 rule 2 calls that the worst thing
  this codebase can do. Six measured answers rebuilt the family: the limit is
  `RLIMIT_MEMLOCK` (8 MiB, not `RLIM_INFINITY`), and pages already pinned **inside**
  the range come off the count before the limit test, so re-locking is free and a
  sub-range `munlock` charges off exactly its own pages; coverage is the question and
  a hole anywhere — including past every mapping — is `ENOMEM`; zero length is judged
  *after* alignment, so `mlock(unmapped + 1, 0)` is `ENOMEM` while `mlock(aligned,
  SIZE_MAX)` rounds to nothing and answers `0`; at a limit of 0 without
  `CAP_IPC_LOCK` the answer is `EPERM` **before** the address is looked at, and
  `munlock` asks for nothing at all; `mlock2`'s flag word is `MLOCK_ONFAULT` = **1**
  where this kernel had `0x10` (so Linux's accepted spelling was refused and its
  refused one accepted), and `mlockall` refuses an empty word and `MCL_ONFAULT`
  alone; `MAP_LOCKED` is the same pin through `mmap` (`EAGAIN` past the limit), and a
  pin makes `MADV_DONTNEED`/`MADV_FREE` `EINVAL` with the contents intact. `fork`
  inherits none of it — `dup_mmap` clears the bit and the child can `munlock` what it
  never pinned. Pinned by `t-mlock` (25 claims, counterfactually 13 red) (§6.119,
  item 233)
- [x] `/proc/<pid>/stat`'s data segment, break and string bounds are the load's
  record — fields 45-51 were seven literal zeros while §6.117 had already given
  the image addresses the record they come from. All seven are written once at
  `exec` and never again: a `malloc(1 MiB)` moves none of them and `sbrk(0)` is
  not `start_brk`. The four string fields forced a change this kernel needed
  anyway — `arg_end` and `env_start` are *the same address* in Linux because
  `create_elf_tables` walks one cursor upward through `argv` then the
  environment, and this kernel's entry block wrote them downward, putting
  `argv[0]` highest and the environment below it. The strings now ascend from
  one base (argv, environment, `AT_EXECFN`), so the pin can assert the byte
  range `arg_start..arg_end` *is* this program's `argv` in order. Pinned by
  `t-stat-image` (11 claims, counterfactually 6 red) (§6.120, item 234)
- [x] the `sched_*` policy word is a policy number with an optional flag, and the
  band is judged before support — `sched_setscheduler`'s second argument is a
  policy number with `SCHED_RESET_ON_FORK` OR-ed into it, and Linux masks the
  flag off before it asks which policy the rest names. Around that word it asks
  three separate questions in a measured order: is it a policy at all
  (`0,1,2,3,5,6,7` after the mask, so `4` and `8+` are `EINVAL`; `6` is a policy
  whose band is 0 but whose door is `sched_setattr`), is the priority inside
  that policy's band (`1..=99` only for `FIFO`/`RR`), and can *this* kernel serve
  it. This kernel had the first and the third crossed: `SCHED_BATCH`,
  `SCHED_IDLE`, `SCHED_EXT` and `RESET|*` — four policies that exist — answered
  `EINVAL`, "that is not a policy", where the honest answer is `EOPNOTSUPP`; and
  an out-of-band `SCHED_FIFO`/`SCHED_RR` priority answered `EOPNOTSUPP` where
  Linux refuses the argument itself. `sched_get_priority_max(7)` answered
  `EINVAL` for a policy number Linux bands at 0, and `sched_setattr` never read
  the struct's `priority` at all, so `sched_setattr(SCHED_OTHER, 1)` answered 0
  and dropped it. The judgements are `abi.rs` verdicts now — `Policy`,
  `scheduler_policy` (the one door that masks), `policy_band`, and three
  `*_verdict` functions — and the handlers only read the caller's memory.
  Pinned by `t-sched-policy` (21 claims, counterfactually 9 red); its two
  `/proc/<pid>/stat` rows are *agreements*, not counterfactuals: `policy` field
  agrees with `sched_getscheduler`, `rt_priority` and `processor` are honest
  zeros of a scheduler with one band, one policy and one CPU (§6.122, item 236)
- [x] a POSIX timer is built on a clock, and one of those clocks is CPU time —
  `timer_create`'s first argument names the clock a timer's deadline is measured
  on, and the two CPU-time clocks are not time in the tick sense:
  `CLOCK_PROCESS_CPUTIME_ID` counts the CPU the machine charged to the timer's
  own thread group. Measured on the host, a 30 ms CPU-time timer fired **zero**
  times across 200 ms of `usleep` and once after a CPU burn, and its
  `timer_gettime` remaining did not move across the sleep at all. This kernel
  refused the clock with a reason that had expired - "it keeps no per-task CPU
  accounting", untrue since §6.95 and §6.99 - while keeping one reading,
  `hal::ticks()`, subtracted in four places (the expiry scan, the periodic
  reload, `timer_gettime`'s remaining, and `timer_settime`'s `old_value`), each
  correct for a wall timer and wrong for a CPU one. The clock now lives on the
  timer (`TimerClock { Wall, Process }`, a `PosixTimer::clock` field) and its
  reading is the group's own `utime + stime` — the same pair
  `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` answers from — and `timer_arm` takes
  a value rather than a deadline, so the arm's arithmetic happens where both the
  timer and the reading are in hand. `CLOCK_THREAD_CPUTIME_ID` stays
  `EOPNOTSUPP`: its clock dies with the creating thread while the timer lives on
  the group, and a frozen reading has nowhere to live in this table. Pinned by
  `t-timer-cpuclock` (16 claims, counterfactually 7 red); `t-timer-create`'s old
  divergence claim became an agreement on both arms (§6.123, item 237)
- [x] the `timer_*` doors judge in Linux's order, and `timer_settime`'s flags
  carry one bit — a refusal's *order* is an errno promise: measured on the
  host, `timer_create` judges an unreadable `sigevent` (`EFAULT`) before the
  clock (so a bogus or refused clock cannot answer `EINVAL`/`EOPNOTSUPP` over
  it), and a broken sigevent's meaning (`EINVAL`) before a bad `timer_t*`
  (`EFAULT`); `timer_settime`'s `flags` is a 32-bit word whose only meaningful
  bit is `TIMER_ABSTIME` (every other bit is ignored and the timer arms — this
  kernel used to refuse them all with `EINVAL`), and its order is new-value
  pointer → flags → id and both timespecs → arm → `old_value`, so a bad
  `old_value` answers `EFAULT` **after** the timer is already armed (the host
  reads `remaining=3599.999944756` right after such a call) rather than leaving
  the timer unarmed; `timer_gettime` judges the id (`EINVAL`) before its output
  buffer (`EFAULT`). The flags word now lives in one host-testable pure
  verdict, the four check orders are the handler's statement sequence, and
  `TIMER_ABSTIME` stays an honest `EINVAL` named gap (one monotonic clock, no
  `CLOCK_REALTIME` epoch). Pinned by `t-timer-order` (23 claims, counterfactually
  11 red, including the arm-with-`EFAULT` side effect that a right errno alone
  would miss) (§6.124, item 238)
- [x] `setitimer`/`getitimer` judge in Linux's order, and `which` is an `int` —
  measured on the host, `which = 2^32` truncates back to `ITIMER_REAL` and arms
  it (`2^32 + 3` is `EINVAL`, a sign-extended `-1` is `EINVAL`), while this
  kernel compared the whole `usize` and refused them all; an unreadable
  `new_value` answers `EFAULT` **before** a bogus `which` is judged, and a
  malformed `tv_usec` answers `EINVAL` **before** a bad `old_value` answers
  `EFAULT`; and a bad `old_value` still leaves the timer **armed** (the host
  reads `armed=yes it_value=9.999998` after that `EFAULT`), which a kernel that
  only returned the right errno would miss. The 32-bit word now lives in one
  host-testable verdict, the order in the handlers' statements; the three stale
  "no per-task CPU accounting" comments now name the real gap (CPU time is
  charged, but the expiry scan covers `ITIMER_REAL` only - and that half is
  reachable on **both** machines now that §2.13's trap-time delivery is on both,
  which is the door that hands an expiry over to a task that never calls). Pinned by `t-itimer-order` (27 claims,
  counterfactually 8 red) (§6.125, item 239)
- [x] `/proc/uptime`'s two columns are two `%lu.%02lu` words and the idle column is a live counter,
  not a literal `0.0`: an unpadded `S.4` was read by `atof` as `S+0.4` (360 ms high) on ~9% of reads,
  which is the real cause of `t-starttime`'s long-running "flake" — §6.108's "180 ms fork latency"
  and the §6.125 ledger's "single-core TCG contention, not a regression" were both misdiagnoses and
  are corrected in place. Pinned by `t-uptime-shape` (8 claims, counterfactually 4 red, a 2600-read
  shape loop deterministically crossing the bad centisecond window); host renderer test `760 → 761`
  (§6.126, item 240)
- [x] `sched_rr_get_interval` answers a slice for a target on the runqueue and **zero** for one that
  is not — blocked in a syscall, held at a `SIGSTOP`, or a zombie not yet reaped — which is Linux's
  rule (`build/scratch/rrslice/d.c`, `e.c`; nice, policy and queue length do not move it, `f.c`,
  `c.c`, `g.c`). This kernel answered the constant `1_000_000_000 / TICK_HZ` for every number it
  accepted, and §6.122 had explained the divergence as "the value is tuned, not fixed" — which
  described the figure and missed that the answer's *shape* differs. The runnability question is the
  same one `/proc/<pid>/stat`'s state field answers, so its ladder moved out of `snapshot_of` into
  one `state_of`/`task_state` pair rather than being derived twice. Pinned by `t-rr-interval` (12
  claims, counterfactually 3 red — exactly the three state claims; the runnable-child control is
  what stops a kernel that answered zero for everything from passing); host unit tests `761 → 762`
  (§6.127, item 241)

**Architecture debt (Phase 11)** — audit done, Steps A–G landed, V-1/V-4/V-13 closed (§11):

- [x] 11.A subtract · 11.B device seam · 11.C per-open-file registry · 11.D terminal out of `vfs` · 11.F host-test criterion · 11.G depgraph ratchet (§11.2)
- [x] the `mm`/`vfs` seam (V-13) — `MappedObject` trait (§11)
- [x] twelve leaf crates + `mpos-blk` as the thirteenth — enforcement by compile error (§11.2 Step E, R.11.E)
- [x] 11.4 the HAL, stages S1–S5 — machine seam enforced by the compiler (§11.4)
- [x] the two largest files stop being single files — `sched/runtime/` and `drivers/drm/`, one child  
  per responsibility, with the two `arg_shape` rows a mechanical move dropped caught by `t-drm-args`  
  and restored (R.11.F)

**ABI truthfulness (Phase 12)** — Group A landed, gate runs to a summary (§12):

- [x] 12.0 green baseline restored (§12.4a)
- [ ] [~] 12.A honest-refusal batch — landed and gated through the buffer-direction audit (§12.1, R.12)
- [x] 12.B contract-completion batch (§12.2) · [x] 12.C stale rationale comments corrected (§12.3)

---


### Suggested next moves (highest impact, tractable)

In the order that keeps each step verifiable on its own:

1. **namespaces** (§6.3) — **complete** (2026-10-25): all eight kinds of `namespaces(7)` are served, each with its pin, CGROUP last with the tree it views (§6.25). What remains in the family is parity work, not kinds — the mount's re-rooting inside a cgroup namespace, controllers behind the tree if a caller ever needs one. The original text follows for the record of how each rung landed: UTS, MNT, PID, NET and TIME are landed, and the ladder's remaining work  
   is named per rung: a **wire that can move** into a namespace has landed (2026-10-16) -  
   `ip link set eth0 netns N` moves the device, and it arrives **down and unconfigured** because  
   an address is the namespace's and not the device's (measured on Linux first; `t-ns-net` walks a  
   whole round trip, §6.3); and the **clocks can be moved** too (2026-10-16) - `unshare` arms the  
   caller's children slot, the offsets are written through `/proc/self/timens_offsets` and frozen  
   by the first task to enter, and `t-ns-time` walks the round trip both ways with the caller's own  
   readings pinned unmoved (§6.3). So what is left here is the **two** kinds with no entry at all  
   (IPC and cgroup - two of `namespaces(7)`'s eight), and of the three this item once named, the one  
   it picked is the one whose absence is felt by everything else (a container  
   runtime's first call is `unshare(CLONE_NEWUSER)`, and uid mapping is what the other two are  
   *scoped* by), which also made it the most expensive - it touched the credential model rather  
   than adding a namespace object. **USER landed on 2026-10-16**, its whole shape measured first  
   (§6.3, five probes run as root *and* unprivileged: `unshare` moves the caller, an unprivileged  
   caller may make one, the caller lands on the overflow id because the map starts empty, it holds  
   the **full** capability set inside the moment it is made - "root inside" - `uid_map` takes a  
   single self-naming line once and nothing else, `gid_map` waits for `setgroups=deny`, there is no  
   `user_for_children`, `clone` is a door on Linux and `setns` into one's own is `EINVAL`). What  
   landed is the map object and the translation at the four `getuid`-family readers, pinned in 23  
   claims, and then the same translation at the four `stat`-family doors (2026-10-16) - a file  
   created before an unshare reads the stored id from the machine map and the inside id from the  
   new one, `statx` agreeing with `stat` (§6.3, measured on the host first). A third surface, the  
   socket's, landed on 2026-10-16: `SO_PEERCRED` and the `SCM_CREDENTIALS` a read carries map the  
   peer's stored ids through the reading task's namespace, at the read rather than at capture  
   (§6.3, measured on the host first). And the rung's other  
   half, the **capability grant**, landed on 2026-10-16 as well: entering a namespace is itself the  
   grant, so `unshare`/`setns` hand the maker the full effective, permitted and bounding sets  
   (`t-ns-user`, a caller that drops first reads them back), measured across all four  
   doors on the host (§6.3). The **child-side creation** of `clone(CLONE_NEWUSER)` landed the same  
   day: a child born already inside a namespace of its own, the overflow id until its map is  
   written, the full set at birth, and the parent's `CLONE_NEWUSER` lifting the capability gate for  
   the other kinds beside it (`t-ns-user`, 46 claims, §6.3). And the socket's `ucred.pid` landed the  
   same day - the pid-namespace half of the same struct, rendered with `pid_vnr`'s rule so a reader  
   inside a namespace names a peer there and reads `0` for one it cannot see (`t-ns-pid`, §6.3).  
   The **netlink/uevent family's own `ucred`** followed (2026-10-16): its ids render through the  
   reading task's user namespace - the kernel's synthetic zero record included - and its pid is the  
   *sender's* number in the sender's own pid namespace, resolved once at send, which is also the  
   port id a zero-`nl_pid` bind prefers (three probes; `t-uevent`, §6.3). So nothing of the `ucred`  
   stays named now.  
   **IPC's wait is over in full** (2026-10-23): the four families landed
   (§6.12-§6.15) and then the namespace itself (§6.16) - `unshare`/`setns` move
   the caller, the four tables are one slot space per namespace, a number
   restarts at 0 inside one, and `t-ns-ipc` holds the pair for all four
   families, and the namespace is an `Arc` whose last holder (a task or a
   descriptor) frees its tables. All three doors serve it. **CGROUP's wait is
   over too** (2026-10-25): the tree it would isolate is organizational only -
   controller-less, measured on a current host before it was built - and the
   namespace is the view-root a `/proc/<pid>/cgroup` path renders from, with
   `t-ns-cgroup` walking the move, the fork, the identity and the `setns` back
   (§6.25). **The ladder has no rungs left: every kind of `namespaces(7)` is
   served.** Nested namespaces pin the  
   *numbers* two levels deep now, as the zap's reach already did. The `/proc/net` reader's rows were  
   per-namespace but unexercised; `t-ns-net` reads the file on both sides of the boundary now  
   (2026-10-15). Two more left this list on  
   2026-10-15: the pid namespace's own creation rules (one per process, and a sealed one takes no  
   tasks - both refused now with the errno Linux uses), and `NSpid`'s single number, which is the  
   chain in all four `NS*` fields now. `F_GETLK`'s `l_pid` is the reader's number too, from  
   2026-10-15 - and its `F_OFD_*` neighbours are served, in the same record domain, from the same  
   day (§3.2). `AF_UNIX` moved off this list  
   (2026-10-15) and vsock left it with a measurement instead of an implementation, both recorded in  
   §6.3. Beside it: seccomp's `PTRACE_EVENT_SECCOMP` stop landed (2026-10-15), and a tracer  
   rewriting the frame there redirects the call now (2026-10-16) - as it does at an ordinary  
   `PTRACE_SYSCALL` entry stop, from the same day, pinned counterfactually in `t-ptrace` (§8.2:  
   the exit stop answers the parent's pid with the re-read and the tracee's own without it).  
   What was unnamed there is closed now (2026-10-16): the `-1` sentinel that *cancels* a call  
   rather than replacing it is measured and pinned at both stops - the call does not run and the  
   tracee reads `-ENOSYS` - while a write at the *exit* stop is a no-op, and the ordering the  
   redirect implies is Linux's too (a `SECCOMP_RET_ERRNO` filter judges the number the tracer  
   wrote, not the one the program asked for). Nothing of the number slot stays unnamed (§8.2).
2. **interrupt-driven block completions** (§10.5's last row) — **landed, in two steps, both  
   2026-10-25**: both virtio-blk drivers submit non-blocking and wait on a completion interrupt  
   (x86_64 MSI-X on vector 0x42; aarch64 each MMIO window's own tree-named SPI through  
   `arm_block_line`), the masked window a read holds collapsed from the device's latency to the  
   submit/retire edge, and `drivers::kunit`'s pin flipped to "a wide read no longer holds the  
   machine". The same day, the wait became the scheduler's **uninterruptible park** — the ext4  
   cache grew the re-entrancy the park was refused for (a slot *claim* per miss, per-device  
   transfer serialization in the mailbox, no borrow spanning a device call — §10.5 carries the  
   whole measurement) — and the machine-side pin is `t-blk-sleep`: a sibling's counter moves  
   inside one-sector device calls (64 of 64 parked, 1 of 64 on the non-parking shape, measured  
   both ways). What stays named of the row: nothing; multi-request pipelining stays dropped  
   until something above asks for two in flight.
3. **the weston pointer on the window** (R.7, §7.4) — the root cause is fixed in code: the cursor  
   queue is configured when the device has one, `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is accepted  
   with `HOTSPOT_X`/`HOTSPOT_Y` published per-client, and binding or moving the plane hands the  
   pointer to the host over virtio-gpu `UPDATE_CURSOR`/`MOVE_CURSOR`; the software blend is now only  
   the no-cursorq fallback. Host tests, both bare-metal builds and `depgraph` are green and the pins  
   (`t-drm`, `t-drm-props`) cover the cap gating and the `RMFB` dangling-resource fix. Both turned  
   red on the sweep and are green again after three faults it found (2026-10-13): `OBJ_SETPROPERTY`'s  
   type check named `PLANE_ID` but not `CURSOR_PLANE_ID`, so every write to a cursor property was  
   `EINVAL` while the read of the same property reported the old value; the cursor plane's property  
   list carried `HOTSPOT_X` *and* `HOTSPOT_Y` but its count stopped one short of the second, so a  
   client that had set the cap was shown one of the two; and `t-drm`'s `NR_RMFB` was `0xA4`, which  
   is `MODE_GETGAMMA` and not `MODE_RMFB` (`0xAF`, read out of `drm.h`) - the card was answering a  
   gamma request's `EINVAL`, and the test read it as a refusal to remove the buffer. The measurement  
   a `t-*` program cannot make is answered: `run` binds a QMP socket  
   (`build/<arch>-9p-share/qmp.sock`), `input-send-event` moves the pointer, the pointer bitmap is  
   read back over VNC's `CursorWithAlpha` pseudo-encoding (`screendump` answers `no surface` on the  
   GL backends), and a whole weston session on the GL card shows the desktop up — the epoll report  
   fix (§1.1) removed the startup stall — with its cursor plane bound through atomic. **What the  
   plane receives is empty at its source**: weston's `gbm_bo_write` copies 16 KiB of zeros into an  
   object Mesa does not make the plane's backing, while a weston-free control's image reaches the  
   host. The remaining gap is inside Mesa's virtio-gpu GBM allocation for the cursor BO, upstream of  
   this kernel (§7.4); the no-cursorq fallback branch stays named, since neither configured machine  
   can exercise it.
4. **the display path specifically** — §7.4 carries its own order, and the step it named first is a  
   **dead end, measured 2026-10-18**: a wlroots compositor on `WLR_RENDERER=pixman` stops in  
   `wlr_allocator_autocreate` (`[wlr] [render/allocator/allocator.c:168] Failed to create  
   allocator` / `[sway/server.c:306]`) on the image's `wlroots 0.20.2` — that renderer is the one  
   with no allocator to be handed, while the same binary with its default renderer gets past the  
   line. What the same session found in its place: `sway 1.12` fails `Unable to bind IPC socket` on  
   the DRM backend and headless alike (the errno is the next measurement), and takes a user-mode  
   NULL dereference it survives (§7.4 carries both instruction sites). **Re-measured 2026-11-01: the
   headless backend now runs a whole sway session** - EGL on the virtio_gpu card, `HEADLESS-1`
   created and arranged, transactions committing, its IPC server accepting clients - so that line
   was that session's shape, not the socket's (the IPC doors answer 0 for an abstract name, a
   filesystem path and the blocking variant of each, as Linux 7.2.11 does); the **DRM** backend
   stalls before printing its first line, and `strace -f sway` on that run is the next measurement
   (§7.4, item 175).
5. **the renderer's device identity** — **answered** (2026-10-18). The devpath this item's own lead  
   named has landed (2026-10-13: `/sys/devices/pci0000:00/0000:00:04.0/drm/card0`, which `udevadm  
   info -a -n /dev/dri/card0` walks up to `SUBSYSTEMS=="pci" / DRIVERS=="virtio-pci"`), and Mesa is  
   on the card rather than on llvmpipe: `virgl (AMD Radeon 780M Graphics (radeonsi, phoenix, ACO,  
   DR...))`, Mesa 26.2.4, with weston's `Using GL renderer` behind it (§7.4). What keeps the GL card  
   from being the gate's default is the *host's* Mesa teardown crash, not a refusal this kernel  
   could answer (`machine.toml`'s key and §7.4 carry it), so the split stays.

---

## Current Status

  An exec from a worker thread ends the group, as Linux's `de_thread` does, and the old image's
  frames wait for the last member: before, the exec emptied the frame list while a sibling was still
  fetching instructions from those pages (`t-s[17]: segfault ... error 14`, an instruction fetch
  inside the old `.text`) - which is also the shape udevd, `openrc-run.sh` and `ebegin` died of
  (item 190, §6.74). `/proc/<pid>/status`'s `Threads:` is counted from the live members now, too:
  it answered 2 for the one-thread process an exec leaves. Pinned by `t-exec-threads`, whose host
  reference runs on Linux for seven claims, and whose counterfactual reddens the thread count while
  saying so honestly about the sibling claim that happened to stay green.
  A private mapping of a **file** crosses a `fork` private: the fork loop treated every
  `VmaKind::Frames` range as a shared object and never read the `shared` field, so between
  `4930158` and this item a child inherited its parent's file pages writable and unmarked and both
  wrote the same frames - measured as `child's writes visible to the parent: YES (BROKEN)` on the
  machine where Linux answers `no (correct)` (item 191, §6.75). That is the regression the user's
  bisect named, and the shape udevd's `mallocng` `#PF at 0x10`/`0x30` took: a forked child
  overwriting its parent's heap metadata. Pinned by three claims at the end of
  `t-mmap-file-lazy`. A boot of the fixed kernel printed **no** user-fault line at all through udev's
  device processing, where the baseline was five `udevd[..]` deaths per boot (§6.75a).
  A handled signal ends a parked wait with `EINTR`: measured on 7.2.11 and on this machine for a
  parked `epoll_wait(-1)` and a parked pipe `read(2)`, with the handler already run when the call
  returns; a `SIG_IGN` disposition is not an interruption and does not end it. Pinned by
  `t-signal-park`, whose seven claims each run their parked call in a bounded child so a lost
  interrupt fails under a name instead of spending the program's budget (item 192, §6.76). It also
  clears the cheapest explanation for weston's post-`SIGTERM` park, which did not reproduce when the
  signal landed during startup.
  A userland fault's report names the program that faulted, not a task slot: `<comm>[<tid>]:
  segfault at <addr> ip <ip> sp <sp> error <word> in <module>[<off>,<base>+<len>]`, and
  `traps: <comm>[<tid>] general protection fault ...` for a trap (item 189, §6.73). The shape is
  Linux's, measured on 7.2.11 across six fault kinds; the `in ...` clause appears only when the ip
  is in a named mapping, as Linux's does. Two things a compositor boot taught on the way: the
  reports named only a task slot, and their ring lines were filed without a newline, so `syslog(2)`
  answered two entries as one.
  A mapping that outlives its name says so, as Linux's does: `Inode::name_is_gone()` now exists
  beside the ambiguous `nlink()`, and a descriptor of an unlinked file reads as `<path> (deleted)`
  (item 184, pinned host-side). The guest-side pin taught what was really missing there, and it was
  not an orphan list: `Ext4Ino` was `(fs, ino)` with no incarnation, so a number recycled while a
  descriptor was open made that descriptor answer about the new file. Fixed in item 186 - the handle
  carries a generation, `alloc_inode` bumps it, and a stale handle answers `ESTALE` while its bytes
  stay readable until the number is reused.
  A regular file's descriptor now keeps the path it was opened by, so its mapping's line in
  `/proc/<pid>/maps` is named as Linux's is and `/proc/self/fd/<n>` answers the path (item 183),
  with Linux's ` (deleted)` suffix once the name is gone (items 184, 186).
  An atomic commit is judged in Linux's order - flags, then `reserved`, then the pointers - so an
  empty commit (what a compositor's teardown sends) is served and a null array behind a nonzero count
  is `EFAULT`, both measured on 7.2.11 + `vkms` (item 187, §7.4f). The immutable-property refusal that
  a niri log opened the question with is Linux's own answer.
  A generated procfs file is a snapshot for the length of one read: `/proc/<pid>/maps` re-rendered
  on every `read_at`, so a chunked reader of a file the process was growing printed a span twice
  (`insert_present`'s clipping fixed a second defect on the same path; item 182).
  `t-maps` now also pins the maps offset column as the byte offset Linux prints, with the
  memfd's own inode beside it (item 181).
  The fault path item 177 introduced left its admission rule behind: a syscall reading a page of a
  private file mapping the process had not touched yet answered `EFAULT` where Linux faults it in,
  which is what made both compositors die far from the call that was refused. `MappedObject` now
  answers whether the kernel may populate a page on the caller's behalf - `true` only for a private
  file mapping, whose bytes come from the filesystem rather than from the card whose lock a DRM
  ioctl already holds - and populates it *before* the caller's copy, because that copy runs inside
  the filesystem's own borrow (item 188, §10.15a).
  That refusal was the display wall, not only a mmap bug: on kernel `3f6bd02` weston 16's DRM backend
  reports `EGL version: 1.5`, `GL renderer: virgl (AMD Radeon 780M ...)`, `Using GL renderer` and
  `Output 'Virtual-1' enabled`, then paints - the serial shows it scanning out two alternating
  resources. Before the fix the same boot ended at `EGL error state: EGL_NOT_INITIALIZED (0x3001)`.
  Card-node EGL was never a DRM door refusing it and never Mesa's loader choice; it was a syscall
  reading an untouched page of a private file mapping (§7.4g).
Two machines boot the same flow to a real Alpine userland; the ABI answers Linux's shape across  
syscalls, signals, filesystems, networking, memory, devices and security, with the honest refusals  
named. What is true today — counts, measurements, gates — is in [`notes/`](notes/index.md); this  
section states direction only: the frontier is the second compositor family (the wlroots one, which
now comes up on this kernel — `sway` runs as a compositor, binds its IPC socket and applies three
transactions, §7.4a; what it cannot do yet is scan out a client buffer - the wall is clientside, `wlr_buffer_get_dmabuf`
failing before any kernel door is reached (§7.4a) - and its log's one kernel-side gap, the
framebuffer-close request, is now served and pinned (`MODE_CLOSEFB`, item 179, §7.4b; the same
measurement moved two `ENOENT` answers `GETFB`/`GETFB2`/`RMFB` had wrong) and closing the remaining
ABI gaps below. `/proc/<pid>/smaps_rollup` is a pinned contract now
(`t-smaps-rollup`, 10 claims, item 180, §15.99); writing it down first suggested the two accounting
files disagree — re-measured, they do not: the `Pss_Anon`/`Pss_File`/`Pss_Shmem` split belongs to
`smaps_rollup` alone on Linux 7.2.11 too (`grep -c` is 0 on `smaps`, 1 on the rollup), and the pin
keeps that asymmetry as a claim. The block layer's counters
now read on a desktop boot — the submit/retire pair and the mailbox agree at all 115 heartbeats of
three boots — and that boot's own identified two-minute wait is `udev-settle` in `poll(7)`
(§7.9b, item 148). The kernel's half of it is now measured out: one uevent announcement reaches
every listener, parked or spinning, in one process or two (§7.9e, item 152, pinned by
`t-uevent-multi`), and every receive door on a device-event socket answers the event - `recv(2)`
and `read(2)` had answered `EBADF` while only `recvmsg(2)` worked (§7.9r, item 165, pinned by
`t-uevent-doors`), because a listener's choice of door must not decide whether it hears a device.
The compositor's twenty-second silence is measured out as well: a private mapping of a file copied
the whole file *inside* `mmap(2)` (3.4 s for a 189 MB shared object, linear in the length), and the
call now records an address range whose pages arrive on the fault that needs them - 0.0 ms at every
size measured, with the I/O paid per page actually used (§10.15, item 177, pinned by
`t-mmap-file-lazy`'s residency claims).
Its options are Linux's table now, too: a device-event socket used to answer **zero** to every
name it had not been told about - `SO_TYPE` included - and now answers `SOCK_RAW`/`AF_NETLINK`,
the datagram buffer defaults with Linux's doubling, and `ENOPROTOOPT`/`EOPNOTSUPP`/`EPERM` where
the family refuses (§7.9s, item 166, pinned by `t-uevent-opts`), and its
`SO_RCVTIMEO` now bounds the receive it used to only store (§7.9t, item 167,
pinned by `t-uevent-doors`'s timeout section). The allocation audit's last
user-steerable count is closed too: `epoll_wait`'s collect buffer was sized by
the caller's `maxevents`, so a large enough mapping asked the kernel for
gigabytes to report at most 1024 events and the allocator's `panic = "abort"`
ended the machine — the buffer is now bounded by what `collect` can fill, the
number by Linux's `INT_MAX / sizeof(struct epoll_event)`, and the caller's array
checked as each record is written rather than all of it up front (§10.12, item
168, pinned by `t-epoll`; the counterfactual is a 2 863 311 520-byte allocation
and 30 programs lost). Its sibling door is closed the same way: `poll(2)`/`ppoll(2)`
bounded `nfds` by a memory window instead of by the caller's own `RLIMIT_NOFILE`,
so a process allowed a million descriptors could not wait for them, and sized its
watch list from the same `int` — the count is now measured against the limit and
refused with `EINVAL` before any entry is read, each entry is validated as it is
reached, and the list grows with the entries that pass (§10.13, item 169, pinned
by `t-poll-wait`; the counterfactual reddens exactly the refusal and the wide
wait, while the at-limit claim stays green because a 1024-entry array crossed no
window — a green counterfactual is a measurement of what a claim cannot see).
The third door in that family was judged by a constant that was never the
kernel's: `select(2)` refused any count past libc's `FD_SETSIZE`, where Linux
clamps to the descriptor table's own capacity and never reads a bit above it -
and its `pselect6` sibling wrote the remainder back in microseconds through a
`timespec`, off by a thousand. Both shapes now answer Linux's measured table:
the count is a right and the clamp is the table's, the timeout is judged before
the count, an over-unit `tv_usec` folds for `select` and is refused for
`pselect6`, and a signal writes the remainder back in the caller's own unit
(§10.14, item 170, pinned by `t-select`; four distinguishing claims red and
eighteen green under the counterfactual).
One more wait was bounded the same way: a socket's `SO_RCVTIMEO`/`SO_SNDTIMEO` were stored
and never consulted, so a `recv(2)` with a 200 ms bound parked forever. Linux's measured
contract — a 16-byte `timeval`, `EDOM` outside its unit, a `tv_usec` rounded up with carry,
`{0,0}` meaning no bound, the bound honoured by every receive door *and* `accept` *and* the
send side, and inherited by an accepted socket — is now served on the IP family, a unix
socket and a `socketpair`; the seven distinguishing claims go red under the counterfactual
while every storing claim stays green (§4.7, item 171, pinned by `t-sock-timeout`).
The socket's *option names* answered the same way: an unmodelled name read back **zero**, a
claim that the socket had been told a value nobody stored — `SO_DOMAIN` as 0 is `AF_UNSPEC`.
Linux's rule is measured and served now: the socket answers what it *is* (`SO_TYPE`,
`SO_DOMAIN`, `SO_PROTOCOL`, `SO_ACCEPTCONN`, `SO_ERROR`), an unmodelled name is
`ENOPROTOOPT` under a level the socket's stack has and `EOPNOTSUPP` under one it does not,
and the `set` door refuses a read-only identity name and a foreign level before the store
(§4.8, item 172, pinned by `t-sockopt-names`; seven distinguishing claims red under the
counterfactual — the one leniency left, `set` of an unknown name under a level the socket
*has*, is a named divergence because a partial allow-list would be stricter than Linux for
the real names it omits).
The bus's wake list no longer belongs to a network stack, either: a machine with no NIC used to
park a device-event listener on no list at all, so `udevd` slept through every announcement —
its database stayed empty across a full trigger, `udevadm settle` burnt its timeout and the
boot's "Waiting for uevents" ran 555 serial lines; the list is machine-wide now and that boot
reaches "Starting networking" 2 lines later (§7.9u, item 173; the distinguishing evidence is
that NIC-less boot, because every lane attaches a NIC and the lane pins stay green either way).
What remains of the bus is how the image's `udevd` binds and reads. Two items left this list
on 2026-10-25: the block layer's completion path (completions interrupt-driven, the transfer
parking its task, §10.5), the namespace ladder (all eight kinds of `namespaces(7)` served,
§6.25) and `clone3`'s last unserved word (`CLONE_INTO_CGROUP`, §6.26).

The shutdown path is its own open item (item 193, `docs/notes/` §7.4h): with the kill landing after the main loop is up, weston logs `caught signal 15` and stays alive — `/proc/<pid>/stat` reads `S threads: 2` before and `R threads: 1` after, the survivor parked at a wait, so the group is never reaped. Half of the mechanism is now named and closed: the group-death rule had **two** doors and only one of them ended the group, so a task that killed itself died alone and left its parked sibling alive (`syscall 62` at the `kill` gate, `retired=false`) — the one door is `die_of_fatal_signal`, pinned by `t-kill-ends-group`'s self-kill fixture (§6.77b, item 123). The second half is named and closed too: the copy that killed the wrong thread was **process-directed but delivered to one task**, and Linux hands such a signal to a thread that may take it - measured as a parked worker's `FUTEX_WAIT` answering `EINTR` where this kernel's left the handler unrun and both threads parked (§2.12, item 197, pinned by `t-signal-park`'s third shape). What still stands open is the composition's own ending: weston's survivor may be a thread at a futex nobody wakes for a different reason (§7.4h's fourth and fifth readings, both measured with `retired=false`), and the confirmation still owed is weston's own boot reaching a reaped group.

Two more observations were named on 2026-11-04. **One is closed: a detach could hand back a tracee the user had
stopped.** `t-ptrace-attach`'s "a detach leaves a stop the user asked for alone" reddened twice on the second
machine's six-lane sweep, once with `(state "", collected true)`, and would not reproduce alone. The suspect named
here — `PTRACE_ATTACH`'s stop and the user's `SIGSTOP` sharing one stop object — was the wrong half; the defect was
in `trace::park`, which slept once while its task was still in the ready queue the picker pops from, so the park
came back on the first wake of any kind and the stopped task ran on (`mpos-dbg: PARKWAKE id=6 stop_now=true`, the
record still set, nothing clearing it). The fix is `policy::leave_ready` plus a re-park loop that also lets
`SIGKILL` through, and the reason two earlier versions of that loop were reverted is now part of the record: the
fixtures they were written against stayed green without the fix **because a stop-state copy cannot answer the
question** — `set_trace_regs` only asks whether `trace.stop` is `Some`, and the old park left it set even as the
task ran. What distinguishes the two is whether the stopped task ran, so the shipped pin arms a 50 ms timer in the
tracee before its stop and asks `waitpid(WNOHANG)` 200 ms later: `FAIL: and the tracee has not run past it
(waitpid 6)` without the fix, `waitpid 0` with it (§6.88, item 201), and the 100-cycle detach claim stays as the
detach guard. §6.79's open paragraph is closed by this.

**What that left, and is still open:** `t-seccomp`'s "the tracer can rewrite the syscall number at the seccomp
stop" was the same park answered `ESRCH` (§6.85), and it is green now that the stop holds. A lane that ran past
its 900 s budget with no summary has no reading yet.

---


## Remaining ABI support (gaps + services)

The full inventory — grouped by what they block, with each refusal's reason — is  
[notes/appendix-status-board.md](notes/appendix-status-board.md)'s **R.Remaining**. In one line each:  
the delivery *point* is Linux's on **both machines** now - a handled signal is handed over on the
way back to ring 3 from a trap, not only from a system call, so a program computing without kernel
entries is still interrupted (`Ctrl-C` works): aarch64 since 2026-11-12, x86_64 since 2026-11-13,
where the PIT entry became a hand-written stub pushing a `TrapRegs` block because the
`x86-interrupt` ABI hides the general registers (`t-iret` measures both: 12 067 696 and 153 985 811
of 400 000 000 iterations, §2.13, items 261-262); what stays named is the narrower case - a delivery at
a trap while the task is traced falls back to the pending set, because a ptrace stop here is a
syscall-exit shape;  
`memfd_secret` is the one number left whose honest answer is `ENOSYS` — Linux's own answer without `CONFIG_SECRETMEM`, measured and pinned (`docs/notes/` §8.4, item 131) (the `rseq` registration, the `fanotify` notification class, the kernel keyring, the `io_uring` ring and `userfaultfd` are served); xattrs keep `user.*` on the memory filesystems and refuse the other namespaces by name (item 132, §3.15);  
the datagram lines are Linux's now - 65 507 for a datagram, and `EMSGSIZE` from the  
destination link's MTU where Linux's default would fragment it - while IP fragmentation  
itself stays a named gap behind a measured blocker (item 134, §4.6). A datagram socket's `SO_RCVBUF` now  
sizes its receive queue, as a stream's sizes its window (items 98 and 58's boundary,  
§4.3.3.1: the payload is what the queue is charged here where Linux charges each skb's  
truesize, the ring is capped at 64 KiB per socket, and the number still reads back its Linux  
figure). The namespace ladder has no rung left - all eight kinds of
`namespaces(7)` are served, cgroup's last (§6.25) - and `clone3`'s struct has no
unserved word left either: `CLONE_INTO_CGROUP` names a group now that the tree
exists to name (§6.26, item 105).

**The child is born in the group a descriptor names: `CLONE_INTO_CGROUP`**
(`docs/uapi/` §5.1 item 105, landed 2026-10-25). The last unserved word of
`struct clone_args`, and the one §6.10 deferred honestly - *there is no cgroup
tree here for a descriptor to name* - which stopped being true when the tree
landed the day before. Measured whole on a current host first
(`.workbuddy/probe_clone3_cgroup*.c`, Linux 7.2.11): the child and its own fork
read the target's path while the caller's line stays put; only a **group
directory** is a target (a file, a directory on another filesystem, a cgroup
attribute file and an fd that names nothing are `EBADF`, `O_PATH` is served); a
group removed after the descriptor was opened is `ENOENT`; `CLONE_THREAD` beside
the flag is `EOPNOTSUPP` into any group; an unprivileged caller is `EACCES`, and
the question is the **first common ancestor's** `cgroup.procs` - a world-writable
target under a root the caller may not write is still `EACCES`,
`CAP_DAC_OVERRIDE` answers it and `CAP_DAC_READ_SEARCH` alone does not;
`CLONE_NEWCGROUP` beside the flag roots the new namespace at the **child's own
group**; a target outside the caller's cgroup namespace is `ENOENT`; and
`clone(2)` accepts the bit and ignores it, as Linux does. Served by
`vfs::cgroupfs::clone_target` (the tree's answer: `EBADF`/`ENOENT`/`EACCES`),
`may_attach` (the common-ancestor DAC question), `proc::handle_clone3` (resolve
the fd, after every field rule - the measured order) and the spawn's `join_group`
inside the same critical section as the inheritance. The measurement also closed
a pre-existing divergence: the tree **reported** Linux's modes and policed
nothing, so `CgDir`/`CgFile` now keep them (a `0644` `cgroup.procs` is a gate,
as kernfs's is; procfs/sysfs stay reports, and the difference is recorded).
Pins: `t-cgroupfs` grows the tree half (12 claims, 26 → 38), `t-clone` the `clone(2)`
half, `t-clone3`'s `EBADF` claim is reworded to what it measured, and four host
tests hold the modes, the descriptor's three answers and the path questions in
`vfs::cgroupfs`. Named: the cgroup namespace's **mount** view is still the
machine's (the prior rung's divergence), and the tree has no delegation, so a
nested namespace owner is refused where Linux would let it through (§6.26).

**Every pid door answers the caller's namespace's number, and a `CLONE_NEWPID` child's two `SETTID` words are not the same number**
(`docs/uapi/` §5.1 item 107, landed 2026-10-25). Found by item 106's own pin and named there rather
than hidden: inside a pid namespace, `fork`/`vfork`, `clone` and `clone3`'s return value, `wait4`'s
return value and `waitid`'s `si_pid` all answered the scheduler's **task slot**. A parent outside
the namespace cannot see it (the slot *is* its number), which is why every existing claim was
green; a parent inside one - a container's own process - got a number no other call accepts.
Measured whole on a current host first (`.workbuddy/probe_pid_numbers.c`, Linux 7.2.11): `fork`
answers the child's number in the caller's namespace and `wait4(-1)` reports it; `clone3` likewise;
`waitid(P_PID, 6)` reports 6; and the two `SETTID` words are deliberately different - the
**parent's** word carries the child's number in the parent's namespace (4 for a `CLONE_NEWPID`
child whose own `number` is 1) while the **child's** own word carries its own namespace's number
(1). Served by one `pid_answer` helper for every door (they are one question, and a caller uses one
door's answer in another's call), `own_pid_number` for the child's word, and a `Retired::numbers`
chain on the exit record - the number has to be carried per namespace level, with the namespace's
identity, because by the time a `wait` reads a death the slot and its registration are gone.
Pins: the new `t-pid-numbers` (4 claims), and `t-kill-all` **loses the workaround** item 106's pin
needed - it names children by `fork`'s answer and waits by that number again, which is the second
half of the evidence. Named: `t-ns-pid`'s outside-parent claims are unchanged and still right; the
slot fallback in `pid_answer` survives only for records made before the chains were kept.

**A stream's receive buffer is its window, and a listener hands it to the socket it accepts**
(`docs/uapi/` §5.1 item 98, landed 2026-10-21): item 58 made `SO_RCVBUF` Linux's number, but on a
stream the number governed nothing — a listener's option stayed on the listener and every accepted
connection was built with the default ring, where Linux copies `sk_rcvbuf` onto the accepted socket
(`inet_csk_clone_lock`) and sizes the window from it. Measured whole on a current host first
(`.workbuddy/probe_tcp_rcvbuf.c`, Linux 7.2.11, a server that never reads): ask 2048 is inherited
as 4096 and absorbs 8192 bytes, 32768 as 65536 and absorbs 47104, 262144 as 524288 and absorbs
212886, and the answer is the same whether the option was set before or after `listen`. Served by
`tcp_rx_bytes` (half the stored figure), `add_tcp_sized`, `Listener::rx_bytes` through the pool,
`set_listener_rx` for a post-`listen` change, `settle_tcp` for a relocated socket, and an `accept`
that copies the stored number into the accepted socket. The pin is `t-sockbuf` grown to 18 claims
(two inheritances and three sized listeners pushed until they refuse; guest 18432/49152/147456,
host 8192/47104/286561). It also measured a send bug, and that fix ships with it: `tcp_send`
parked on a **partial** `send_slice` even under `MSG_DONTWAIT`, so a 4096-byte send against a
2048-byte window never returned; it now answers the count it queued, as Linux's `tcp_sendmsg`
does. Gate: x86_64 `pass=198 fail=0`; aarch64 `pass=197 fail=0`; `mpos-kunit` 32/0 on both. Named:
the datagram ring is still fixed (item 58), a connected stream's own `setsockopt` cannot resize
the live ring, the ring is capped at 128 KiB under §8.0's panicking allocator, and a stream's
`SO_SNDBUF` still does not size its send ring.

**A thread is its group's child: `CLONE_THREAD` implies `CLONE_PARENT`** (`docs/uapi/` §5.1  
item 66, landed 2026-10-19): item 65's own residual. A thread's `real_parent` on Linux is the  
parent of the group it joined, so a worker's `getppid`, `status`'s `PPid` and `stat`'s field 4  
all name the group's parent — where this kernel's thread carried no link at all and all three  
read `0`. Three behaviours move with the link and all three were measured on a current host  
first: a thread's death notifies nobody; a dying task hands its children to an alive task of its  
group before init (a thread's child to the leader, a leader's to a live worker — the mirror  
needed its own probe); and `wait` walks every thread of the caller's group, so the leader  
collects a child its worker forked. Served by the clone-path link, `waitable`'s group-and-leader  
rule (and `child_alive_for` asking the same question), a leader gate on the death path's  
notify/wake/console, `alive_thread_of`'s reaper choice, and a reap-time CPU fold only for  
leaders. Pin: `t-tid-dir` 37 → 45 claims, four counterfactuals one per half. What it named — a  
non-leader's `exit_group(2)` announcing nothing to its parent — landed as item 67 below.

**The group's last thread is the process's death** (`docs/uapi/` §5.1 item 67, landed  
2026-10-19): item 66's named residual, and the answer to *when* a process exists to be waited  
for. Linux's `group_dead` is the group's **last** thread leaving, so a non-leader's  
`exit_group(2)` is the process's death — the parent reaps the **leader**, with the dying thread's  
code — and a leader that leaves first on the thread door is a zombie (`Z`, `WNOHANG` 0, not  
`ECHILD`) until its group ends. Measured on a current host first  
(`.workbuddy/probe_exit_group_thread.c`, Linux 7.2.11, five scenarios): a worker's  
`exit_group(7)` reaps the leader with 7; a leader that `pthread_exit`s first is reaped only at  
the group's death and with the **last thread's** code (7 under `exit_group`, 5 under plain  
`exit`); a `clone(SIGUSR1)` group's death is invisible to a plain wait, collected by `__WCLONE`,  
and delivers the **leader's** byte (SIGUSR1, not SIGCHLD); and a thread's child is handed to a  
live task outside the dead group. Served by `exit`'s `group_dead` gate (the record names `tgid`,  
with the dying thread's code and uid and the leader's byte), `trace::group_has_live_task` with  
`is_child`/`child_alive_for` admitting a zombie leader, and `exit_group`'s reparent walk for a  
sibling's children. Pin: `t-exit-group` 4 → 22 claims, four counterfactuals one per rule.

**A device node made by `mknod` carries the number `mknod` was given** (`docs/uapi/` §5.1
item 71, landed 2026-10-20): the last thing standing in front of the call's `dev` argument, and one
§5.2 had charged to a prerequisite it does not need — a number-to-driver table, which is what would
make a number *mean* a device and not what it takes to keep one. Made here, such a node used to
report `st_rdev` `0:0` whatever number the caller named, was called a **character** device even
when asked for from `S_IFBLK`, and opened to read end-of-file and swallow writes — which is what
`/dev/null` does exactly, so a caller could not tell the device that is not there from the one that
is. Measured on a current host first (`.workbuddy/probe_mknod_rdev.c`, Linux 7.2.11, every row on a
tmpfs and again on an ext4): `makedev(136, 300)` reports `st_rdev` `0x0010882c` and decodes back to
`136:300` — the row the old packing would have read as `136:44`, another device's number, as every
pts above 255 is — `254:70000` reports `0x1110fe70` on a node `stat` calls a *block* device, only
the argument's low 32 bits are the number at all, `4095:1048575` fills the word whole, `statx`'s two
unencoded halves agree, and `0666` under `umask 027` leaves `0640` with the type field untouched.
The pair rides with the kind through `Inode::create` — one door rather than two, because a device
node made without one answers `stat` about a device nobody made — both `mknod` doors unpack the
argument with Linux's `new_decode_dev`, `struct stat` writes it through the inverse, and the node
that keeps it answers **`ENXIO`** from every file door: Linux's own word for a number no driver
owns, and here for all of them, since reaching a driver is still what this kernel's number does not
do. Pins: `t-mknod`'s five new claims (its `ok:` lines identical on the two machines, and every one
of them green on the host's Linux first) and `vfs::ramfs`'s two new unit tests.

**`MSG_PEEK` looks without taking, and the stack's own `peek` was the missing door**
(`docs/uapi/` §5.1 item 72, landed 2026-10-20): the last flag-word item on the IP receive path, and
the one §5.3 had parked behind a prerequisite that never existed — `tcp::Socket::peek` and
`udp::Socket::peek` have been in the stack all along, and what was missing was the reading of two
flags. Both doors refused `MSG_PEEK` with `EOPNOTSUPP` (honest while a peek could not be
preserved) and `t-sock-flags` pinned that. Measured whole on a current host first
(`.workbuddy/probe_msg_peek{,2,3,4}.c`, Linux 7.2.x): a datagram peek does not consume and a short
one leaves the message whole, `MSG_TRUNC` answers the datagram's own length, and a zero-length
non-peek receive *does* consume (a zero-length peek does not); a stream's `MSG_TRUNC` is a
different rule — it answers the count it would have copied and copies **nothing**, consuming what
it counted unless asked to peek — and a zero-length stream receive answers 0 (even when a signal
ends its wait) without taking a byte. `recvmsg` reports a cut datagram with `MSG_TRUNC` in
`msg_flags` whether or not the caller asked, says nothing for a stream, and still names the sender
on a peek. The fix keeps the copy's length apart from the message's (`RecvAnswer { copied, full,
endpoint }`), threads the two flags through `recv_common`, reads with smoltcp's `peek` and an
explicit dequeue, and copies datagrams by hand — which also replaces the `EMSGSIZE` a short
non-peek datagram used to draw with Linux's truncation. Pins: the new `t-msg-peek` (47 claims,
green on the host first, the two `MSG_TRUNC` readings among them) and a rewritten `t-sock-flags`
whose peek-refusal claim the served answer made false. Named: a **raw** socket's peek (the host
has no `CAP_NET_RAW`, so it stays the honest refusal), the other families' peeks, and `recvmsg`'s
zero-length data room, which predates this item. Gate: x86_64 `pass=190 fail=0`, aarch64
`pass=189 fail=0`, `mpos-kunit` 32/0 on both, both pins' `ok:` lines byte-identical; the
counterfactual (every peek refused again, one line) is `pass=188 fail=2`, exactly the two
pins. The pass also fixed `t-ns-net`'s handover fixture, which connected before the child's
listener announced itself - a race the new socket timing lost on aarch64.

**A `recvmsg`'s data room is the whole chain, and a stream's fault is not a datagram's**
(`docs/uapi/` §5.1 item 73, landed 2026-10-20): the item the `MSG_PEEK` landing above left named.
Every receive door but the message door takes one buffer, so `recvmsg` had read the **first
non-empty iovec** and stopped — a `4+4+2` chain got four bytes of a ten-byte message and answered
4 — and a zero-length data room was `EINVAL` for every family. Measured whole on a current host
first (`.workbuddy/probe_recvmsg_iov.c`, Linux 7.2.x): a datagram fills the chain in order and
stops where it ends, sets `MSG_TRUNC` in `msg_flags` without being asked, answers its own length
when the flag *is* asked for, and a zero-length chain consumes it; a stream has no message to cut
— `MSG_TRUNC` counts the bytes it would have copied, copies none and consumes them, a zero-length
chain answers 0 and takes nothing — and a fault on a segment costs a datagram its message but
leaves a stream's bytes queued. An iovec array longer than `UIO_MAXIOV` is `EMSGSIZE` on both
doors, where the send side answered `EINVAL`. Served by one `DataRoom` (the array read once, no
copy at all when the chain has a single non-empty segment) and by `RecvAnswer`'s third length —
`written`, 0 in the one shape whose count is not a copy. Pins: the new `t-recvmsg-iov` (56 claims,
green on the host first, `ok:` for `ok:` identical on both machines); the counterfactual (the
first-iovec rule restored, two lines) is `pass=190 fail=1` with 21 of those claims red, and every
single-segment, zero-length and `EMSGSIZE` claim still green. Gate: x86_64 `pass=191 fail=0`,
aarch64 `pass=190 fail=0`, `mpos-kunit` 32/0 on both. Named: a **raw** socket's zero-length
receive keeps its `EINVAL` and the **vsock** branch still reads its first segment — neither could
be measured on the host this contract came from.

**A raw socket's receive is a datagram's, and its hop count is read off the packet**
(`docs/uapi/` §5.1 item 74, landed 2026-10-20): the two receive shapes item 73 left named, both of
them named around a host that holds no `CAP_NET_RAW` - a zero-length data room answered `EINVAL`
and `MSG_PEEK` answered `EOPNOTSUPP`, because no probe could open the door. The probe makes the
capability instead of waiting for it: `unshare(CLONE_NEWUSER|CLONE_NEWNET)` hands the whole set
inside a user namespace of its own, and a network namespace that namespace owns is one
`ns_capable(CAP_NET_RAW)` answers for (item 43's rule from the other side). `lo` up, ICMP echo
*replies* to itself (type 0, the one the kernel's own ICMP input does not answer, so one send is
one packet), and every answer is the datagram's (`.workbuddy/probe_raw_recvmsg.c`, Linux 7.2.11):
a zero-length room answers 0 with `MSG_TRUNC` and consumes, no iovec at all answers 0 and
consumes, a 4-byte segment of a 36-byte packet answers 4 with `MSG_TRUNC` **unasked** and 36 when
the flag is asked, `MSG_PEEK` copies without taking through a full segment and a zero-length room
alike, a chain 4+`PROT_NONE` is `EFAULT` with the packet gone, 1025 segments is `EMSGSIZE` with
the packet still queued. The `IP_RECVTTL` record rides along even when the buffer cut the header
short, so the octet is read off the packet (`RecvAnswer::ttl`) rather than off the copy - and the
same measurement corrected two words this kernel had been writing: `cmsg_len` is the unpadded
`CMSG_LEN(4)` (20; 24 is the space it occupies, which is what `msg_controllen` reports) and
`cmsg_type` is `IP_TTL` (2), not the `IP_RECVTTL` (12) that asks for it, so `t-rawicmp`'s
control-data claim now reads the host's own answers. Pin: the new `t-raw-recvmsg` (50 claims,
green on the host first - the host run makes its own door as the probe does - and `ok:` for `ok:`
identical on both machines); its first guest run lost one echo beyond slirp and went red on a
packet that never came, so `queue_one` now drains, sends and confirms with a bounded peek, and the
claims it serves are named "an echo is queued" because that is what the helper promises. Gate:
x86_64 `pass=198 fail=0`, aarch64 `pass=197 fail=0`, `mpos-kunit` 32/0 on both; the
counterfactual (the two refusals restored) is the two programs this item touched going red and
nothing else. Named: the **vsock** branch still reads its chain's first segment only.

**The x86_64 signal frame carries the interrupted floating-point image, and a frame that names
none resets it** (`docs/uapi/` §5.1 item 75, landed 2026-10-20): the first machine's `rt_sigframe`
was 440 bytes, `sigcontext.fpstate` was null, and `rt_sigreturn` restored only the general
registers — so a handler that used an `xmm` register (all sixteen are caller-saved, so any
function may) resumed the interrupted code with the handler's values, a frame that named the
state and did not carry it. Measured whole on a current host first
(`.workbuddy/probe_signal_fp{,2}.c`, Linux 7.2.11): the 512 bytes `fpstate` names are the
`fxsave` block verbatim (ten bytes differ from a live `fxsave`, all at 464, the `_fpx_sw_bytes`
XSAVE header this machine has no extended state for; clearing the whole 96-byte padding still
restores the legacy area), and a frame whose pointer the handler nulls is accepted and
**resets** the state — zeroed `xmm`, `mxcsr` `0x1f80` — rather than leaving the handler's
registers live. The image now sits at frame offset 440 (`SIGFRAME_BYTES` 440 → 952, 16-aligned),
`sigcontext.fpstate` at `mcontext + 184` points at it, `SignalDelivery` carries
`fpu: Option<[u8; FPU_BYTES]>` from the lazy record (`sched::trace_fpu`) and `SignalFrame::exit`
answers `SignalFrameExit`, with a null pointer given `FpuState::reset()`. Pin: `t-signal-frame`
17 → 24 claims on x86_64, the handler now clobbering every vector register after reading the
frame; the counterfactual (the kernel files reverted) is `pass=190 fail=1` with exactly five of
its claims red, the alignment claim passing vacuously on the null pointer beside the non-null
one. Gate: x86_64 `pass=191 fail=0`, aarch64 `pass=190 fail=0`, `mpos-kunit` 32/0 on both.

**AF_UNIX's receive doors look without taking, and a datagram's `MSG_TRUNC` is its own length**
(`docs/uapi/` §5.1 item 76, landed 2026-10-20): both flags were refused `EOPNOTSUPP` on every
unix door. Measured whole on a current host first (`.workbuddy/probe_unix_peek{,2,3,4,5,6,7}.c`,
Linux 7.2.x): a stream peek copies without taking and is repeatable; a stream's `MSG_TRUNC`
changes nothing (it copies and consumes as the plain read does); a datagram peek leaves the
front message and reports its own length under `MSG_TRUNC`, a zero-length peek does not consume
while a zero-length non-peek receive does, and `recvmsg` sets `MSG_TRUNC` in `msg_flags` unasked;
a peeked `SCM_RIGHTS` record installs its descriptors and stays queued, so the following read
installs a second set; and a zero-byte handover answers `EAGAIN` on `recv`/`recvfrom`. Served by
`PipeEnd::peek_once`/`peek_scm`, `UnixSock::peek_dgram`, and a `RecvDgram` carrying the whole
length. Pin: `t-unix-peek`, 63 claims green on a host first, plus two `vfs::pipe` host tests;
the counterfactual (the four refusals back) is `pass=191 fail=1` with 18 of its claims red by
name. Gate: x86_64 `pass=192 fail=0`, aarch64 `pass=191 fail=0`, `mpos-kunit` 32/0 on both.
Named: `NETLINK`/`AF_PACKET`/vsock peeks, vsock's first-segment `recvmsg`, and a zero-byte
stream handover being receivable through `recvmsg` here where Linux answers `EAGAIN`.

**Netlink's and `AF_PACKET`'s receive doors look without taking, and a truncated answer names
its own length** (`docs/uapi/` §5.1 item 77, landed 2026-10-21): item 76's named residual on the
two families that were not reading the flag word. A `NETLINK_ROUTE` `recvfrom`/`recvmsg` popped
the front answer whatever `MSG_PEEK` said and returned the copy whatever `MSG_TRUNC` said, the
uevent socket's `recvmsg` did the same, and every `AF_PACKET` door refused `MSG_PEEK` with
`EOPNOTSUPP`. Measured whole on a current host first (`.workbuddy/probe_netlink_peek.c` over a
`RTM_GETLINK` dump; `.workbuddy/probe_packet_peek.c` inside
`unshare(CLONE_NEWUSER|CLONE_NEWNET)` with `lo` up and one frame the socket sends itself): a
peek answers the front's length and leaves it, `MSG_TRUNC` makes the return the message's own
length (3020 through 8 bytes on netlink, 16 on one frame) while the copy stays short, a short
receive answers the room and consumes, `msg_flags` reports the cut unasked, and a zero-length
room answers 0 - consuming without the peek and leaving the message with it. Served by
`rtnl::peek` + `recv_blocking(handle, look)`, `link::Frames::peek` with
`PacketSock::try_peek`/`peek_blocking`, and `uevent::peek`, with each of the five doors reading
the flag word. Pin: `t-netlink`'s 14 new claims, `t-uevent`'s 5, `t-dhcp`'s 4 (external group,
both packet doors, green there at `pass=199 fail=0`) and one `mpos-link` unit test; the
counterfactual (every `MSG_TRUNC` return put back to the copy's length, the peeks kept) is
x86_64 `pass=190 fail=2` - `t-netlink` and `t-uevent` red at the two length claims by name.
Putting the netlink/uevent peek back makes the same tests park on a queue the old take had
already drained, so that arm was observed and stopped rather than counted. Gate: x86_64
`pass=192 fail=0`, aarch64 `pass=191 fail=0`, `mpos-kunit` 32/0 on both. Named: the **vsock** family, measured in the same pass
(`.workbuddy/probe_vsock_recv.c` over `VMADDR_CID_LOCAL`, after `modprobe vsock_loopback`):
Linux serves its peek, ignores its `MSG_TRUNC` (4 of 10 answers 4 and consumes the four), uses
the whole iovec chain (4+4+2 answers 10) and answers 0 for a zero-length room without taking
anything - and serving the peek needs a look at the device seam, which the crate's private ring
does not offer, with no vsock peer on the sweep machine to pin it against.

**A named `SOCK_SEQPACKET` connection keeps its message boundaries, and its messages carry
their descriptors** (`docs/uapi/` §5.1 item 78, landed 2026-10-21): the last divergence in the
AF_UNIX family. A listener reached by a name handed `accept` a `DuplexEnd` byte channel, so the
one type whose description is "message boundaries over a stream" carried a byte stream for
exactly the connections that had a name (a `socketpair` half already carried messages).
Measured whole on a current host first (`.workbuddy/probe_unix_seqpacket{,2,3}.c`, Linux 7.2.11):
3 then 5 bytes read back as 3 then 5, a 4+4+2 `sendmsg` chain is one message of 10, a short read
discards the rest of that message, `MSG_TRUNC` returns the message's own length while `MSG_PEEK`
leaves it, a zero-length send is a message while `read(fd, buf, 0)` is a no-op,
`recvfrom`/`recvmsg` with a zero-length room consume, `SCM_RIGHTS` rides the message (a peek
installs its own set and the take a second, distinct one), and a path-name type mismatch is
`EPROTOTYPE` (an abstract name answers `ECONNREFUSED` from its type-keyed lookup). Served by a
`Pending` queue that holds either a stream end or an already-paired message socket,
`connect_messages`, the existing `recvq` machinery, `Dgram`'s own `fds`, and `read_at`'s
zero-length no-op. Pin: `t-unix-seqpacket`, 58 claims, green on a host first; `t-unix-dgram`'s
claim now says "listens" rather than "is served as a stream". Gate: x86_64 `pass=193 fail=0`,
aarch64 `pass=192 fail=0`, `mpos-kunit` 32/0 on both. Named at landing and served by item 79:
`sendto` naming a destination on an already-connected seqpacket socket answered `EISCONN` where
Linux sends to the peer.

**A send's destination on a connected AF_UNIX socket is the type's rule: a seqpacket connection
drops it, a datagram connection honours it** (`docs/uapi/` §5.1 item 79, landed 2026-10-21):
item 78's named residual, which turned out to be two opposite rules wearing one sentence. Both
message families answered `EISCONN` to any named send on a connected socket. Measured whole on a
current host first (`.workbuddy/probe_unix_sendto_connected{,2}.c`,
`probe_unix_sendto_family_only.c`, `probe_unix_sendto_long_name.c`, Linux 7.2.11): a connected
**seqpacket** socket sends to its peer whatever the name says — the listener's own name, another
live socket, a dead path, a dead abstract name, a stream listener, a family-only `sockaddr_un`
and an `addrlen` of 110 or 128 all send, because Linux clears `msg_name` before the shared
datagram send looks at it (a bad pointer is still `EFAULT`, and >128 still `EINVAL`, from the
syscall layer's copy) — while a connected **datagram** socket resolves and validates the name
exactly as an unconnected one does: another live socket receives, a dead path is `ENOENT`, a dead
abstract name and a regular file are `ECONNREFUSED`, a stream listener is `EPROTOTYPE`, a
family-only address is `EINVAL`, and the pinned peer only decides a send with no name. A stream
pair keeps `EISCONN`. Served by `UnixSock::ignores_send_address` (connected seqpacket only), a
`copy_send_address` that performs the copy bounds/readability check without interpreting the
bytes, and `send_dgram`'s two branches. Pins: `t-unix-seqpacket` 58 -> 66 claims (the three
ignored destinations, the 129-byte `EINVAL` and the empty queue) and six new claims in
`t-unix-dgram` (its `EISCONN` divergence claim gone), both green on a host first. Gate:
x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (the
old rule restored): `t-unix-dgram`'s first named-send claim is `EISCONN` and the blocking read
behind it parks, so that arm was stopped rather than counted. Named: an unconnected (or
listening) seqpacket socket's named send is `ENOTCONN` on Linux; the family's message doors are
not reached by one here, so its answer is unchanged.

**A datagram aimed at a path naming another type is `EPROTOTYPE`; an abstract name of another
type stays `ECONNREFUSED`** (`docs/uapi/` §5.1 item 80, landed 2026-10-20): item 79's named
residual. A datagram send or connect answered `ECONNREFUSED` for any live socket without a
message queue, so a path naming a stream or sequenced-packet listener looked like a dead name.
Measured whole on a current host first (`.workbuddy/probe_unix_dgram_wrongtype.c`, Linux 7.2.11):
a **path** naming a live socket of another type is `EPROTOTYPE` (91) for `sendto` and `connect`,
stream or seqpacket listener, connected sender or not; an **abstract** name of another type is
`ECONNREFUSED` (111) in every shape, because that table is keyed by the socket's type and the
lookup misses before any comparison. Served by `send_dgram`'s path branch (type compared after
the liveness upgrade) and `connect_to`'s datagram arm (`WrongType` instead of `Refused`). Pin:
`t-unix-dgram`, 49 -> 57 claims (two divergence claims become `EPROTOTYPE`, four new path/
abstract claims plus four setup claims), green on a host first. Gate: x86_64 `pass=193 fail=0`;
aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (old rule restored): x86_64
`pass=192 fail=1`, exactly the four path claims red with `ECONNREFUSED` (111) where the pin wants
`EPROTOTYPE` (91), abstract half green, sweep completes.

**A seqpacket socket with no peer sends nothing, whatever the name says: `ENOTCONN`**
(`docs/uapi/` §5.1 item 81, landed 2026-10-20): item 79's other named residual. An unconnected
or listening `SOCK_SEQPACKET` socket has message doors but no peer; the doors recognised only
sockets that carried messages, so the shape fell through to `ENOTSOCK` (88), a named `sendto`
included. Measured whole on a current host first
(`.workbuddy/probe_unix_seqpacket_unconnected{,_msg}.c`, Linux 7.2.11, unconnected and
bound+listening alike): `write`, `sendto` with no name, and `sendto` naming a live same-type
listener, a dead path, a regular file, a family-only address or a live abstract name all answer
`ENOTCONN` (107) - the name is not looked at, and a NULL pointer with a length is still "no
name"; `sendto` still refuses 129 bytes (`EINVAL`) and a bad pointer (`EFAULT`); `sendmsg`
imports the header, name bytes, iovec array and control buffer (`EFAULT`), without bounding
`msg_namelen` to 128 and without reading the iovec data, so a bad iovec base is `ENOTCONN`.
Served by `UnixSock::unconnected_seqpacket` and a branch in each message door. Pin:
`t-unix-seqpacket`, 69 -> 88 host-run claims, green on a host first. Gate: x86_64 `pass=193
fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (branch removed):
`pass=192 fail=1`, exactly the 17 message-door claims red with `ENOTSOCK` (88), `write` still
green, sweep completes.

**A descriptor's filesystem is the one it was opened on, not the root's** (`docs/uapi/` §5.1
item 89, landed 2026-10-21): `fstatfs(2)` answered by inode identity - a mount's own root inode
named that mount and everything else fell through to `/` - so every regular file, every directory
below a mountpoint and every `/proc/<pid>` file reported the root filesystem while `statfs(2)` on
the same path reported the mount that serves it. Two doors, one file, two answers, and `df` read a
mount's free space as the root's. Measured whole on a current host first
(`.workbuddy/probe_fstatfs.c`, Linux 7.2.11): Linux asks the vfsmount the open took, so `/` and
`/etc/hostname` both answer the machine's ext4, `/proc` and `/proc/self/status` both answer procfs,
`/dev/shm` and a file written into it both answer tmpfs, and `fstatfs(fd)` equals `statfs(path)` in
every row. The door now keys on the device number the open recorded (`File::dev`, the record
`fstat`'s `st_dev` already answered from, which is why *that* field agreed between the two doors
while nothing else did), and `MountTable::point_of_dev` walks `mount_points` - the table
`serving_point` reads forward - taking the deepest record carrying it, so the reverse lookup is the
forward one's mirror rather than a second rule that can drift from it. Pin: `t-statfs`, 24 -> 26
claims green on the host first and byte-identical on both machines; counterfactual (the inode-based
body restored) is `pass=204 fail=1` with exactly those claims red and everything else green. The
first draft's `/tmp`-shaped claims were **dropped rather than kept**: the first machine mounts a
ramfs at `/tmp` over a ramfs root, so they were green with the fix reverted too - a claim answered
alike either way is not evidence, and what replaced them compares the mountpoint's descriptor with
the one for a file inside it, which catches the old answer on either machine.

**Every mount now carries an `f_fsid` of its own instead of a zero they all shared** (`docs/uapi/`
§5.1 item 90, landed 2026-10-21): `f_type` names the *kind* of filesystem - two ramfs mounts, or
two ext4 mounts, share it - so a caller comparing two mounts through `statfs(2)` had nothing to
compare, and `f_fsid` is the field Linux's own man page offers for exactly that. This kernel wrote
zeros. Measured whole on a current host first (`.workbuddy/probe_fsid.c`, Linux 7.2.11):
`sizeof(struct statfs)` 120, `f_fsid` at 56, every row non-zero, `fstatfs(fd) == statfs(path)`
throughout - and the values split two ways, which is the whole constraint: `/proc` answers `0x18`,
**exactly its own `st_dev`** through Linux's `new_encode_dev`, while `/`, `/sys` and `/tmp` answer
UUID hashes that differ per machine and per boot. So the item pins the structure (non-zero, shared
by every name one instance serves, different between two live mounts) and says plainly that it does
not pin a value. The landing gives the field one source rather than a second identity beside
`st_dev`: `abi::encode_fsid` spells `new_encode_dev` (checked against a C program first), and both
doors feed it the device number they already report - `File::dev` for `fstatfs`, `dev_of_path` for
`statfs` - so the field and `st_dev` cannot disagree about which filesystem answered. Pin:
`t-statfs`, 26 -> 36 claims, green on the host first and byte-identical on both machines; the new
ones read a caller-owned 120-byte buffer by offset because neither wrapper exposes the field.
Counterfactual (the field cleared again) reddened the `write_statfs` host unit test first, then
`pass=204 fail=1` with exactly the three non-zero/different claims red - the two *equality* claims
stay green against zeros by construction, and are kept for the half-fix they do catch, not claimed
as the discriminating ones.

**A socket's buffer size is Linux's number, and a datagram queue is the size it names**  
(`docs/uapi/` §5.1 item 58, landed 2026-10-18): the pair the status board carried as  
record-only was wrong in both halves - the figure stored was the caller's rather than Linux's,  
and the figure governed nothing. Measured whole on a current host first  
(`.workbuddy/probe_sockbuf.c`, Linux 7.2.11): `setsockopt` stores **twice** the ask and  
`getsockopt` reports it (2304 -> 4608, 10000 -> 20000, 100000 -> 200000), each option has its  
own floor applied to the doubled figure (`SO_SNDBUF` 4608, `SO_RCVBUF` 2304 - asking 2048 for  
the receive side reads 4096, the doubling again above half the floor), the ceiling is the  
sysctl maximum applied to the *ask* (ten million reads back twice four mebibytes), and a  
negative ask is not an error but "as much as allowed" (`-1` answers 0 and reads 8388608). A  
fresh socket reports its family's own default - 212992 for both datagram options, 16384 and  
131072 for a stream. The governing half is measured too, with the dead-neighbour fill: 1400-  
byte datagrams stop after 2 at the floor, 4 at 9216, 9 at 20000 and 57 at 131072, while a  
5600-byte datagram is accepted into a socket reading 4608 - so the buffer bounds the queue and  
is not the datagram line. `net::sock_buffer_size` is the rule, `udp_send` asks it before the  
enqueue and `udp_room_for` asks it again (the park's predicate and the refusal must be one  
question), the datagram ring rose from 4 KiB to 16 KiB and from four packet slots to sixteen so  
that the buffer can be the smaller number, and the grace for an empty queue is Linux's. Pins:  
`t-sockbuf` (a new program, 12 claims, green on a host and on both machines) and `t-udpblock`'s  
fill, which now stops where the buffer's number says. Named at the time: `SO_RCVBUF` is a number  
and not a window (the receive ring is a fixed 16 KiB - a stream's became its window in item 98,  
a datagram's is still fixed), the datagram-too-big line was the ring's rather than 65507 at  
the time (closed since: item 134 makes it 65507 and the link's MTU the second line, while  
IP fragmentation stays a named gap), and a floor buffer admits three here against Linux's two (payload  
accounting against skb truesize).

**Giving the terminal back is the session's act, and only the session's** (`docs/uapi/` §5.1  
item 57, landed 2026-10-18): `TIOCNOTTY` is the residual item 56 named and did not serve. Linux  
answers two different things about *who* asks, measured whole on a current host first  
(`.workbuddy/probe_tty_notty.c`, six scenarios, each in a session of its own): a session leader's  
call gives up the **session's** terminal - its foreground group hears a `SIGCONT` and a `SIGHUP`  
(in that order, and again with the group already in front so no `TIOCSPGRP` could have sent the  
`SIGCONT`), the caller itself hears nothing when it is not in that group, the terminal keeps no  
foreground, `/dev/tty` is `ENXIO` for every task that still named it, a second call is `ENOTTY`,  
and the pair is **not** hung up (a `read` answers `EAGAIN`, a `write` lands) - while any other  
member's call is `0` too and clears only its own record, leaving the leader's `/dev/tty` opening  
and the terminal still naming the session's group. A descriptor that is not the held terminal is  
`ENOTTY`: the master end of the pair (a different terminal from the one `TIOCSCTTY` bound), a  
process with no controlling terminal, and a leader that never claimed one. The arm is `0x5422`,  
`sched::release_controlling_tty` does the writing (session-wide for the leader, caller-only  
otherwise), and `signal::SIGHUP` joins the vocabulary. Pin: `t-pty`'s fourth section, 18 claims  
(67 printed in the file, was 49); counterfactually six of them go red by name and the two  
refusal rows stay green.

**A terminal's foreground group is the session's, and a reader is not its owner** (`docs/uapi/`  
§5.1 item 56, landed 2026-10-18): the last divergence §5.2's terminal entry carried. Linux keeps a  
tty's foreground group strictly as *session state* — set when a session leader makes the tty its  
controlling terminal (`__proc_set_tty`) or by `TIOCSPGRP` from that session, and never by a read —  
while this kernel answered with the reader: `Tty::read` recorded the reading task's group as the  
terminal's foreground, so a program that read a tty it never claimed *became* its owner (the typed  
`SIGINT`/`SIGQUIT`/`SIGTSTP` and the resize `SIGWINCH` reached it, and `tcgetpgrp` named it), and  
a pair no session held answered `0` and accepted any group from anyone. Measured whole on a current  
host first (`.workbuddy/probe_tty_fg.c`, `probe_tty_fg2.c`, Linux 7.2.11, every scenario in a  
session of its own): a pair nobody holds answers `ENOTTY` on the *slave* and `0` on its *master*  
(Linux asks the controlling-terminal question of the slave end only, which is what lets a  
supervisor holding only the master learn whether it may do job control), `TIOCSPGRP` is `ENOTTY`  
there and from any process in another session, and from the owning session the refusals are ordered  
and distinguishable — `EFAULT` for a bad pointer, `EINVAL` for a negative number, `ESRCH` for one  
that names no process, `EPERM` for a group in another session, with the group it would not take  
left where it was. Reading is not claiming: a process in another session reads the slave and the  
answer does not move. Two shapes stay named rather than built, both measured: a background group  
that tries to change the terminal is `ENOTTY` when its group is orphaned and served when `SIGTTOU`  
is ignored (this kernel sends no terminal-generated `SIGTTOU`), and a background *read* of the  
terminal's own session is `SIGTTIN`'s business. `t-pty` gains 17 claims (54 in the file, was 37),  
all green on a current host first; the counterfactual puts back the two old halves and turns  
exactly nine of them red, including the reader's.

**A `poll` with no descriptors is a wait, and `pause()` is that call on arm64** (`docs/uapi/` §5.1  
item 55, landed 2026-10-18): a zero-`nfds` `poll`/`ppoll` answered `0` at once where Linux waits  
out the timeout (`poll(NULL, 0, 100)` answers after 100.1 ms; with no timeout it waits for ever).  
It mattered because musl builds `pause()` as exactly that call on aarch64 - Linux has no `pause`  
number there - so a program idling in `pause()` returned immediately and spun. Found by item 54's  
fixture, whose parked child exited at once on the second machine while every `Rss` in its file  
read zero.

**`/proc/<pid>/smaps` is served, and the three numbers this kernel cannot derive are a rule**  
(`docs/uapi/` §5.1 item 54, landed 2026-10-18): each record's header is that mapping's `maps` line  
byte for byte (both renderings of one gatherer, `procfs::mapping_lines`), then Linux's twenty-four  
field lines in Linux's order. `Pss`, the shared/private resident buckets and `Referenced` are the  
three this kernel's model cannot answer - no mapcount, no dirty bit with a page cache, no accessed  
bit - and each reads what the model does support rather than a fabricated number.

**A wait's two writes go where Linux's go — the wait, the rusage, then the record** (`docs/uapi/`  
§5.1 item 51, landed 2026-10-18): item 34 gave both wait doors the caller's `struct rusage`; what it  
left was *where* the writes sit, which only a caller with a pointer the kernel cannot write to can  
see. Linux collects (or refuses) first, copies the rusage when it has an event to report, and writes  
the six-word `siginfo_t` last on every path — so a bad pointer costs the caller its `EFAULT`  
*after* the child is gone, a refusal still writes the record (six zeros, and nothing else: the pad  
at 12 and every byte from 28 on stay the caller's), and the record's own `EFAULT` outranks `EINVAL`  
and `ECHILD`. This door cleared 128 bytes up front and refused before the wait, which is exactly the  
two divergences the entry named — a zeroed record where Linux leaves the caller's bytes, and a child  
still uncollected. The handler is now a collector that touches no pointer plus two writers in  
Linux's order; the record is written word by word (a record whose tail is unmapped is still served,  
and one whose word straddles the boundary stops there with the words before it written), and the  
rusage goes through the page-by-page copy that is `copy_to_user`, so a straddling one leaves its  
head written. That door's `*status` was already in this order, and the `wait4` four-byte word it left  
named in `docs/uapi/` §5.2 was measured rather than assumed (`docs/uapi/` §5.1 item 53, landed  
2026-10-18): Linux's `put_user` there is **atomic** — a word that straddles the page end is `EFAULT`  
with not one byte written, where the record beside it is the range copy that leaves its head behind —  
so this door's whole-word check followed by one store was already the right shape and no kernel code  
moved. The fourteen shapes are claims in `t-wait4` now (33 new, 84 in the file), which is what keeps  
a later "just copy four bytes" from landing quietly.

**`__WCLONE` narrows a wait to the kind of child `clone` named** (`docs/uapi/` §5.1 item 50, landed  
2026-10-18): `clone`'s low byte is the signal a child's death should report, and Linux reads it back  
at the other end — a child born to report `SIGCHLD` and one born to report nothing or something else  
are two kinds, and `wait4`/`waitid` only answer about the kind the option word admits (`__WCLONE` the  
clone kind alone, `__WALL` both, neither the non-clone kind alone). This kernel accepted the bit and  
reported every child, so a clone child was a child Linux says the caller does not have. The byte now  
survives on the task (and on the retired record, for the wildcard wait), and `sched::ChildKind` rides  
in the one eligibility rule every scan asks, so the seven places that look for "my children" cannot  
disagree. `fork`/`vfork` are `clone(SIGCHLD)` and unchanged, which is why every existing wait answers  
as it did. The narrowing is `ECHILD` — not `WNOHANG`'s 0 and not a park — so the blocking shape is  
pinned too. One thing stays named in `docs/uapi/` §5.2 — `__WNOTHREAD` is still
accepted-and-ignored — and the byte's *delivery* half, item 50's other end, **landed 2026-10-18**
(`docs/uapi/` §5.1 item 59): a child's death now carries the signal its `clone` byte named rather
than a constant `SIGCHLD`. The pin also caught a bug beside it: both doors read `options` as a  
whole register, where Linux declares it an `int` — so a C caller's `__WCLONE` (bit 31, sign-extended)  
was `EINVAL`, and the word is narrowed to 32 bits now.

**A `clone` child's own exit signal is what its death reports** (`docs/uapi/` §5.1 item 59, landed  
2026-10-18): item 50 read `clone`'s low byte at the *waiting* end; this is the other end — Linux's  
`do_notify_parent` sends the byte's own signal, so a `fork`'s child reports `SIGCHLD`, a  
`clone(SIGTERM)` child reports a `SIGTERM` with the child's pid, real uid and exit code, and a  
`clone(0)` child reports **nothing**, where this kernel raised `SIGCHLD` through every door. Measured  
whole on a current host first (`.workbuddy/probe_exitsig.c`, Linux 7.2.11, one scenario per  
disposition, the byte the only variable): the delivery predicate is not what either half of it  
suggests — `SIG_IGN` **drops** a non-`SIGCHLD` byte even when blocked (the one place "a blocked  
signal is never ignored" does not hold) while a blocked `SIGCHLD` is queued, and the set of exit  
signals Linux ignores by default is exactly `sig_kernel_ignore` (`{SIGCHLD, SIGURG, SIGWINCH}`), so  
a `clone(SIGURG)` death is dropped at `SIG_DFL` unblocked while a `clone(SIGTERM)` death at `SIG_DFL`  
unblocked **kills** the parent — and `SIGCONT` is *not* in it, which only its own probe can show (a  
`clone(SIGCONT)` child's death resumes a **stopped** parent; `probe_exitsig_cont.c`). Served by  
`sched::notify_parent_of_death` (the rule and the measurement) beside `signal::ignored_by_default`;  
pinned by the new program `t-clone-exit` (41 claims now — 29 here, plus item 63's twelve — green on  
a host first, byte-identical on both machines). The wake in the death path is unconditional and stays  
so — a parent parked in `wait4(__WCLONE)` for a `clone(0)` child is woken by nothing else. The  
residual this paragraph used to name is closed: **the record's own byte as a *report* landed  
2026-10-19** (`docs/uapi/` §5.1 item 63) — `/proc/<pid>/stat`'s field 38 carries the task's own  
byte, and `-1` for a thread.

**A read of a watched file reports `IN_ACCESS`** (`docs/uapi/` §5.1 item 60, landed 2026-10-19):  
the inotify module's own header had carried `IN_ACCESS` as a deliberate non-event since it landed,  
on a rationale that had gone stale — "the read path does not bump atime" — while the ext4 read path  
had since grown the `relatime` bump, and a read is an access whatever the stored time then says.  
Linux's `fsnotify_access` sits at the syscall boundary, not in the VFS, because the kernel's own  
readers (`read_file`, the `execve` ELF load) reach `File::read` too and `kernel_read` reports  
nothing. Measured whole on a current host first (`.workbuddy/probe_inotify_access.c`, Linux 7.2.11),  
one `IN_ACCESS` per successful read-family or copy call that moved bytes — `read`/`pread`/a  
two-iovec `readv` (one, not per iovec)/`sendfile`/`copy_file_range`/file→pipe `splice` — with  
nothing for a read at EOF, a zero-length read, a failed read, a directory read, or an `mmap` page  
fault, and `IN_MODIFY` (no `IN_ACCESS`) for the pipe→file `splice`. The owner is the syscall layer  
(`syscall::linux::fs::file::notice_access`, one helper the three read doors and the three copy doors  
ask) over `vfs::inotify::emit_access`; pinned by `t-inotify`'s 22 new claims (24 → 46, byte-identical  
on both machines) beside the host unit test. The shape it named as still open — a write made by  
`copy_file_range`/`splice` invisible to `IN_MODIFY` — is closed by item 61 below.

**A write of a watched file reports `IN_MODIFY`, once per call** (`docs/uapi/` §5.1 item 61, landed  
2026-10-19): the write half of the same rule, and item 60's own named residual. A `copy_file_range`  
or a pipe→file `splice` wrote through `Inode::write_at`, below the `File::write` chokepoint that  
reports, so a watcher on the destination heard nothing — and the `File::write` comment claiming every  
copy door reached it "so the event is said once" was false for two of them. Measured whole on a  
current host first (`.workbuddy/probe_inotify_modify.c`, Linux 7.2.11): one `IN_MODIFY` per call,  
however many pieces carried it — a two-iovec `writev`/`pwritev`/`pwritev2` reports one (not per  
iovec), a 10 KB `sendfile` (three chunks) one, a 10 KB `copy_file_range` one on the destination while  
its source reports none, and a pipe→file `splice` one. The fix is a `write_quiet` on `File`  
(`remove_privs` still runs, only the report is held) the vectored and copy doors write their call  
through, each saying the one event itself from the syscall layer's `notice_modify(fd)`, the mirror of  
`notice_access` (`handle_writev`, its body moved to `writev_inner`, plus `handle_sendfile`,  
`handle_copy_file_range`, `handle_splice` when the output is not a pipe). Pinned by `t-inotify`'s 21  
new claims (46 → 67, byte-identical on both machines) beside the host unit test. The module's other  
departures (`IN_UNMOUNT`, a watched directory's `IN_MOVE_SELF`, and - since item 267 - a watched  
regular file not surviving a rename) stay named in `docs/uapi/` §5.2.

**`numa_maps` and `smaps_rollup`: the two other spellings of `smaps`' accounting** (`docs/uapi/`  
§5.1 item 62, landed 2026-10-19): `/proc/<pid>` had `maps` and, since item 54, `smaps` — and  
neither of the two files `docs/uapi/` §5.2 had already measured, so `numastat -p`, `numactl` and  
every reader that wants a total rather than a walk found no file. Measured whole on a current host  
first (`.workbuddy/probe_numamaps*.c`, `probe_rollup.c`, Linux 7.2.11, one node), with two probes'  
worth of care about the fixture: **`/tmp` is tmpfs here**, so a "file" mapping made there is a shmem  
mapping whose folios are dirty from the moment the probe wrote them (a read-only mapping reported  
`dirty=4`), and **adjacent anonymous mappings merge**, so two shapes became one VMA and one of them  
looked absent — every shape is fenced by `PROT_NONE` pages now. `numa_maps` is one line per mapping  
in `maps`' order (the gate VMA excepted, and that is Linux's own: `[vsyscall]` is not a VMA of the  
address space): the **start alone, with no `-end`**, the policy — `default`, since `set_mempolicy`  
is not served — and then the name, which is `file=<path>` (a space `\\040`, a tab `\\011`), `heap`,  
`stack`, or nothing at all for a name that is not a path. A range with no resident page prints the  
policy and **no number at all**, not `N0=0`. `smaps_rollup` is one record for the whole address  
space: a header that is the **bounds** of the space — `<first start>-<last end> ---p 00000000  
00:00 0`, `[rollup]` in the same column 73 a `maps` line names its mapping — then twenty-two field  
lines, which is `smaps`' twenty-five less the three per-mapping sizes and the three per-mapping  
flags, plus `Pss_Anon`/`Pss_File`/`Pss_Shmem`. Its numbers are the sums of `smaps`' (`Rss` exactly,  
2096 and 2096; `Pss` not — 278 against 281, because each record's is rounded before it is added).  
Both renderers sit over the records `maps` already gathers and `resident_lines` is the one place  
the residency is counted, so the three files cannot disagree. Pins: `t-numamaps`, a new program, 20  
claims, and `t-smaps`' ten more (33 → 43) — all green on a current host first and byte-identical on  
both machines; the counterfactual (the two nodes unregistered) turns sixteen of them red by name.  
Named rather than approximated: `mapped=` and `mapmax=` are absent (no mapcount, the fact `Pss`  
already names), the `stack` word is absent (this kernel names the heap only), a read-only  
object-backed range says `dirty=N` where Linux says `mapped=N` (no dirty bit), and `Pss_Shmem` is  
zero (a `memfd`'s pages are an object's here). One divergence runs the other way and is worth its  
own sentence: **reading an anonymous page gathers nothing on Linux** — the read fault is satisfied  
with the shared zero page — where this kernel materialises a frame and reports it.

**`/proc/<pid>/stat`'s `exit_signal` is the task's own byte, and a thread's is `-1`** (`docs/uapi/`  
§5.1 item 63, landed 2026-10-19): the field item 50 and item 59 both named and left standing ("a  
rendering nothing pins"), and the last named thing on the `clone` byte's ladder. Linux stores the byte  
`clone` carried on the task, rewrites it to **`-1`** for a task created with `CLONE_THREAD` (a thread  
never notifies anyone of its death), and prints what it stored — so the field is a *signed* word  
carrying a number no signal byte can. Measured whole on a current host first  
(`.workbuddy/probe_exit_signal_field.c`, Linux 7.2.11), the byte the only variable: a `fork`'s child  
`17`, a raw `clone(0)` child `0`, a `clone(SIGTERM)` child `15`, a **thread** `-1` (the same answer  
its `task/<tid>` spelling and, on Linux, the tid's own `/proc/<tid>/stat` give), the *leader* of a  
threaded process its own `17`, and a **zombie** keeps the byte it died with. This kernel answered the  
constant `17` through every record, so a supervisor reading a `clone(0)` child's record or a thread's  
was told a `SIGCHLD` no death of theirs would ever deliver. The report is *derived* rather than read  
off the stored byte, because this kernel's threads deliberately store `SIGCHLD` (the one place a  
thread does leave a record is its group's `Retired`, and a default `wait` that named that slot must  
not read it as a "clone" child's) — so `vfs::procfs::exit_signal_field(byte, is_thread)` is the rule  
and `sched::TaskSnapshot` carries the byte: one field, one predicate, one rendering. The file is  
`stat` and not `status`: the two paragraphs that named this said "`/proc/<pid>/status`'s  
`exit_signal` line", and Linux's `status` has **no such line at all** (measured in the same run),  
which is corrected here and in `docs/uapi/`. Pin: `t-clone-exit`'s 12 new claims (29 → 41 — each  
record read from a **live** child, the leader's control, the thread's `-1`, and the zombie's  
state-and-byte pair) plus the host unit test  
`a_tasks_exit_signal_field_is_its_byte_or_minus_one_for_a_thread`, which needs no machine under it.  
The named shape this paragraph used to carry — a thread has no `/proc/<tid>` directory — is served  
now, by item 64 below.

**A thread's tid names a `/proc` directory, and a dead thread names nothing** (`docs/uapi/` §5.1 item  
64, landed 2026-10-19): the gap item 63's pin found and §5.2 carried, and the last name a thread's  
record had. This tree's `/proc` directory was a *leader*'s (`pid_exists` asked `snapshot(pid).pid ==  
pid`), so `/proc/<tid>` was `ENOENT` and a thread's record was reachable through  
`<pid>/task/<tid>/stat` alone. The question §5.2 said to settle first — which of the two names a  
*program* uses — is measured, on a current host under `strace` (`.workbuddy/probe_tid_dir.c`, Linux  
7.2.11): `ps -eLf` walks the group, with **306** opens of the *relative* name `task` beside not one  
absolute `/proc/<pid>/task/<tid>` path, while `gdb -p <tid>` opens **`/proc/<tid>/status`** outright —  
so the `task/` spelling is `procps`'s and the tid's own is a debugger's. Measured whole in the same  
probe, a live thread of a live group: the tid's directory carries the **process** file set — its names  
are the leader's exactly, 54 against 54 — where the `task/` spelling is the narrower *thread* set  
(47: `children` in, eight process-only names out), which is Linux's own  
`base_stuff`/`tid_base_stuff` split; `readdir /proc` still lists no tid; `stat`'s field 1 is the tid  
and field 38 `-1`, where the leader's record says the pid and its own byte; `status` is the pair  
(`Tgid` the group, `Pid`/`NSpid` the thread); and the *task's own* structures are what the tid's  
directory reads — a thread that `unshare(CLONE_FS)`s and `chdir`s reports its own `cwd` there while  
its leader keeps the group's, and one that `unshare(CLONE_FILES)`s lists its new descriptor under  
`/proc/<tid>/fd` alone, which is the half a leader-shaped answer gets wrong. A lookup also has to be  
Linux's gate rather than merely a wider one, so the measurement came with a clause: a task is  
unhooked from the namespace it was numbered in **at its exit** unless it leads its group  
(`__unhash_process`), so a dead thread is out of `/proc/<pid>/task` and its tid names nothing —  
`vfs::procfs::tid_has_a_directory`, one clause, asked at the two funnels that *name* a task  
(`process_dir` and `thread_ids`), because this kernel's table holds a dead thread's *slot* until the  
slot is reused. Pin: **`t-tid-dir`**, a new program, **26 claims**, green on a current host first  
(26 of 26, five identical runs) and byte-identical on both machines, plus `t-clone-exit`'s one more  
(41 → 42 — item 63's `-1` is now read through *both* spellings) and the host unit test  
`a_dead_threads_tid_names_nothing_and_a_zombie_leaders_does`. Gate: x86_64 `pass=188 fail=0 (188  
built)`, aarch64 `pass=187 fail=0`, `mpos-kunit 32 passed, 0 failed` on both. Counterfactual (the  
lookup back to `pid_exists`, the filter dropped): stage 1 untouched at 578, the sweep  
`pass=186 fail=2 (188 built)` with 18 red claims by name, and the one that stays green either way is  
the tid-names-nothing claim — it is the *live* readings that carry the item. One thing stays named in  
`docs/uapi/` §5.2: `children` is the *task's* child list on Linux and the process's here, measured —  
a child forked by a non-leader thread appears under that thread's `task/<tid>/children` alone.

**A task's `children` are its own, and the parent a record names is its group** (`docs/uapi/` §5.1  
item 65, landed 2026-10-19): item 64's residual, and the last word on *which task* a record points  
at. `children` is reached through the *group* — `/proc/<pid>/children` and `/proc/<tid>/children` are  
both `ENOENT` even for a task that has children, because the name is the thread set's alone — and it  
lists the children **that task** forked, where this kernel answered the process's through every tid's  
file. The same probe settled the link's other half, which no entry had measured: a child forked by a  
non-leader thread is named to its **group** — `status`'s `PPid`, `stat`'s field 4 and `getppid` all  
read the group's number (`.workbuddy/probe_children_owner.c`, Linux 7.2.11) — where this kernel named  
the forking thread's tid. `vfs::procfs::child_slots` is the rule (keyed on the link, leaders only, a  
corpse included until the reap), `children_of` writes Linux's own bytes (`<pid> ` per child, a  
zero-byte file for none) and asks the *task's* directory clause as its gate, which retired  
`pid_exists`, `sched::TaskSnapshot` carries the link beside a `ppid` that is now the parent's group,  
and `getppid` asks the new `sched::parent_group_of`. The fixture itself then found **two kernel  
defects**, both fixed here and both Linux's answer: `fork` from a non-leader thread answered  
`ENOMEM` (it asked the calling task's `space`, which a thread does not own — the group's is the  
leader's slot), and a reaped child lingered in the table until the next scheduler `reap()`, where  
Linux's `release_task` runs inside `do_wait`. Pin: `t-tid-dir`'s **11 new claims** (26 → 37),  
green on a current host first (37 of 37, three runs), plus the host unit test  
`a_children_file_lists_the_children_the_task_itself_forked` (the gate figures are §15.84's). One  
thing stayed named in `docs/uapi/` §5.2: a **thread's** own parent — this kernel's thread carried  
no link at all (`parent: None`, so its three doors read `0`), where Linux gives it its group's  
parent (`CLONE_THREAD` implies `CLONE_PARENT` — measured, a thread reads the group's parent  
through all three doors). It landed the next day as item 66 (§15.85), with the behaviours a link  
change moves: a thread's death notifies nobody, a task's exit hands its children to an alive task  
of its group, and `wait` walks every thread of the caller's group.

**The `F_GETFL` word is the caller's own** (`docs/uapi/` §5.1 item 49, landed 2026-10-18): Linux  
answers `filp->f_flags`, so a caller that compares the word against what it opened with agrees — its  
mode, `O_DIRECTORY`, `O_NOFOLLOW`, `O_PATH`, `O_SYNC`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME` — where  
this kernel answered the mode plus `O_APPEND` and `O_NONBLOCK` and nothing else. The open now records  
the word it was asked for (`File::born`), minus exactly what the open *spends* (the creation flags  
and `O_CLOEXEC`, which is a descriptor flag), and `F_SETFL` still reaches only its own bits — the  
split Linux's mask makes rather than a rule of this kernel. Two members of Linux's word stay  
deliberately missing and named in `docs/uapi/` §5.2: `O_LARGEFILE`, which this kernel neither defines  
nor invents, and two of the five bits Linux's `SETFL_MASK` carries (`O_ASYNC` and `O_DIRECT` are  
reported when an open asked for them but cannot be set afterwards — `O_ASYNC` for want of  
`fasync`/SIGIO plumbing, `O_DIRECT` for want of a direct-I/O path and because its number is  
machine-specific). The third, `O_NOATIME`, became settable 2026-11-13 (§6.148, item 265).

**The device tree removes a directory it made** (`docs/uapi/` §5.1 item 48, landed 2026-10-18): the  
`mpos: syscall 84 (path /dev/dri/by-path) -> EOPNOTSUPP` a boot printed once per boot was udev's  
per-boot idiom — remove that directory, then publish the `by-path` links under it again — meeting a  
filesystem that had no `rmdir` at all and an `unlink` that took a directory as readily as a device  
node, so the two doors were not a split. Both nodes own the door now, with Linux's order in front of  
the name going (`ENOTDIR` for a kind that is not a directory, `ENOTEMPTY` for one that still holds  
something), a refused `rmdir` therefore takes nothing away, `unlink` answers `EISDIR` where it used to  
remove, and the mount point's `EBUSY` was already the mount layer's (item 45). The boot that logged  
the line is quiet now and its `by-path` links are published. One shape stays named in `docs/uapi/`  
§5.2: a driver's own node cannot be re-made through `mknod` here — the device tree's names come from  
the drivers.

**A directory is not a byte stream** (`docs/uapi/` §5.1 item 47, landed 2026-10-18): its four read  
doors answer `EISDIR` — the *kind* before the *count*, so even a zero-length read of one is refused  
where the same count on a file is `0` — where this kernel answered `EOPNOTSUPP`, and `sendfile`'s  
*source* question answers `EINVAL` beside it, a different word for a different reason (the copy is  
a splice, and a directory has none). The write half needs no word of its own: a directory cannot be  
opened writable at all, and that refusal is where its `EISDIR` comes from.

**A FIFO is a channel now, not a file** (`mknodat(S_IFIFO)` served 2026-10-16, `docs/uapi/` §5.1  
item 26): its `open` runs Linux's **rendezvous** — a reader and a writer meet at the name, a  
peerless open parks (or is `ENXIO` under `O_NONBLOCK`), a parked reader counts as a reader so the  
arriving writer releases it, and the buffer is discarded when both counts reach zero — and the one  
shape that open used to leave behind is closed too (`docs/uapi/` §5.1 item 28, landed 2026-10-17): a  
**fatal** signal now *ends* the park instead of ending the task inside it, so the count comes back  
before the death, which happens at the syscall gate — Linux's own order, where `err_rd:` runs on  
`ERESTARTSYS` and the task dies after the syscall has unwound. `mknod`'s `dev` argument **carried no device number until item 71 landed on 2026-10-20** — a node
made here now reports the pair the caller named and `open(2)` of it answers `ENXIO`, no driver stands behind any number `mknod` can be given — this kernel's devices are objects a
driver published under a name, rather than entries in a table keyed by that word.

**A procfs/sysfs node reports its own word, not the kind's default** (`docs/uapi/` §5.1 item 37,  
landed 2026-10-17): both trees are filesystems that keep no modes, so every directory answered  
`0755`, every file `0644` and every link `0777` whatever Linux's creator had said about the node.  
Measured whole on a current host first, the word now follows each node's own *shape* — which turns  
out to be Linux's rule on both trees (`proc_create` with no mode means `S_IRUGO`; `DEVICE_ATTR_RO`  
against `DEVICE_ATTR_RW`): a proc directory `0555`, a file nothing can write `0444`, one that can  
`0644`, `/proc/<pid>/environ` `0400` where its neighbour `cmdline` is `0444`, `/proc/<pid>/ns`  
`0511`, the live `/proc/bus/pci/<bus>/<dev>.<func>` window `0644`, `/proc/cmdline`'s `st_size` its  
own line's length, `/sys` itself `0555` while every directory under it is `0755`, a read-only  
attribute `0444` and a writable one (`uevent`, the door `udevadm trigger` writes) `0644`. Three  
things stay named in `docs/uapi/` §5.2: the read-only nodes Linux makes writable, the fact that a  
word here is a *report* rather than a gate (procfs polices nothing), and the three names whose  
*kind* is a directory here where Linux answers a link. The same pass' measurement turned up a fourth  
divergence of its own, now named in §5.2: the no-follow resolution splits the last name off its  
parent, so the final component's mount crossing never happens and `lstat("/proc")` answers the  
directory the mount covers.

**A device node asks `CAP_MKNOD` now** (`docs/uapi/` §5.1 item 36, landed 2026-10-17): exactly the  
two device types do — `S_IFREG`/`S_IFIFO`/`S_IFSOCK` nodes are served to a caller holding not one  
capability — in Linux's place in the order (after the type, the name, the name's existence `EEXIST`  
and the directory's own `EACCES`), with the character node whose `dev` is `0` served too, because  
that is how overlayfs spells a whiteout. The corner this paragraph used to name — Linux asks  
`capable(CAP_MKNOD)`, which is the **initial** user namespace, where this kernel's checks asked the  
caller's own — **closed 2026-10-17** (`docs/uapi/` §5.1 item 38): the capability model now carries  
the two questions Linux asks (`capable()` for the initial namespace, `has_effective()` for the  
caller's own), and the four gates whose Linux rule is `capable()` ask it — `mknod`, `FS_IOC_SETFLAGS`  
at the driver's/`fileattr_set`'s door, `reboot` and `syslog` — measured one namespace at a time  
(a user-namespace root is served `chroot` and `unshare(CLONE_NEWNS)` and refused those four). Two  
scopes the one word cannot express stay named in §5.2 with their measurements, and both landed on  
2026-10-18: `setns`'s and `AF_PACKET`'s ask the *object's* namespace (§5.1 item 42, the paragraph  
below), and `CAP_DAC_OVERRIDE` asks the *file's* filesystem's —
**whose DAC half landed the same day** (`docs/uapi/` §5.1 item 39): the permission model's override  
is now Linux's two capabilities (`CAP_DAC_OVERRIDE` for a read and a write on a non-directory  
whatever its bits are, `CAP_DAC_READ_SEARCH` for a read on its own, execute only where some execute  
bit exists) asked of the file's namespace through `Credentials::capable`, which is where the two  
passes meet. **And the *owner-shaped* calls landed on 2026-10-18** (`docs/uapi/` §5.1 item 40):  
`chmod`, `chown` and the `utimes` family ask `inode_owner_or_capable` — the node's own uid, or  
`CAP_FOWNER` — rather than "is the caller root"; `CAP_CHOWN` is what a `chown` naming *another*  
owner asks, and naming the ids the node already holds is still a `chown` rather than a no-op;  
`CAP_FSETID`, not the uid `0`, is what stands in for being in a file's group when a `chmod` would  
keep the set-group-id bit; and the timestamps call turned out to be **two** questions, since write  
permission buys the "both clocks to now" form (`EACCES` when it is refused) and nothing buys an  
explicit pair (`EPERM`). The same pass measured what a `chown` does to the two set-id bits and  
found this kernel's rule wrong in the other direction: they go for **every** caller, capability or  
not — even a `chown` that names nothing at all, and even root holding the whole set — with two  
exceptions, a set-group-id that carries no group execute (a locking mark, not an authority) and a  
directory. **And the same capability's third door landed with it** (`docs/uapi/` §5.1 item 41): a  
**write** to a file costs it the set-uid and set-group-id bits — a write, a `truncate`, an  
`ftruncate` and an `open`'s `O_TRUNC` alike — and `CAP_FSETID` is the only thing that spares them,  
which is the reason a set-uid program cannot be handed over by being edited in place. Measured,  
three things belong to the rule rather than to the caller: a write that writes *nothing* costs  
nothing, owning the file buys nothing (a caller writing another user's world-writable file loses the  
bits too), and a set-group-id that carries no group execute is a mandatory locking mark that no door  
takes — the clause a `chown` honours and a `chmod` does not. Two shapes stayed named in §5.2 that  
night: `fallocate`'s resize and a write through a mapping. **The first of them closed the next day,  
and turned out to be wider than a resize** (`docs/uapi/` §5.1 item 52): `fallocate` is a write even  
when it writes no byte. Linux's `vfs_fallocate` ends **every** successful path in `file_modified`,  
so the set-id bits go and the clocks move for a `KEEP_SIZE` allocation and for a range already  
inside the file exactly as for a grow — measured with the `mtime` pinned to a known second first,  
on ext4 and tmpfs agreeing — while an immutable file refuses the call with `EPERM` before anything  
is allocated and an append-only one is served, which is `may_modify`'s question and not  
`may_alter`'s. This door reached the inode's `truncate` directly and passed none of it. One shape  
stays named: a write through a mapping, which Linux does not strip either.

**And the two object-shaped scopes landed the same night** (`docs/uapi/` §5.1 item 42): a capability
question about a namespace *object* asks that object's owner, not the caller's own standing —
`setns` asks `ns_capable(ns->user_ns, CAP_SYS_ADMIN)` about the namespace being **joined** and
`AF_PACKET` asks the same of the network namespace the socket is opened in, so a caller inside a
bare user namespace holding the whole set *in it* is refused the mount namespace it is standing in
(the no-op every other kind serves, which is what proves the question comes first) and served one
made inside its own. `vfs::ns`'s new owner table records whose each namespace is where it is made —
with a user namespace its own owner, Linux's `ns_common::user_ns` — and `userns::Caller::capable_in`
is the walk: the target is the caller's and the set is the answer; the machine's own reached from
below is not privileged over it; the caller *made* the next one down and holds every capability in
it whether or not its own set carries the bit; otherwise up one rung. That rung's last named gap
was **closed the same day** (`docs/uapi/` §5.1 item 44): a namespace descriptor *is* the namespace
now — the procfs link's followed name denotes a namespace-shaped file (`vfs::ns::NsFile`) holding
the object resolved at the open, so `setns` joins the namespace the descriptor named rather than
wherever its maker stands by then, and `stat`/`fstat` report a regular `0444` file whose `st_ino`
is the namespace's **own** number, which is what Linux's `nsfs` inode is
(`.workbuddy/probe_ns_fd_stat.c`: `lstat` symlink `0777`, `stat`/`fstat` regular `0444` with
`st_ino` `4026531832`, size 0, `read` `EINVAL`, `lseek` `ESPIPE`, a write-ish open `EPERM` for
root and unprivileged callers alike and `O_APPEND` served; `.workbuddy/probe_ns_fd_pin.c`: the
three readlinks `4026531832`/`4026532932`/`4026531832`). `lstat` and `readlink` keep answering
live, because they stop at the link — the split Linux makes.

**And the third raw door was measured and closed with it** (`docs/uapi/` §5.1 item 43): the *same*
question in the door `ping` and a DHCP client take — `AF_INET`'s `SOCK_RAW` — which asked the
caller's own effective set for `CAP_NET_RAW` where Linux asks the network namespace's owner, exactly
as `AF_PACKET` does. The measurement is the other probe in the same three positions
(`.workbuddy/probe_rawip_ns.c`, Linux 7.2.9): the machine's root served, a caller inside a bare user
namespace holding the whole set *in it* refused `EPERM`, the same caller served the moment it has a
network namespace of its own — so the refusal was never about the set. Both raw doors now ask one
helper (`net::socket`'s `net_ns_capable`), because the two are one question and a second copy of it
is a second place to drift; `t-ns-net`'s section walks both doors in one child. Nothing else moved:
the capability is still asked **before** the protocol is judged, and a root caller is privileged
over the machine's own network namespace, so no guest answer that existed before changed.

**A mount point answers the mount, and a name a mount sits on may not be taken away**  
(`docs/uapi/` §5.1 item 45, landed 2026-10-18) — the last named divergence in the filesystem's own  
resolution, and the one §5.2's entry had called "not a two-line fix" because one helper answered two  
questions: the no-follow resolver looked the last name up *in its parent*, so the crossing the  
walking resolver performs on every child it looks up never happened for the final component  
(`lstat("/proc")` answered the ramfs directory the initramfs made, `0755`, where `stat` answered the  
mounted root's `0555`), while the make-or-remove doors want that same covered name — Linux's `rmdir`  
of a mount point is `EBUSY`, not the removal of the mounted root. **Measured whole on a current host  
first**, in nine sections (`.workbuddy/probe_mountpoint_doors.c`, Linux 7.2.9): the reading and  
altering doors *cross* the mount — `lstat`, `stat`, `statx` with and without  
`AT_SYMLINK_NOFOLLOW`, `newfstatat`, `open`, `faccessat2`, `chmod`, `utimensat` — because the  
crossing is not what `LOOKUP_NOFOLLOW` turns off (the flag is about links, and a mount is stepped  
into whatever the flag says); a plain bind covers the crossing up, since a non-recursive bind clones  
no submounts, which is why the crossing has to come from the walk's own state; the make-or-remove  
doors answer `EBUSY` for the name (`rmdir`, `unlinkat(AT_REMOVEDIR)`, `rename`) and `EEXIST` for a  
make (`mkdir`, `mknod`, `symlink`); and the questions ahead of that refusal are ordered — the type  
pair first (`rename(file, mounted dir)` is `EISDIR`, `rename(mounted dir, file)` `ENOTDIR`), then the  
permission half, then the mount point, which is what an unprivileged caller in a `0555` directory  
holding a mount point reads as `EACCES` for all of `rmdir`, `unlink` and `rename`. What landed is  
one mode on the walk (`resolve_walk`'s `LastLink::Follow`/`NoFollow`, which the no-follow resolver  
now *is*) plus one question at the refusal (`is_mount_point`, a fact about the node rather than the  
view, asked last in `may_remove` and on either end of `renameat2`); the old parent-and-lookup body  
stays as `resolve_entry`, which is exactly what the three doors whose question is a *name* want.  
`t-mountpoint` is the new pin, 37 claims on both machines — it mounts its own tmpfs, so it needs no  
fixture from the boot — and `t-proc-modes` now claims `/proc` and `/sys` through both doors (20  
claims, was 18). Two findings stay named rather than served: a `rename` between a bind's own view  
and the outer tree is `EXDEV` on Linux and is still served here, and a *file* mount point — the only  
case where `unlink`'s half of the refusal is reachable — cannot exist in this kernel at all, because  
every mount it can make lands on a directory.

**The three procfs names Linux makes links are links** (`docs/uapi/` §5.1 item 46, landed  
2026-10-18) — the third and last named consequence of item 37, and the one that program's own header  
carried from the other side: `/proc/self`, `/proc/thread-self` and `/proc/mounts` are `0777` links  
on Linux, where this kernel answered a directory (a mirror of the reading process) for the first two  
and a plain `0444` file for the third. **Measured whole on a current host first**  
(`.workbuddy/probe_proc_self_links.c`), and two of its questions are the ones a table cannot guess.  
The target text is the reader's number **in the pid namespace the procfs instance belongs to** — a  
process whose `getpid()` is 1 inside a fresh pid namespace still reads the outer number from the  
machine's `/proc`, and reads `1` from a `/proc` it mounts itself inside it. And the `st_size` follows  
how the name was *made* rather than what it answers: 0 for the two live targets (registered empty)  
and 11 for `/proc/mounts`, the length of `self/mounts`. The node was already here — `ProcLink`,  
whose `0777` mode and live target are this measurement's answer — so the landing is a `registered`  
length, a `fixed_link` helper, the two names built from `link(|| ...)` through the number this tree  
names the reader by, and the deletion of `MirrorDir` with its last two callers; every name reached  
*through* a link (`/proc/self/status`, `/proc/self/fd/N`, `/dev/stdin`) reaches the node it reached  
before. `t-proc-modes` gains 15 claims (35 in the file, was 20), `DT_LNK` in the listing included,  
and all of them pass on a current host first. One residual stays named: this tree's `/proc` names a  
task by the *reader's* pid namespace throughout, where Linux keys an instance to the namespace it  
was mounted for — the same answer for the container case, and different for a task reading a `/proc`  
mounted outside its own namespace.



**A child's death is one `SIGCHLD` record, and it is Linux's** (`docs/uapi/` §5.1 item 29, landed  
2026-10-17): the second book this kernel kept for "an exit whose signal is unread" is deleted — the  
exit queues the real signal with `CLD_EXITED`/`CLD_KILLED` and the status beside it, and a death the  
parent never asked to be signalled for leaves nothing readable (measured on a host first, all four  
dispositions). One semantics stays named there: an **ignored** child is reaped by Linux and kept as a  
zombie here. Three more this paragraph used to name are closed: a record now carries the child's CPU  
in **`si_utime`/`si_stime`** (`docs/uapi/` §5.1 item 33, landed 2026-10-17 — `USER_HZ` ticks, through  
the descriptor, a handler's frame and `sigtimedwait`, while `waitid` carries none, which is Linux's  
own answer), a signalled death is  
now **`CLD_DUMPED` with `WCOREDUMP`** when the signal's default action dumps core *and the child was  
dumpable* (`docs/uapi/` §5.1 item 32, landed 2026-10-17) — a bit that follows the signal rather than  
a file written, which is what the measurement corrected in this kernel's own comment about it — and  
**`wait4`/`waitid` fill their `struct rusage`** (`docs/uapi/` §5.1 item 34, landed 2026-10-17): the  
child's CPU for every event they *report* — a death, a stop and a resume alike, and not for the  
`WNOHANG` "nothing yet" zero — with Linux's own order on a bad pointer (`wait4` writes `*status`  
first, `waitid` copies the record first), and the last residual of that family named rather than  
approximated in `docs/uapi/` §5.2 (the `waitid` record is cleared up front, so a bad `rusage` leaves  
zeros where Linux leaves the caller's bytes).

One of those "buffer sizes" is served now, and the *blocking* half of it was the last of it: a  
**blocking UDP send waits for the send buffer** (`docs/uapi/` §5.1 item 27, landed 2026-10-16) — the  
wakeup source it was waiting for turned out to be the stack's own poll rather than a TX completion  
interrupt, and what the two earlier shapes got wrong was the *condition* (the metadata ring alone,  
which is true while the enqueue still refuses on the payload ring — a park that returns at once, and  
so spins). Measured whole on a current host first: with the buffer full behind a neighbour that never  
answers ARP, three datagrams go in and the fourth blocking send sleeps 3072-3098 ms and then returns  
it, where the non-blocking one is `EAGAIN` at once. Three things stay named: Linux's `EMSGSIZE` line  
is the maximum UDP datagram (65507), not the send buffer, so a datagram wider than `SO_SNDBUF` is  
taken there and refused here — the buffer-size item below answered that line (the ring is 64 KiB  
now and the size answer is 65507, item 134); the park is **bounded** at three seconds (the window Linux's own neighbour resolution  
takes), because smoltcp keeps a datagram its egress could not dispatch where Linux's device drops the  
frame and releases the charge, so an unbounded park would be a hang; and the ring's wraparound, which  
the condition cannot see and which clears as the buffer drains. The MTU story that program also caught  
is a test-side lesson, not a gap: it lives with §10.4's measurement.

---

*The phase-by-phase effort/impact table this file used to end with is retired: P1–P9's estimates  
predated Phases 12–15 landing, and "Recommended order: P1 → … → P9" orders work the status board  
above marks done. What is open lives in the board and the moves above; `docs/notes/`'s appendix  
carries the per-phase history.*
- [x] **`getcpu`'s two output pointers are two writes** — a bad `node` answered `EFAULT` and threw
  away the `cpu` word the caller had a legal pointer for, because the handler judged both pointers
  and then wrote both. Linux attempts each and or's the errors: `(good,BAD)` = `EFAULT` with `cpu`
  written, `(BAD,good)` = `EFAULT` with `node` written, `(BAD,BAD)` = `EFAULT` with neither
  (`build/scratch/getcpu/r.c`). One loop in Linux's order with an accumulated verdict; the values
  were never the divergence (one CPU, no NUMA node, so both are `0` before and after). Pin
  `t-getcpu` 15 claims, 2 red counterfactually; host 762 unchanged (no pure half — `buffer_ok` is
  always true on the host). (§6.128, item 242)

- [x] **`/proc/<pid>/stat`'s `tty_nr`, `tpgid` and `session` are three answers** — fields 6, 7 and 8
  were `pgid` copied into the session column and the borrowings `"0"`/`"-1"`, on the reasoning that
  no `setsid` had ever been served and that everything types at one console. Both had expired:
  `setsid`, `TIOCSCTTY` and ptys are all served. Linux recodes the terminal's device id into
  `tty_nr` (`0x8801 -> 34817`), answers **the terminal's** foreground group in `tpgid` — the same
  number for every task on the line, and one that moves for both files at once on `tcsetpgrp` — and
  puts the task's session in field 6, which is the leader's id for its descendants
  (`build/scratch/ttynr/b.c`). The copy was the invisible half: it is right for every process that
  never unshared, and wrong exactly where the answer carries information. Pin `t-stat-tty` 12
  claims, 8 red counterfactually (the `session 12` where the leader is `11` is the discriminating
  one); host unit tests `762 -> 763`. (§6.129, item 243)

- [x] **`/proc/<pid>/stat`'s minor-fault columns are counters** — fields 10/11 (`minflt`/`cminflt`)
  and `struct rusage`'s `ru_minflt` (offset 64) were literals. Linux keeps two books: the group's own
  faults and each thread's own (`/proc/<pid>/stat` and `/proc/<pid>/task/<leader>/stat` read `5401`
  and `4525` while a worker's `task/<worker>/stat` reads `403`), and a reaped child's total folds into
  `cminflt`, which *is* `RUSAGE_CHILDREN.ru_minflt` (`build/scratch/statflags/{j,k,l,m,o,r}.c`).
  `Mm` gained `minflt`/`cminflt` and `Task` gained `minflt`, charged at the three sites where this
  kernel actually takes a frame (`populate_in`, `split_cow`, `resolve_stack_growth`) and folded at
  reap; `getrusage` splits three ways. `majflt`/`cmajflt`/`ru_majflt` stay a named-gap zero: nothing
  on the frame path says whether a page came in with I/O. Pin `t-stat-faults` 14 claims, **5 red**
  counterfactually, and three of its own claims had to be rewritten first because the act of measuring
  moved the counter being measured. Host unit tests `763 -> 764`. (§6.130, item 244)
- [x] **`/proc/<pid>/stat`'s `flags` is the task-state bitmask** — field 9 printed a literal `0` for
  every task. It is five facts in one word, and all four inputs already existed: `PF_FORKNOEXEC` is
  `Task::user_resume.is_some()` (cleared at exactly one place, the exec path), the death bits are
  `exited`, `PF_SIGNALED`/`PF_DUMPCORE` are bits of the wait status field 52 already computes (§6.112),
  and `PF_RANDOMIZE` is the ASLR this kernel really implements. One leaf function
  (`crates/abi::stat_flags`) beside `tty_nr` and `wait_status`; `sched` computes the word, `vfs` renders
  it. Measured against the host with ASLR on and off: `0x400000` live, `0x400040` forked-not-execed,
  `0x40844c` a `SIGTERM` zombie, `0x40864c` a `SIGSEGV` one — the dump bit follows the signal, not the
  file. Pin `t-stat-flags` 16 claims, **7 red** counterfactually. Host unit tests `764 -> 765`.
  (§6.131, item 245)
- [x] **`sched_rr_get_interval`'s blocked child is a machine claim** — §6.131's gate reddened on
  `a child blocked in a read is answered zero (state S, got 0, 4000000 ns)` on the loaded aarch64 arm,
  the same line §6.127's gate had reddened on earlier. It was the pin, not the kernel: the reference
  host fails that claim itself — `build/scratch/rrstate/r.c` read `S` both before *and* after the call
  and still got the 2 ms slice in 689 to 5586 of 30000 samples, the rate tracking load. `/proc`'s letter
  and the call's answer are two reads of one moving record, so no threshold over the two is stable. The
  pin now asserts the asymmetry instead (a runnable child beside a blocked one is never answered zero,
  and the blocked child's counts print in the same line), holds `blocked_zero == 200` and the zombie
  claim only on the machine — where the answer is taken under the same table lock, so there is no
  window — and leaves the stopped child's claim unconditional (stable 5/5 rounds on the host). Twelve
  consecutive host runs green, machine 13/13. (§6.132, item 246)
- [x] **`read`/`write` ask about the descriptor before the buffer** — `read(-1, bad_pointer, 8)` has two
  things wrong with it and Linux answers `EBADF`; this kernel answered `EFAULT` because all four of
  `handle_read`/`handle_write`/`handle_readv`/`handle_writev` validated the caller's buffer in their
  first statement, before resolving the descriptor. `build/scratch/rworder/{o,z,p,v,w,x,y,z2,copy,
  copy2}.c` measured the host's whole order: offset (`pread` only) → descriptor (`EBADF`, including
  wrong mode and length zero) → kind for the scalar doors (`EISDIR`, but *after* the iovec array for the
  vectored ones) → the caller's buffer, and only a node that actually moves bytes ever asks. The VFS
  layer already had the order right and said so in a comment, but the prefix was inlined in
  `File::read`/`File::write_reporting`, unreachable from the doors above; it is now `File::pre_read`/
  `pre_write` behind `vfs::pre_read`/`pre_write` and `pre_read_mode`/`pre_write_mode` (mode only, what
  the vectored doors need), with `abi::iov_count_verdict` reading the count as Linux's `int` rather than
  the register's `usize`. Pin `t-rw-precedence` (11 `(errno, value)` claims) is `PASS` on both arms and
  **4 of 11 red** counterfactually. Named gap: `write(/dev/null, BAD, 8)` still answers `EFAULT` where
  Linux answers `8`, because the fault-in happens once up front rather than at the copy site — a
  VFS-wide change, carried as a printed observation in the pin. (§6.133, item 247)
- [x] **`/proc/<pid>/wchan` is the wait a task is in now** — the file did not exist. `stat` field 35
  is the *last* park and stays set after a wake (measured: one task asleep reads `f35=1
  wchan="anon_pipe_read"`, the same task awake reads `f35=1 wchan="0"`), so serving the file from it
  would have looked finished and been wrong for every running task. New `src/sched/wchan.rs` holds
  `Wait`/`wchan_word` and the park-site table, **moved out of `src/sched/runtime/task.rs`** so the
  stall dump and this file read one list; `task_wait` in `src/sched/runtime/procfs.rs` answers it, and
  the file is at both spellings. Pin `t-wchan` (12 claims) is `PASS` on both arms and **7 of 12 red**
  counterfactually. Two claims moved while writing it: "a task we may not inspect reads 0" is false
  on the machine (the payload is root, so it *may* inspect `pid 1`), and "it is asleep while it says
  so" was a race — this kernel's pipe read is a *timed* park, so the claim samples the pair instead of
  comparing two reads. **The gate then found a second, unrelated bug**: the aarch64 sweep's root was a
  196 MiB filesystem holding a 196.58 MiB tree, leaving 1.89 MiB free where `t-mmap-file-lazy` writes
  2 MiB — `SWEEP_DISK_BYTES`'s comment still said the payload was "~57 MB" and it is 111.82 MiB. The
  root is now sized from the tree (`tree_bytes` + `ROOT_SLACK_BYTES`, 32 MiB of room, the caller's
  constant as a floor), which took the same image from 485 free blocks to 8371. (§6.134, item 248)
- **`/proc/<pid>/io` exists, and its numbers are the ones Linux counts.** The file was absent
  entirely, so a program asking how much it had read got `ENOENT`. It is seven lines, mode `0400`,
  and the three rules that make the numbers Linux's are each a place a plausible kernel is wrong:
  `rchar`/`wchar` take the **return value** while `syscr`/`syscw` count the **calls** (so an `EFAULT`
  or `EISDIR` still counts as a call and an `EBADF` does not); a vectored call counts **once**; and
  `/proc/<pid>/io` is the group's book where `task/<tid>/io` is the thread's. `Task` and `Mm` carry
  the four counters and one `sched::charge_io` feeds both, `charge_cpu`'s shape; the charge sits at
  the point the answer is known in all seven byte-moving doors. The three device-layer lines print
  zero as a **named gap** - nothing counts device-layer bytes per task. Pin `t-proc-io` (13 claims)
  is `PASS` on both arms and **10 of 13 red** counterfactually; two claims moved while writing it
  because the fixture charged what it measured. (§6.135, item 249)
- **`/proc/<pid>/auxv` reports the vector the program was handed.** The file was absent, and the
  vector existed only on the initial stack: this kernel wrote it and kept nothing, so there was no
  second door onto it. `auxv()` now records the bytes it pushes into `Loaded::auxv`, `Mm::saved_auxv`
  holds the address space's copy (set at init, spawn and `exec`; copied at `fork`), and both
  `/proc/<pid>/auxv` and `/proc/<pid>/task/<tid>/auxv` read it at `0400` with `st_size` 0. The file is
  not text - a `u64` value with `0x80` in it is not valid UTF-8 - so a generated `ProcFile` renders
  bytes now, the text helpers converting without a copy. The promise the copy buys is the measured
  one: a `setuid` after `exec` does not move the ids the file reports, because the stack's vector did
  not move either. Pin `t-proc-auxv` (16 claims) is `PASS` on both arms and **15 of 16 red**
  counterfactually. Named gaps: this kernel's vector is sixteen entries where Linux's is
  twenty-three (the loader's gap, not the file's). The zombie's refusal was the other half of the
  promise: it refused at the **read** and always as `ENOENT`, where Linux refuses the **open** with
  `ESRCH` (and `ENOENT` only once collected) - closed by item 268, which also captured the body the
  open took. (§6.136, item 250; §6.149, item 268)
- **`/proc/<pid>/timers` lists the group's POSIX timers, and a `timer_t` is a counter now.** The file
  did not exist, so a program could only remember what it had created; writing it exposed the worse
  defect underneath: the handle **was the slot index**, so `delete(1); create()` handed `1` back and
  the old handle began naming a different timer - a silent retarget no test could see, because none
  of them deleted anything. Linux keeps a per-process counter (`0,1,2,3,4`, delete `1`, create -> `5`)
  and lists the table descending; `timer_alloc` now returns that counter while still picking the
  lowest slot, and `timer_arm`/`timer_state`/`timer_overrun`/`timer_remove` resolve through
  `timer_slot`. The file is `0444`, four lines per timer (`ID:`/`signal:`/`notify:`/`ClockID:`), the
  `signal:` value the `sigval` as 16 hex digits, the subject named in the reader's namespace
  (`pid.N`, and `signal/tid.N` for a thread-directed timer - the word before the slash names the
  mechanism), `ClockID` the caller's own id for a wall clock and `-6` for the process CPU clock, no
  per-thread spelling, and a zombie keeps the file **empty** until the reap. Pin `t-proc-timers`
  (18 claims) is `PASS` on both arms and **15 of 18 red** counterfactually. Named gaps: the
  eight-slot table answers `EAGAIN` where Linux is bounded by `RLIMIT_SIGPENDING`, and `TIMER_ABSTIME`
  is still refused. (§6.137, item 251)
- `readahead` and `sync_file_range` were answered by their subject rather than their range: the first
  returned 0 for every descriptor that existed, so a write-only descriptor, a directory, a pipe and a
  character device all got the answer Linux reserves for a file it will really read; the second had no
  dispatch arm and fell into `_ => ENOSYS`, which has no fallback for a caller whose whole point is
  not waiting. Both now ask four questions in the measured order, and the two orders are not the
  same: `readahead` asks descriptor → mode (`EBADF`) → regular file (`EINVAL`) → signed range
  (`EINVAL`), while `sync_file_range` asks descriptor → range (`EINVAL`, including a sum that wraps -
  which `readahead` accepts) → flag word `1|2|4` (`EINVAL`) → a node with something to flush
  (`ESPIPE`). The order witnesses are calls with two faults at once: `readahead(999, -1, 4096)` is
  `EBADF`, and `sync_file_range(pipe, flags=8)` is `EINVAL` where the same pipe with `flags=0` is
  `ESPIPE`. `SYNC_FILE_RANGE` is `277` on x86_64 and `84` on aarch64. Pin `t-range-advice`
  (22 claims) is `PASS` on both arms and **19 of 22 red** counterfactually. Named gaps: no
  `MAX_READAHEAD` window and no deferred writes, so the three flush bits have nothing to wait on.
  (§6.138, item 252)
- `fadvise64` was written on "it is only a hint, so every advice on every open descriptor succeeds",
  and three answers came out of that sentence: the node's kind was asked *after* the advice word, so
  `(pipe, advice=6)` was `EINVAL` where Linux answers `ESPIPE`; the range was not asked at all, so
  `(-1, 4096)` and `(0, -1)` were 0 where Linux answers `EINVAL`; and the advice was compared as the
  whole `usize`, so a caller's `0x1_0000_0003` was refused where Linux reads the low 32 bits and sees
  `POSIX_FADV_NORMAL`. The door now asks descriptor → kind → range → advice, which is the **opposite**
  order from `sync_file_range`'s flags-before-kind: two calls with the same arguments and the same
  errno vocabulary order their four questions differently, so neither can share a validator. Only a
  real pipe is `ESPIPE` (a `socketpair` end, a socket, a character device and a directory are all
  served), and a wrapping range is accepted as it is by `readahead`. Pin `t-fadvise` (18 claims) is
  `PASS` on both arms and **5 of 18 red** counterfactually. Named gap: every recognised advice is
  accepted and does nothing, nothing being cached evictably. (§6.139, item 253)
- `name_to_handle_at`/`open_by_handle_at` answer `EOPNOTSUPP` - the word Linux uses on a filesystem
  without export operations - instead of `ENOSYS`, and ask their questions in Linux's order: the flag
  word before the descriptor, the name before both output pointers, and `handle_bytes` before the
  capability. Neither output pointer is validated, because nothing is going to be written there;
  the capability is the discriminator the pin reads out of `/proc/self/status`, so the same program
  says `EPERM` on an unprivileged host and `EOPNOTSUPP` in the guest. 18 claims, 17/18 red
  counterfactually (`ENOSYS`). (§6.140, item 254)
- `/proc/<pid>/fdinfo/<n>` exists - the directory and the file - with Linux's four generic lines
  (`pos`, `flags`, `mnt_id`, `ino`) each a *relation* to a door that already answers the same fact:
  `lseek`'s position, `F_GETFL | (FD_CLOEXEC ? O_CLOEXEC : 0)` (the second half is the descriptor's,
  not the description's), the `mountinfo` column, and `fstat`'s `st_ino`. The per-kind lines are a new
  `Inode` hook: `eventfd` (count in **hex**, width 16), `signalfd` (the wire `sigset_t` in 16 hex
  digits), `timerfd` (the **remaining** span, not the stored deadline) and `epoll` (one `tfd:` line per
  watch, five widths and two radixes). `inotify`'s block is a named refusal - the watch table keys
  watches by the filesystem *instance* id rather than `st_dev`, so its `sdev` column has no honest
  source, and the `f_handle` tail needs export operations no filesystem here has (item 254's finding).
  Pin `t-fdinfo` (24 claims) is green on the host first; the host unit tests compare whole measured
  lines, which is what caught a `flags:` rendered with `{:o}` instead of Linux's literal-`0` prefix.
  (§6.141, item 255)
- `/proc/<pid>/personality` and `/proc/<pid>/oom_adj` are served as **views**, each with one owner:
  the first from `sched::personality_of`, the same function `personality(2)` now asks, so the two
  doors cannot drift when a second persona lands; the second has no store at all - it converts to and
  from the one `oom_score_adj` word, both directions measured, with `-17` mapping to the modern
  *floor* `-1000` rather than through the multiplication (which would give `-1133`, a number the
  scale does not have). The guest found a real divergence on the way: this kernel handed back a
  descriptor for a write-open of a generated read-only file and answered `EOPNOTSUPP` on the write,
  where Linux's `proc_reg_open` asks the node's write *method* and answers `EACCES` at the open for
  root too - `ProcFile` now says it has no write method, which fixes every read-only generated file.
  Pin `t-proc-words` (19 claims), green on the host first and in the guest. (§6.142, item 256)
- `/proc/<pid>/syscall` exists - nine fields (`nr` decimal, six arguments and the two registers in
  `0x` hex) for a task inside a call, the word `running` for one in ring 3, and Linux's three-field
  `-1 0x0 0x0` for a zombie. The record is taken at **dispatch entry** (the arguments are in registers
  the handler clobbers and the frame is on the task's own kernel stack), cleared when the dispatch
  returns - *before* signal delivery - and cleared again where `execve` replaces the image, since that
  call never returns and would otherwise describe a fresh program for ever. `sp` needed a new
  one-register machine question (`Gate::user_sp`) rather than a whole `UserFrame` copy, because this
  runs on every syscall. Pin `t-proc-syscall` (14 claims): its strongest claims are that the file,
  read *by* a `read(2)` of the test's own, describes that very call and its descriptor. (§6.143, item
  257)
- `/proc/<pid>/oom_score` exists - the OOM killer's badness for a task, a **view** computed on each
  read from the resident pages `statm` already counts and the one `oom_score_adj` word, with the
  proportion taken against the managed frame total `/proc/meminfo` prints: `clamp(rss*1000/total +
  adj, 0..=1000)`. `0444`, read-only and `EACCES` at a write-open for root too. The reference host
  measured here does not obey the documented formula literally at all - at a fixed small resident set
  its score moved by two thirds of the adjustment (`200 -> 800`, `1000 -> 1333`) and went past the top
  of the scale - so no absolute score is claimed: the implemented arithmetic is the documented formula
  on this machine's own numbers, and the pin's 13 claims are relations (a larger adjustment scores
  higher, a larger resident set scores higher, another task readable, a gone pid `ENOENT`) that both
  machines keep. `t-oom-score` is green on the host first and in the guest, 10/13 counterfactually
  red without the file. (§6.145, item 258)
- the **poll-window class** is closed across the user suite: a claim that samples a *parkable* task
  now asserts the state the kernel reports and lets sampling only establish that the state is
  reachable. The audit of all 321 programs found four still carrying the shape - `t-proc-syscall`'s
  parked-child sample (measured `199 asleep of 200`, one look in two hundred catching the timed
  park's runnable moment) and its fixed-sleep zombie, `t-wchan`'s zombie claim (a fixed 400 ms sleep
  that never asserted `Z`), `t-waitid`'s named child sampled after a 2000 ms settle loop against its
  own 150 ms sleep, and `t-rr-interval`'s one-letter conditional - a full aarch64 lane read
  `99 asleep of 200, 8 disagreed` because the letter is a *different syscall* from the call it
  conditions, so the fix reads the letter on **both sides** of it (`blocked_moved` counts a letter
  that moved apart, a slice with `S` on both sides is the disagreement) and requires a floor. Host
  PASS and 3/3 boots per changed program; no kernel behaviour touched. (§6.146, item 259)
- the **fuzz workspace** is in the gate, and running it found a real defect on the first day. `fuzz/`
  is its own cargo workspace (it must be - the kernel's `[build] target = x86_64-unknown-none` cannot
  link a libFuzzer binary), so nothing in `cargo xtask test` had ever built it: measured 2026-11-12,
  **8 of 22 targets had stopped compiling** over the month after the last commit that touched the
  directory. Repaired, then *run* - and `ext4_image` at `-runs=400` panicked 6/6 in `read_bytes`'
  own net, with a block number of `1869902697` on a 128-block image. The cause was one level up:
  `ptr_at` vetted its result and never its argument, and its argument is `i_block[12]`/`i_block[13]`
  straight off an unvalidated on-disk inode. Release builds survived it by accident (the zero-fill
  fallback turns it into a hole), which is why the invariant that net asserts had been false without
  anyone knowing - and why a `debug_assertions` fuzz run is the instrument that asks. Fixed at both
  consumers of that word: the read answers `None` (a hole) and the write path answers `Corrupt`,
  because a `put_ptr` into a block the volume lacks is dropped by `update_block`'s vet and reports
  success while moving nothing. Two gate steps now hold it there: a declaration check (a
  `fuzz_targets/*.rs` no `[[bin]]` names, and a `[[bin]]` naming nothing) and a build-and-run step
  (**13.4 s** cold, sancov-instrumented - the same as a plain build, and an uninstrumented one only
  ever reports `exec/s: 0`), where a seeded target executes its corpus from a copy under `build/`
  because libFuzzer appends to whatever corpus it is given. The `lim:` measurement matters here:
  libFuzzer ramps input length with the run count (`lim: 4` at 200 runs, `lim: 198` at 20000), so a
  bare `-runs=N` exercises the shortest inputs and nothing else. A new `seccomp_filter` target closes
  the one rule-2 debt the audit found. The same week's sibling finding: `cargo fmt --check` never saw
  `fuzz/` either, and eleven diffs across five files were sitting in it. (§3.29, §1.5, item 260)
- **`cargo clippy` and `cargo fmt --check` are in the gate now**, and neither had ever been run by
  any stage of it: `grep -rn 'clippy\|rustfmt' xtask/src/` answered nothing, while `AGENTS.md` §2 had
  listed both as commands a change runs since that document was written. The first was *red* -
  `cargo clippy --target x86_64-unknown-none` exited 101 on `clippy::never_loop` in
  `src/net/socket.rs`, in a `loop` whose body always returned, wrapped around a wait by the commit
  that meant to bound it. Nothing about the runtime was wrong (the timeout lived in the inner
  `while`), which is exactly why no test could see it and why the lint was the only instrument that
  did. A clean baseline was not reachable honestly: 162 diagnostics on x86_64, 168 on aarch64, 389 on
  the host `--lib` half (336 of them `dead_code`, because a machine-only caller does not exist
  there), 273 in the user suite, 3 in xtask, 1 in the loader - and `cargo clippy --fix` ran for
  9080 ms and left the tree byte-for-byte identical. So the gate holds a **ratchet**: six arms, each
  with its per-lint counts recorded in `xtask/src/clippy.rs`, failing on an `error`-level diagnostic,
  on a count that grew, and on a lint that arm has never carried - while a count that *fell* prints
  the line telling its finder to lower the constant, because a ratchet left high cannot catch a
  regression back up to it. `cargo fmt --check` joins it for both workspaces, `fuzz/` included, which
  is §1.5's lesson arriving with an instrument rather than a note. Measured warm at 110-214 ms per
  arm (cold 3.4-8.5 s), so it sits in stage 1 with the other cargo invocations; the four fixes it
  drove were each a claim that was false rather than a style preference - `mut_from_ref` in
  `t-io-uring`'s hand-built ring, `erasing_op` in an anti-optimisation that `& 0` had folded away,
  `absurd_extreme_comparisons` on a `u64`, and the socket wait above. (§1.6, item 263)
- **`status`'s resident set is three books now, not one number with three names**: `RssAnon` had
  printed the *whole* `VmRSS`, `RssFile`/`RssShmem` a literal `0 kB`, and `statm`'s third field
  (`shared`) a literal `0` - three places named the gap on the reason "the frames table does not
  record what backs a page". The frames table is just `Vec<usize>` frame numbers, but the two tables
  that *describe* the address space already say what backs each range and `smaps` has been reading
  them for months. Measured on Linux 7.2.14: the three lines sum to `VmRSS` exactly, `statm.shared`
  is `RssFile + RssShmem` in pages, a touched 4 MiB private anon mapping moves only `RssAnon` by
  4096 kB, a shared anon mapping moves `RssShmem`, and writing a private file page moves 64 kB from
  `RssFile` to `RssAnon` (CoW). `rss_split` recounts the VMA and eager ranges by backing object,
  runs **outside** the task-table lock (`snapshot_of`'s three callers are all inside it, and a
  gigabyte of pages must not hold interrupts off), and makes anon the **remainder** so the sum holds
  by construction even when one frame is described twice. New `MappedObject::is_shmem()` / not-gated
  `Inode::is_shmem()` hooks answer shmem for SysV segments and memfds; `[stack]` eager ranges are the
  one anonymous exception, by `vm_book`'s shipped name precedent. `TaskSnapshot`/host `Process` carry
  the split in pages; `vm_block` reads it off `task` rather than taking an 11th argument, because
  clippy's ratchet (item 263) is already at `too_many_arguments`' limit. Linux's counts are `%8lu`, so
  a `vm_count` right-aligns them. **Named residual**: without a page cache this kernel cannot
  reproduce the per-page CoW move (`FilePages::frame_at` always makes a private copy), so the split
  is an honest model over ranges, not a per-page truth. Pins: `t-rsssplit` (13 claims, green on real
  Linux, on a one-shot x86_64 boot and in both gate arms), host unit test
  `the_rss_lines_and_statm_shared_are_read_from_the_split` (counterfactually red against the old
  whole/0/0 renderer). Gate `build/gate-rsssplit-final.log` `GATE_EXIT=0`: stage 1 1311 tests, x86_64
  `pass=308`, aarch64 `pass=307`, kunit 35/0, lint/formatting clean, fuzz 23. (§6.147, item 264)

- **`F_SETFL` can set `O_NOATIME`, and the flag has teeth** (`docs/notes/` §6.148, item 265).
  `docs/uapi/05b-small-measured.md` §5.2 had carried one sentence for months: `SETFL_MASK` here is
  narrower than Linux's by `O_ASYNC`, `O_DIRECT` and `O_NOATIME`, all three *reported* when an open
  asked for them but none *settable*. Measured on Linux 7.2.14 (`build/scratch/setfl/{p,q,r,s,t,u}.c`):
  `F_SETFL(O_NOATIME)` sets and clears the bit in both directions (an open's own bit included),
  the bit is obeyed (atime forced to `1000000000` stays put through a read, moves again once the bit
  is cleared), and a **fresh** 0->1 set by a caller that is neither owner nor `CAP_FOWNER` is `EPERM`
  with no partial store - while re-affirming (1->1) and clearing (1->0) are served. `SETFL_MASK` gains
  `O_NOATIME`; the `born`/`status` split needs no new field because the open writes both halves, so
  the bit moves from the immutable half to the changeable one and a later `F_SETFL` can clear it, as
  Linux's can. The teeth are one line: `File::read` asked `born()` (always 0 after the widening) and
  asks the effective `open_flags()` now. The guard is a pure host-tested predicate
  (`setfl_fresh_noatime`) because the `F_SETFL` arm is bare-metal only, and it refuses **before**
  `set_status` so a refusal stores nothing. **Named residuals**: `O_ASYNC` needs `fasync`/SIGIO
  plumbing this kernel has none of (Linux itself reports it only for an inode with a method - a
  regular file's `F_SETFL(O_ASYNC)` returns 0 and the bit never appears), and `O_DIRECT` has no
  direct-I/O path here and a machine-specific number (x86 `0o40000`, aarch64 `0o200000`). Pins:
  `t-setfl` (9 claims - green on real Linux and in the guest, including the guest-only stranger
  `EPERM`), three host unit tests, and a guest counterfactual that reddens four claims with the old
  mask and keeps the `EPERM` one green (`build/scratch/oneshot-setfl-cf.log`). Gate
  `build/gate-setfl.log` `GATE_EXIT=0`: stage 1 1313 tests, x86_64 `pass=309`, aarch64 `pass=308`,
  kunit 35/0 each, lint 162/162 + 168/168 + 389/389 + 273/273 + 3/3 + 1/1, formatting and fuzz clean,
  `t-setfl` green in both arms. (§6.148, item 265)

- **`item 266`** (2026-11-13): **the boot-hang instrument is a build switch, so an ordinary console
  belongs to whoever is using it.** `src/sched/runtime/task.rs`'s `heartbeat` printed a table of
  every task slot every five seconds in *every* build - ten lines a beat on this machine, 105 lines
  in one lane's 2920-line log - which is a console nobody can type on: five seconds of quiet is not
  enough to type a command. It is now `Cargo.toml`'s `heartbeat` feature, off by default, beside
  `selftest` and argued the same way; the guard is a `cfg!` early return (measured with rustc: the
  format string is dropped from both debug and `-O` binaries, so a byte scan of the artifact reads
  the build rather than an optimiser's mood). Measured on one image with a 12 s idle `/init`: 20
  instrument lines with the feature, 0 without, and nothing else in the wire moved (93 vs 73 serial
  lines). The gate now checks it **per arm** - the kernel image must carry no `mpos: heartbeat`, and
  `cargo check --features heartbeat` must still compile, since no xtask command passes the feature
  and its body would otherwise rot - for ~5 s a boot. **Named residual**: the counters it prints are
  still bumped in hot paths when it is off (one relaxed store each, cost unmeasured). **Trap found
  and recorded**: `boot/build.rs` watches the kernel's mtime, so toggling a feature and rebuilding
  the loader can silently keep the *previous* kernel (the first counterfactual here measured the
  loud build twice, §7.9v) - rebuild the payload from scratch and check the wire. (§7.9v, item 266)

**A rename that replaces a name retires the replaced inode's watch, and a directory is told one move
and nothing else** (`docs/uapi/` §5.2, item 267, landed 2026-11-13): the inotify module's header had
carried a sentence nothing had measured - "a rename that replaces a name really does unlink the old
inode here, so the replaced name truthfully reports `IN_DELETE` before `IN_CREATE`". Measured on a
current host first (`.workbuddy/probe_inotify_rename.c`, Linux 7.2.14): `rename(source, victim)` over
an existing name tells the watched directory `IN_MOVED_FROM` + `IN_MOVED_TO` on one cookie and
**nothing else**, while the victim's own watch hears `IN_DELETE_SELF` then retires with `IN_IGNORED`
(`inotify_rm_watch` of it is `EINVAL`). This kernel's copy-based rename unlinks the victim through
`dir.unlink` and reported nothing, so the watch stayed in the registry for ever keyed on an inode no
name reached - a watcher of a file an install replaced was never told its file had died. The fix is
the missing report: `renameat2` says `notify_deleted` for the `existing` inode it already looked up.
Pin: `t-inotify` 8 new claims (67 → 75) on both machines, whose leak detector is `rm_watch` of the
retired wd answering `EINVAL`; the counterfactual reddens exactly that pair and nothing else. Named
departures stay `IN_UNMOUNT`, a watched directory's `IN_MOVE_SELF`, and - structural, not bookkeeping
- a watched regular file not surviving a rename (the copy is a fresh inode). (§15.101, item 267)
- **A dead task's memory files refuse the *open*, and the open keeps what it took.** `/proc/<pid>/auxv`
  and `/proc/<pid>/environ` refused at the **read** and always with `ENOENT`, so a task that had just
  died looked like a number that never existed - and `environ` did not refuse at all: it served the
  dead task's 14 bytes of environment. Measured on a current host first
  (`build/scratch/zombiemm/{z,q,s,t}.c`, Linux 7.2.14): a zombie's `auxv`, `environ`,
  `smaps_rollup`, `pagemap` and `mem` refuse the **open** with `ESRCH(3)` where a collected number
  gets `ENOENT(2)`; a stranger's open of a root-owned zombie is `EACCES(13)`, because the permission
  word answers before the address space; `cmdline`/`maps`/`smaps`/`numa_maps` open and read nothing;
  and a descriptor opened while the task lived serves what the open took (`16 + 352 = 368` of 368,
  with no read before the kill) while a re*open* after the death is `ENOENT`. `crate::sched::TaskMemory`
  (`Bytes`/`NoMemory`/`NoTask`, in `sched/policy.rs` because `mod runtime` is bare-metal only) carries
  the split in one table walk, and a defaulted `Inode::open_check` is asked by the open handler
  **after** the permission word - not in `open_replacement`, which runs before it and would answer
  `ESRCH` to the stranger Linux answers `EACCES`. `ProcFile` gained a `capture` the open runs,
  distinct from `generate`, so a captured body is the descriptor's for its life; `smaps_rollup`
  renders text through the same gate (`memory_text_file`), and `cmdline`/`mapping_lines` answer
  empty for a dead task instead of a body. Pin `t-proc-dead` (17 claims) is green on both machines
  and under `MPOS_ON_HOST=1`; three counterfactuals redden exactly the claims they remove, and a
  host unit test pins the two refusals and the capture. Named leftovers at
  the time: `pagemap`/`mem`/`stack` had no node here (the path itself was `ENOENT`, so Linux's
  `ESRCH` was moot) - **item 269** built `pagemap` (§6.150) and **item 270** built `mem`
  (§6.152), `stack` still has none - and
  the `X` state shares the zombie's predicate without its own measurement. (§6.149, item 268)
- **`/proc/<pid>/pagemap` (item 269).** The file had no node at all while §6.149 and the
  "Remaining ABI support" list both recorded it as a gap. Linux's shape measured first
  (`build/scratch/pmap/{p,q,s}.c`, 7.2.14): `0400`/size 0, byte-offset words with `EINVAL(22)` on a
  non-word offset or length, end-of-file past the top of the address space, `0x8180_…` for a
  touched private anon page, soft-dirty alone for an untouched page of the same mapping, the
  file-page bit for a read-faulted file page, `0x0` in no VMA, and the open's three refusals
  (`ESRCH` zombie / `ENOENT` collected / `EACCES` stranger, the ptrace check first). This kernel
  answers from the describing ranges plus a new one-page `sched::page_present` walk and **names**
  its three derived bits (PFN always 0 = unprivileged parity; soft-dirty always set, because
  nothing here clears a reference and there is no `clear_refs`; `exclusive` = present-and-not-shared
  because there is no mapcount, measured divergence `0xa080` vs the host's `0xa180` on a
  single-process `MAP_SHARED|MAP_ANONYMOUS` page). Pins: `t-pagemap` 17 claims on both machines and
  under `MPOS_ON_HOST=1`, two host unit tests, and two counterfactuals that redden exactly 3 and
  exactly 1 claim. The gate's first run also caught a **claim** whose sampling window could not see
  a timed park's wake (§6.151: 1 of 156 loaded-lane disagreements), fixed into a measured bound in
  the same round. Gate `build/gate-pagemap2.log` exit 0: stage 1 1317, x86_64 311, aarch64 310,
  kunit 35/0 both, lint/format/fuzz clean. (§6.150, item 269)
- **`/proc/<pid>/mem` (item 270).** The last memory file with no node. Linux measured first
  (`build/scratch/pmem/{m,n,o,p}.c`, 7.2.14): `0600`/`st_size 0`, the offset is the virtual
  address byte granular, `EIO(5)` in no mapping with `EINVAL(22)` for a negative offset, short
  transfers that stop at the first page they cannot serve, `FOLL_FORCE` serving a write into a
  `PROT_READ` or `PROT_NONE` page, `EBADF` for the wrong direction, and the item-268/269 ladder
  of `EACCES` before `ESRCH`/`ENOENT`. A plain physical copy is not enough for either half: the
  pager gained a `force` parameter that skips the *target's* permission bits (the ptrace
  question has already been answered), the cross-task reads go through a permissive walk so a
  `PROT_NONE` page can be read, and a write **splits the copy-on-write page first** with the
  tracee's own `Mm` books - a debugger poking a shared page must not write the frame the fork
  also maps. The node is `ProcessMem` in `src/vfs/procfs.rs`, sharing `address_facts` with
  `pagemap` so the two classifications cannot drift. Pin `t-proc-mem` (17 claims; the stranger
  and the dropped-uid pair are guest-only) green both arms, four counterfactuals each reddening
  exactly its claims, and two claims rewritten because the first versions measured nothing (a
  grandchild that looked before the poke; a `describes_task` a stranger could never reach).
  Gate `build/gate-procmem2.log` exit 0: stage 1 host tests 0 failed, both arms `fail=0`,
  `mpos-kunit 35/0`, lint/fmt/fuzz clean (the round's first sweep reddened two sampling
  claims instead - §6.151's bound and §6.153's one-tick lag - and both were re-derived from
  their own readings before this one). Named leftovers: the descriptor holds no `mm`,
  `stack` has no node, no swap or huge-page bits exist. (§6.152, item 270)

- **`mmap` of a node with no pages is refused at the call, with Linux's `ENODEV` (item 271,
  §5.6).** A user's `mkfontscale` died in the guest with `SIGSEGV` inside a mapping
  `/proc/<pid>/maps` described as `object(private)`: `mmap` of a **directory** (its walk maps
  every entry that opens) had been accepted, the fault path asked the directory for bytes,
  `read_at` answered `EISDIR`, and the frame was refused - so the program was killed on the
  first touch of a mapping the call had said was fine. Linux measured first: directory, FIFO,
  socket, `/dev/null` and every `procfs`/`sysfs` file are `-1 ENODEV`, a regular file maps; and
  `len 0`/`offset 1` are `EINVAL`, `fd -1` is `EBADF`, all before the node question. The fix is
  a defaulted `Inode::mappable()` - Linux's `f_op->mmap`, the default **false** so a node that
  cannot serve pages is refused rather than accepted - answered `true` by `RamFile`,
  `Ext4Ino` (a regular file's inode, never a directory's) and `P9Inode`, with the
  `frame_backed` arm still first so memfd/framebuffer/dma-buf are untouched. `t-mmap-refuse`
  (14 claims) green on the host and both arms; the counterfactual reproduces the user's report
  exactly (`t-mmap-refuse[10]: segfault … === t-mmap-refuse exit=139`). Gate
  `build/gate-mmaprefuse.log` `GATE_EXIT=0`, stage 1 1320, x86_64 313, aarch64 312, kunit 35/0.
  Named leftover then: `/dev/zero` still answered `ENODEV` where Linux maps it — **closed by item
  285** (§5.7). (§5.6, item 271)

- `execve` loads an image in bounded pieces (item 272, §6.154). The guest's `chromium` failed with
  `exec: line 36: /usr/lib/chromium/chromium: No space left on device` for a **294 MiB** binary on a
  root filesystem with **549 MB free**: the number was the frame allocator's (`MAX_ORDER = 14` caps
  one block at 64 MiB), the whole-file `read_file_opt` asked for 294 MB at once, and the failure was
  reported with the *disk's* errno. Now `mm::elf::parse` takes a `Source` and holds only headers plus
  the two small relocation tables (each capped); `map_image` reads each page through the source;
  `vfs::InodeSource` is the file as a source; `handle_execve` reads `BINPRM_BUF_SIZE` (256) bytes to
  decide what the file is, hands a script's prefix to the resolver, and an allocation the machine
  cannot serve is `ENOMEM` (`VfsError::NoMemory`). Pin `t-exec-big` (a 96 MiB padded `/bin/sh` copy
  runs, a 96 MiB non-program is `ENOEXEC`, a 96 MiB script runs); counterfactual reddens the three
  size claims with `ENOMEM` 12 - and with the errno lie back, `ENOSPC` 28, the report verbatim.
  Named: the *interpreter* is still read whole (`ld-musl` is small). (§6.154, item 272)

- **Item 273 — `execve` loads its image outside the table lock, and before it tears the old one
  down** (2026-11-14). `cargo xtask run` died after `MPOS_SWITCH_ROOT_OK`: item 272's page-by-page
  read happens inside `with_table`, and a page of a file on a block device **parks** the task
  (§10.5) while it holds the one lock the scheduler needs — a ramfs root never parks, which is why
  every test and initramfs boot was green. Building the space before the lock is also Linux's
  order for a failed `execve` (the caller keeps its image). Three boots with their last serial line
  are the counterfactual, the middle one naming the read and not the loader. Gate
  `build/gate-execorder2.log` exit 0: stage 1 1320, x86_64 `pass=314`, aarch64 `pass=313`, kunit
  35/0 per arm; the sweep before it reddened two aarch64 boot cases that bounded the console's own
  queued bytes at eight (`c4a6443`, §15.102). (§6.155, item 273).

- **Item 274 — `getresuid`/`getresgid` write four bytes, not eight** (2026-11-14). The user's
  chromium died in musl's `a_crash()` a few instructions after `getresgid`: this kernel stored each
  id through a `*mut u64`, and glibc's three `uid_t` locals are four bytes each, so every call
  overwrote the words above them. Linux's shape measured first (`build/scratch/resid/k.c`: canaries
  survive, a page-ending slot is written), then the four-byte store, then six `t-posix` claims -
  canaries for both calls and the page-ending slot - green on both arms and red in exactly those
  three with the old store back. `setsid` measured too and correct. Chromium now reaches its own
  `CHECK` instead of a corrupted stack; the missing dbus and the node-less
  `/proc/sys/fs/inotify/max_user_watches` are separate items. Gate `build/gate-resid2.log` exit 0:
  stage 1 1320, x86_64 `pass=314`, aarch64 `pass=313`, kunit 35/0 per arm (§6.156, item 274).

- **Item 275 — `/proc/sys/fs/inotify`: three ceilings that are policy, not constants** (2026-11-14).
  Chromium logged `Failed to read /proc/sys/fs/inotify/max_user_watches`; the numbers existed only as
  `const`s inside `src/vfs/inotify.rs` (`16384`/`128`/`8192`, Linux's defaults) with no door to read
  or raise them. Measured Linux first (`build/scratch/inoctl/r.c`: `0644` and `EACCES`-at-open, the
  three `EINVAL` shapes, `0` legal, and both ceilings biting), then the constants became
  `AtomicUsize`-backed policy read by the three enforcement sites, and `/proc/sys/fs` grew the
  directory with a shared `sysctl_count` parse. Pinned by 14 claims in `t-inotify-limits` (host 6,
  guest 14) with a four-red counterfactual; the ceilings are counted whole-system (Linux counts them
  per user, which is the same figure with one user) and the pin's claim says "at most two taken" for
  that reason. Gate `build/gate-inolimits2.log` exit 0: stage 1 1322, x86_64 `pass=315`, aarch64
  `pass=314`, kunit 35/0 per arm (§3.30, item 275).
- **Item 276 — the PI futex trio is served** (`FUTEX_LOCK_PI`/`UNLOCK_PI`/`TRYLOCK_PI` = 6/7/8): the word carries its owner's tid with `FUTEX_WAITERS`/`FUTEX_OWNER_DIED`, a contended trylock answers `EAGAIN`, a dead owner's word is handed on with `FUTEX_OWNER_DIED`, and — the reason it was built — musl's `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` no longer answers `ENOSYS`, which was the number that made pulseaudio abort chromium. Linux's answers measured first (`build/scratch/pifutex/probe.c`); pin `t-pi-futex` (22 claims, both arms) + three host unit tests; counterfactual refuses the arms again and reddens 17. Priority inheritance is *not* acted on (the ready queues are kernel-internal bands) and `WAIT_REQUEUE_PI`/`CMP_REQUEUE_PI` remain `ENOSYS` — both named. Gate `build/gate-pifutex.log` exit 0: stage 1 1325, x86_64 `pass=316`, aarch64 `pass=315`, kunit 35/0 per arm. (§6.157, item 276)
- **Item 277 — `/sys/devices/system/cpu` serves the four CPU lists** (`online`/`present`/`possible` = `0`, `offline` empty, all `0444`): crashpad read `online` and got `ENOENT`, so chromium's crash handler gave up before it could report anything; the values are this machine's one CPU (uniprocessor by design, the same fact `/proc/stat`'s `cpu0` line states), the shape is Linux's list. Pins: a host unit test over the built tree + `t-sysfs-cpu` (14 guest claims, including the `/proc/stat` agreement); counterfactual removes the class and reddens the unit test and 12 guest claims. No `cpuN` directories, no hotplug — named. Gate `build/gate-sysfscpu.log` exit 0: stage 1 1326, x86_64 `pass=317`, aarch64 `pass=316`, kunit 35/0 per arm. (§3.31, item 277)
- **Item 278 — a trap report names the trap it took**: six IDT entries (`#DE`,`#UD`,`#NP`,`#SS`,`#GP`,`#AC`) share `user_fault_or_die`, which reported the general-protection literal for all of them — so chromium's `0f 0b` (`ud2` in musl's `a_crash`) was logged as a protection fault, and two readings of that log chased a fault the machine never raised. The seam now takes the vector (`report_user_trap(vector, ip, sp, error)`) and `src/kernel_services.rs::trap_name` turns it into a word; the words are this kernel's own because `dmesg` is unreadable here without `CAP_SYSLOG`, which is said rather than glossed. Pin: the `kernel_services` boot suite (three cases — `#UD` is not `#GP`, the seven dispatched vectors pairwise distinct, an uninstalled vector still named); counterfactual restores the one-word table and reds two of the three (`30 passed, 8 failed` against the harness's own `32/6` baseline). (§6.158, item 278)
- **Item 279 — a synchronous x86_64 exception ends the process with its vector's own signal**: that same six-vector door sent `SIGSEGV` for everything, and the `#BP` gate was DPL 0, so a ring-3 `int3` raised `#GP(0x1a)` (whose index names vector 3 as the one it could not deliver). Measured Linux's `WTERMSIG` first (`int3`→`SIGTRAP` 5, `ud2`→`SIGILL` 4, div0→`SIGFPE` 8, `#AC`→`SIGBUS` 7, `#GP`/`#PF`→`SIGSEGV` 11); the map is host-compiled in `src/sched/signal.rs::signal_for_trap` behind a new `Kernel::signal_for_trap` seam, the breakpoint gate is Ring 3, and `enable_user_sse()` sets `CR0.AM` (QEMU TCG does not emulate alignment checking, so that claim is accelerator-tolerant and the host unit test pins the row). Pins: the map's host test + `t-trap-signal` (six forked children, `hlt`/null-store controls, `MPOS_ON_HOST=1` PASS), in `[aarch64] gaps` because that machine's `ESR`→signal decoding is a named gap; counterfactual reddens five claims and leaves the controls green. Trap-time handler delivery, aarch64's classes, and `siginfo_t`'s `si_addr`/`si_code` are named gaps. Gate `build/gate-trapsignal-final.log` exit 0: stage 1 1327, x86_64 `pass=318`, aarch64 `pass=316`, kunit 38/0 per arm. (§6.159, item 279)
- **Item 280 — a fault's `siginfo_t` carries `si_addr` and its own `si_code`**: three writers (x86_64's frame, aarch64's frame, `sigtimedwait`) spelled the union themselves, and every fault record was `SI_USER` with a pid where Linux puts the fault address. Measured Linux 7.2.14's 128-byte wire first (`build/scratch/siginfo/wire.c`, `faultcodes.c`: `SEGV_MAPERR 1`, `SEGV_ACCERR 2`, `ILL_ILLOPN 2`, `FPE_INTDIV 1`, `BUS_ADRALN 1`, `SI_KERNEL 0x80`, `si_addr` at +16). One pure `hal::SignalInfo::to_wire()` now renders every record on both machines; `SigInfo::fault` + the host-compiled `fault_record` map sit behind the new `Kernel::send_fault_signal` seam, and the x86_64 `#PF`/six-exception doors queue the record. `SEGV_MAPERR` and `CLD_EXITED` sharing code 1 is why the discriminator is the record's origin. Pins: three boot kunit wire-image cases (both machines) + two host unit tests; counterfactuals red both layers (2 host tests; `si_addr is 0x580000004d, not 0x10`), files restored byte-identically, guest field reading `H sig=11 code=1 addr=0x10`. Handler-at-trap delivery (2/16 reliable) and aarch64's ESR map stay named gaps. Gate `build/gate-siginfo.log` exit 0: stage 1 1329, x86_64 `pass=318`, aarch64 `pass=316`, kunit 41/0 per arm. (§6.160, item 280)
- **Item 281 — a handled `#PF` enters its handler at the trap**: `x86-interrupt` hid the GPRs on `page_fault`, so a queued `SIGSEGV` waited for a PIT tick and only 2/16 children ever saw their handler (`docs/notes/` §6.160's gap). The port's second hand-written stub (`page_fault_stub`) pushes a `TrapRegs` block, consumes the error code with `rep movsq`, and `rust_page_fault` calls `deliver_at_trap(frame)` at the fault; a `PROT_NONE` page repaired by the handler resumes the same instruction with every register intact. Measured Linux's 16/16 + `mprotect`/resume shape first (`build/scratch/pfdeliver/{rel,resume,resume8}.c`); pin `t-fault-at-trap` (8 forked children, live `r12` across the fault, `MPOS_ON_HOST=1` PASS), in `[aarch64] gaps`; counterfactual removing only the trap-time door → `good=2 died=6` `FAILED`, restored byte-identically. The other six exception vectors' GPR-preserving stubs and aarch64's ESR→signal map stay named gaps. Gate `build/gate-pfdeliver.log` exit 0: stage 1 1329, x86_64 `pass=319`, aarch64 `pass=316`, kunit 41/0 per arm. (§6.161, item 281)
- **Item 282 — a page fault's `si_code` asks the mapping, not the page table**: §6.160 derived `SEGV_ACCERR` from `PageFaultErrorCode::PROTECTION_VIOLATION`, which is a page-table fact, so a `PROT_NONE` read answered `SEGV_MAPERR` where Linux answers `SEGV_ACCERR` — the branch a handler takes to decide whether there is anything to repair (measured on Linux 7.2.14 across four fault classes: `PROT_NONE` read, `PROT_READ` write, NX fetch and read-only file write are all 2; never-mapped and mid-hole are 1). `send_fault_signal` lost its `protection: bool` (the machine layer cannot answer the question) and the kernel now asks `sched::vma_prot_at(addr).is_some()`, gated on `sched::in_task()` because that lookup panics with no tenant and the boot's own EL0 check faults without one. Pin `t-fault-sicode` (nine claims, `MAP_SHARED` page so a forked handler's finding reaches the parent, `MPOS_ON_HOST=1` PASS), in `[aarch64] gaps`; counterfactual both ways — `mapped = false` and a kernel rebuilt from HEAD each redden exactly the two `SEGV_ACCERR` claims, and the HEAD run showed the old rule got `PROT_READ`-write wrong too (an untouched anon page has no leaf at all). Gate `build/gate-sicode-v3.log` exit 0: stage 1 1330, x86_64 `pass=320`, aarch64 `pass=316`, kunit 41/0 per arm. The same gate's first run also found `t-aslr` drawing two samples from 2048 pages and calling a 1/2048 coincidence a proof, now six samples (§6.163). (§6.162, item 282)
- **Item 283 — the seven process-visible exception vectors enter a handler at the trap**: `#DE`/`#BP`/`#UD`/`#NP`/`#SS`/`#GP`/`#AC` were `x86-interrupt` handlers, which hide the general registers, so a queued `SIGTRAP`/`SIGILL`/`SIGFPE` waited for a PIT tick — and `fault_repeats` usually killed the task first (`div0` died 8/8 with its handler never entered). Measured Linux 7.2.14 first: `int3` delivers `SIGTRAP` with the saved `rip` *after* the instruction and `si_addr` 0; `ud2`/`idiv 0` deliver `SIGILL`/`SIGFPE` with the saved `rip` *of* it and `si_addr` = that `rip`. One shared `trap_common` body with seven tag-pushing entry labels now builds a `TrapRegs` and calls `rust_trap`, which queues the vector's record and then hands over through item 281's `deliver_at_trap`; the ring-0 halves report through `rust_trap_kernel` (named from the new `Kernel::trap_name` seam) and `rust_breakpoint_kernel` (the boot's own `int3` kunit still counts and returns). **The gate caught a real mistake**: the stub shifted three words on the ring-0 path, assuming a same-ring trap pushes no `SS:RSP`; the boot's clock suite died with `#GP(0x280)` (`0x280` = the saved `rflags` `0x283` masked) until a probe printed the raw block and showed all five words are pushed at every privilege level — one frame shape, one shift, recorded in the section. Pins: `t-trap-at-trap` (8 forked children per vector, `si_code`/`si_addr`/saved-rip/resume, `MPOS_ON_HOST=1` PASS), in `[aarch64] gaps`; `t-trap-signal`'s 17 claims still green; counterfactual restores the pre-item file → `good=1/0/0 of 8` and `FAILED`. Gate `build/gate-trapstub2.log` exit 0: stage 1 1330, x86_64 `pass=321`, aarch64 `pass=316`, kunit 41/0 per arm. (§6.164, item 283)
- **Item 284 — a fault's *class*, not a vector: aarch64 signs its EL0 faults and delivers them at the trap**: the second machine's `el0_sync` resolved what it could and then died on everything left with `die_of_fatal_signal(11)` — one hardcoded `SIGSEGV`, no `siginfo_t` record, no handler entry — and the seam could not have helped, because `Kernel::send_fault_signal` took an **x86_64 vector number** whose whole policy was `vector == 14`, so a class aarch64 names from `ESR_EL1` fell to `fault_record`'s `_` arm. The fix is a vocabulary rather than a translation: `mpos_vm::FaultKind { Divide, Breakpoint, InvalidOpcode, Page, Alignment, Other }` lives in the leaf crate because both halves of the seam must name it (`sched::signal` is host-compiled; `arch` cannot be reached from a leaf) — the same reason `MapFlags` is there — so the machine answers the *class* and the kernel keeps the ABI table. `el0_sync`'s three dying arms now queue the record and hand it over through §2.13's `deliver_at_trap` door via a new `deliver_or_die`, so a `SIGSEGV` handler runs **at the trap** on that machine: `EC_DATA_ABORT_EL0`/`EC_INSTR_ABORT_EL0` → SIGSEGV with `SEGV_ACCERR`/`SEGV_MAPERR` from the same `vma_prot_at` question x86_64 asks, `si_addr = FAR_EL1`; `EC_SYS64`'s unhandled register → SIGILL/`ILL_ILLOPN` with `si_addr` the instruction rather than the stale `FAR_EL1` a copy would have invented; anything else → `Other`. Pin `t-fault-sicode` lost all 19 `#[cfg(target_arch = "x86_64")]` items and now runs on both machines — and its **host leg runs on Linux for both instruction sets** (`cargo build --release --target aarch64-unknown-linux-musl` needs no gcc, only `rust-lld`; then `qemu-aarch64`), so the four claims are Linux's answers on x86_64 *and* aarch64. Counterfactual: the pre-item aarch64 file restored → `Error: self-test failed on: aarch64` with `FAIL: … the handler ran (not a default death)` on all four classes, i.e. **no handler ran at all**, then restored byte-identically and green again. Gate `build/gate-item284.log` exit 0: stage 1 1330, x86_64 `pass=321`, aarch64 `pass=317` (one more than the previous green — the newly-open pin), kunit 41/0 per arm. `t-fault-at-trap`/`t-trap-at-trap` stay in `[aarch64] gaps` by design (their subjects are the first machine's vectors; what aarch64 gained is the delivery, not the entry), `fault_repeats` still absent there, and `signalfd`/`PTRACE_GETSIGINFO` still drop `si_addr` — named. (§6.165, item 284)

- **Item 285 — `/dev/zero` maps, and the descriptor's access mode is judged first**: `§5.6` left
  `/dev/zero` answering `ENODEV` where Linux maps it, calling the prerequisite "a zero-page mapping
  operation". Measured first, the prerequisite turned out not to exist: what `/dev/zero` hands a
  mapping is **fresh private anonymous memory** (`build/scratch/zeromap/{z,w,v,mat,n}.c` on Linux
  7.2.14 — two `MAP_SHARED` mappings of one page are **not** aliased, no extent to run past at
  offset a thousand pages, `smaps` says `Anonymous: 4 kB` privately and `0 kB` shared, `maps` prints
  `/dev/zero` privately and `/dev/zero (deleted)` shared), which is the arm `MAP_ANONYMOUS` already
  takes. So the fix is a **third `Inode` question** beside `mappable()` and `frame_backed()` —
  `maps_as_anonymous()`, defaulted false, carried on `CharDevice` (Linux asks the driver through
  `f_op->mmap`), forwarded by `DevNode`, answered `true` by `ZeroDev` — routing that node into
  `map_anon_named` privately and the unnamed `Memfd` object the shared-anonymous arm builds.
  Deliberately not a `VmaKind` variant (the memory *is* `Anon`; a variant would touch 24 match
  sites). The same measurements found **the door had no descriptor-access check at all**:
  `O_WRONLY` is now `EACCES` for every prot including `PROT_NONE`, and a write prot on a
  non-writable descriptor under `MAP_SHARED` is `EACCES` while the same request privately is
  served — in Linux's measured order, `EINVAL` → descriptor access → the node's answer. Pin
  `t-dev-zero-mmap` (17 claims, host 17/17 + guest 17/17), with `/dev/null` still `ENODEV` both ways
  as the half a "map every char device" fix would break; counterfactual stashes the kernel half and
  reddens **all fifteen** `/dev/zero` claims (every one errno 19, the `EACCES` rows included) while
  the `/dev/null` pair stays green. The first gate was red on a **pre-existing pin**: `t-fbdev`
  opened `/dev/fb0` `O_RDONLY` and mapped it writable, which the host's own card refuses as root
  (`RDONLY + SHARED|RW = EACCES`) — fixed to `O_RDWR`, what a compositor does. Gate
  `build/gate-devzero2.log` `GATE_EXIT=0`: stage 1 1331, x86_64 322, aarch64 318, kunit 41/0. (§5.7,
  item 285)

- **Item 286 — a timer's `siginfo_t` carries its handle and its miss count**: the
  `SI_TIMER` arm is `{ si_timerid; si_overrun; sigval }` and this kernel wrote only
  the payload — every timer record delivered `si_timerid` 0 and `si_overrun` 0, and
  `signalfd` left the flat struct's `ssi_tid` (byte 24), `ssi_overrun` (32) and
  `ssi_int` (44) at zero, so even a `sigqueue` payload read as 0 through the `int`
  view. Measured first on Linux 7.3.4, for `SIGALRM` and `SIGRTMIN+3` alike
  (`build/scratch/sfdhost/`): the handler words are `[0..6] = 37 0 -2 0 1 0`, so the
  handle is byte 16 and the count byte 20 while byte 12 is padding; the flat
  `signalfd_siginfo` puts them elsewhere (24/32/44/48/72) — and the payload is one
  field seen twice (`ssi_int -559038737` beside `ssi_ptr 0x77441122deadbeef`).
  **`si_overrun` is live and `timer_getoverrun` is not the same number**: a 20 ms
  timer left unread for 500 ms delivered a record reading 24 while the door answered
  **0** at four asks along the way and 24 only after the read — Linux's `it_overrun`
  against `it_overrun_last`. `setitimer`'s expiry is `SI_KERNEL` (128), not
  `SI_TIMER`; a one-shot's count is 0; one pending record per timer, ever. So
  `SigInfo` gained `tid`/`overrun`, `SignalInfo::to_wire` chose the union arm by the
  record's class, `vfs::signalfd`'s writer became one host-testable `write_record`,
  `PosixTimer` split into `overrun` (the record's) and `last_overrun` (the door's),
  and the expiry scan's bump reached **both** classes — its first version skipped
  real-time signals, which is exactly the class musl's `SIGEV_THREAD` helper arms
  its timer with. Pin `t-sfd-timer-record` (54 claims, both machines) plus three host tests
  (host lib 818); counterfactual stashes the kernel half and the guest reddens **9**
  claims while the pin's restored tree answers `=== PASS`. The **guest** found two
  further defects the host could not: the timer arm of `to_wire` never wrote the
  `sigval` at byte 24 (a handler got a zero payload the signalfd door carried), and
  `last_overrun` was reset by the next expiry rather than at delivery. Gate
  `build/gate-item286b.log`: stage 1 1336, x86_64 `pass=323`, aarch64 `pass=319`
  (each one more than the previous green), kunit 41/0 per arm, clippy ratchet
  unchanged; the aarch64 arm ran separately because this sandbox's `/var/tmp`
  turned read-only and QEMU hardcodes it as the snapshot overlay's directory. Named: a deleted timer's
  queued record is still kept (a pre-existing, separately pinned divergence),
  `CLOCK_THREAD_CPUTIME_ID` timers stay `EOPNOTSUPP`, and the counters are reported
  nowhere else. (§6.166, item 286)

- **Item 287 — a deleted timer's record is freed, and the pending bit is left standing**:
  §6.166 closed with this as a named divergence — "this kernel keeps the record, Linux
  frees the `sigqueue` and a take answers `EAGAIN`". Measuring it showed the sentence was
  wrong in both directions. Linux frees the record and **leaves the pending bit set**
  (`pending=1` before and after the delete); a take then answers `EAGAIN`; a *blocking*
  `sigtimedwait` parks its whole timeout and only then answers `EAGAIN`; that failed
  dequeue is what drops the bit; unblocking runs no handler and does not end the task;
  a `signalfd` read is `EAGAIN`; a later `kill` of the same number is delivered normally;
  and deleting an unfired timer touches nothing (`build/scratch/timerdel/{del,del2,del3,
  del4}.c`, Linux 7.3.4, both signal classes). This kernel did worse than "keeps the
  record": the handler **ran** for a timer that no longer existed, a `signalfd` handed
  back a full record naming it, and a task blocked in a syscall **died** of the number —
  and the pinned claim called that a divergence by design, which is the shape §3 rule 2
  forbids. The fix is a marker **on the record** (`SigInfo::freed`, keyed by the timer's
  handle so a second timer's record on the same number is not marked by mistake) plus one
  `record_is_freed(task, sig)` predicate asked by all three doors that could hand the
  signal over; `RtQueue::holds_freed` answers the real-time half. The fatal door was the
  third one found, by the guest dying of signal 38 in the probe's own cleanup. Pin
  `t-timer-create`'s divergence claim became three claims of Linux's answer with no
  `host_run` branch (green on both arms) plus host lib 819; the counterfactual reddens
  exactly `so a take of that number answers EAGAIN, not a record (got Ok(14))` while the
  other two stay green. Gate `build/gate-item287-x86.log` exit 0: stage 1 1337, x86_64
  `pass=323`, aarch64 `pass=319`, kunit 41/0 per arm, clippy ratchet unchanged. Named: two timers on one signal each deleted with a record queued
  leave one marker (the model's limit, not measured against Linux), the stale bit's window
  duration is unpinned, and a `SIGEV_THREAD_ID` expiry's marker lands on the named
  thread's record. (§6.167, item 287)

- **Item 288 — a `CLOCK_THREAD_CPUTIME_ID` timer counts its creating thread, and is
  disarmed when that thread ends**: §6.166's gap 2 named the refusal with a reason that
  had never been measured — "the clock belongs to the creating thread while the timer
  hangs off the group and outlives it; Linux freezes the clock when the thread dies, and
  this table cannot express a frozen reading". The ownership half was right; the
  mechanism half was **wrong**. Measured on Linux 7.3.4 (`build/scratch/thrcpu/
  {t,t2,t3,t4,disarm}.c`): the clock counts the **creating thread's** CPU (a creator
  burning ~60 ms fired a 30 ms repeating timer once, and the process's own `RUSAGE_SELF`
  is irrelevant to it); it **stops** when that thread exits (300 ms of *main-thread* CPU
  produced 0 further expiries); and Linux **disarms** the timer rather than freezing a
  reading — after `pthread_join`, `timer_gettime` succeeds with value *and* interval 0,
  `timer_getoverrun` is 0, `timer_settime` answers **`ESRCH`**, and `timer_delete` still
  answers 0. So `TimerClock` gained a `Thread` arm carrying the creating thread's slot
  **and generation** (a recycled slot must not read a stranger's CPU — the §5 trap this
  repository has paid for with `setpriority` and pidfds), an optional latched final
  reading; the rule itself is the `abi` leaf's pure `thread_clock_reading(live,
  latched)`, host-testable because the decisive case needs a thread to die.
  `latch_thread_clocks` runs from `exit` before the task is marked gone and disarms the
  timers whose clock it was; `timer_arm` answers a `TimerArm` enum so `EINVAL` and
  `ESRCH` stay distinct. Pin `t-timer-cpuclock` grew eight claims and **lost its
  `host_run` branch entirely** (every one is Linux's answer on both arms); host leaf test
  pins the four reading cases, including the one that must not be spelled as a zero.
  Counterfactual reddens exactly the refusal (`code: 95`), the rest gated behind it.
  Named: Linux's own reading for a 60 s timer whose creator died is `0.000000000` where
  this kernel reads `9.728 s` (an arithmetic artefact of the frozen counter — the pin
  asserts the agreement, not the artefact), and a `SIGEV_THREAD_ID` expiry aimed at a
  thread other than the clock's is separated but not yet exercised. Gate: stage 1 1338,
  x86_64 `pass=323`, aarch64 `pass=319`, kunit 41/0 per arm, clippy ratchet unchanged
  (`build/gate-item288.log`, `build/gate-item288-arm3.log`). (§6.168, item 288)

- **Item 289 — the clock's thread and the signal's thread are two roles**: §6.168
  separated them in code and named the gap in its own list — a
  `CLOCK_THREAD_CPUTIME_ID` timer's **clock** is the creating thread's CPU while its
  `SIGEV_THREAD_ID` **target** is the thread the caller named, and no fixture exercised
  a pair that differs, so "the record goes to the named thread" was a reading of the
  code. Measured on Linux 7.3.4 (`build/scratch/thrsig/t.c`, three threads — target
  blocks the signal and owns a signalfd but burns no CPU, creator owns the clock and
  burns ~120 ms, main owns neither): `timer_create` accepts the pair; the expiry is
  driven by the **clock's** thread; all 28 records land on the **named** thread with
  **0** left on main (which also had a signalfd open, so a misrouted record would have
  been seen); and when the clock's thread exits the clock still **ends** — `gettime`
  0/0, `settime` `ESRCH` — though the target lives. The guest agrees row for row (91
  records rather than 28 — the same burn is 499 ms on the host and 1635 ms under TCG,
  so the count is the machine's and the claims are about routing and the end, never
  the count: the error §6.169 records for a drain length). **No kernel code moved** —
  `TimerNotify::Thread`'s routing arm was already Linux's shape; what landed is the
  measurement and five claims in `t-timer-cpuclock`, which is the whole of the item,
  and a gap closed by measurement should not be dressed as a behaviour change. Also
  measured, at the cost of two failed drafts: `sigevent`'s thread id is the union at
  byte **16**, not the `sigval` at 0, and aiming through the wrong word is `EINVAL`.
  Counterfactual routes a thread-targeted expiry to the group's **leader** and reddens
  exactly `FAIL: the record goes to the named thread (0 records on tid 12)`. Named: the
  mirror pair (target dies, clock lives) is derived rather than measured, the tid's pid
  namespace is not exercised, and a thread-targeted expiry's `si_overrun` was not
  asked. Gate `build/gate-item289.log` exit 0, **both arms in one run**: stage 1
  1338, x86_64 `pass=323`, aarch64 `pass=319`, kunit 41/0 per arm, clippy ratchet
  unchanged. (§6.170, item 289)

- **Item 290 — `si_code` names the door that raised the signal**: every sender door wrote
  `SI_USER` (0), so a receiver could not tell `tgkill` from `kill` — and `libc`'s `raise`
  **is** `tgkill`, which makes the wrong code reachable from a plain C program.
  `SI_TKILL` (-6) was already defined in `src/sched/signal.rs` (with its own measured note
  about `rt_sigqueueinfo` refusing it) and never used as a *sender's* code. Measured on
  Linux 7.3.4 (`build/scratch/sfdaddr2/t4.c`, one blocked signal read through a `signalfd`
  per door): `raise` -6, `kill(self)` 0, `tgkill(self)` -6, `tkill(self)` -6, `sigqueue` -1.
  One new door `send_signal_tkill` builds the record with `SI_TKILL`; `handle_tkill` and
  `handle_tgkill` call it and `kill` keeps `SI_USER`, with the permission, namespace and
  gone-leader rules untouched. `t-signalfd` pins the **pair** (`raise` → `SI_TKILL`, `kill`
  → `SI_USER`), because a kernel answering `SI_TKILL` for everything would pass one and fail
  the other; counterfactual reddens exactly `raise's record is SI_TKILL … got 0` with the
  control green. **The same probe corrected a false gap**: §6.166 gap 3 claimed
  `signalfd_siginfo.ssi_addr` "still is not written", but a record reaches a descriptor only
  for a blocked signal and a real CPU fault **kills even with its signal blocked**
  (SIGSEGV/SIGBUS/SIGFPE/SIGILL all `WIFSIGNALED`), so no fault record can arrive and every
  reachable class carries `ssi_addr` 0 across seven signal numbers — dropping `SigInfo::addr`
  is honest, and the gap list is corrected in place rather than quietly amended. Named:
  `pidfd_send_signal`/`rt_sigqueueinfo` keep the caller's code verbatim (unchanged), and only
  the self-directed doors were re-read. Gate `build/gate-item290.log` `GATE_EXIT=0`,
  both arms in one run: stage 1 1338, x86_64 `pass=323`, aarch64 `pass=319`, kunit
  41/0 per arm, clippy ratchet unchanged. (§6.171, item 290)

- **Item 291 — `UFFDIO_COPY` is one page, and the span it accepted was a user-steered
  allocation**: found by scanning the syscall layer for an allocation whose *size* is the
  caller's, chasing §8.0's remaining allocator gap. Most candidates were properly capped
  (`keys.rs`'s `MAX_PAYLOAD`, `setgroups`' `NGROUPS_MAX`, `semop`'s `SEMOPM`, `msgrcv`'s
  queue budget); `UFFDIO_COPY` was not, and it was a wrong answer with a free allocation
  attached. Its bound was "any whole number of pages", so three things disagreed: the
  handler allocated `vec![0u8; len]` from the caller's word and copied `len` out of the
  caller's buffer, `fill_page` installs **one** frame, and the answer word was
  `copy = len`. A caller that registered a 16-page span, faulted its first page and asked
  for a 16-page copy got one page installed and `copy = 16384` reported — and the kernel
  allocated 64 KiB at a number one `UFFDIO_REGISTER` makes arbitrarily large. The bound is
  now `len == page` (a fault is one address; `UFFDIO_ZEROPAGE` takes the same `len`
  through the same check; `fill_page` writes one frame), with the handler still allocating
  but only after the bound refuses the bad argument. **Measured**: inherited from §6.33,
  which took the call as **real root** on Linux 7.2.11 and records
  `0 with copy = 4096 and the page really installed`, `len` of 0/1 `EINVAL` — and
  re-confirmed that this sandbox cannot reach the call either way (`EPERM` for uid 1000
  with `unprivileged_userfaultfd=0`, and for uid 0 inside `unshare -Ur`), so the notes say
  the shape is inherited and the counterfactual is the proof. Pin `t-userfaultfd` grew
  three claims beside the `copy == PAGE` assertion it already made — the gap that let this
  live was checking only the positive answer, never the bound — and the host `copy_len_ok`
  case gained `2*page`/`16*page`/larger-page pairs. Counterfactual reddens **three**
  claims with **three different** wrong answers (`EEXIST` for 2 pages, `ENOENT` for 4 and
  16), which says the old code reached three paths on one malformed argument. Named:
  §8.0.2 **stays open** (one instance of the shape, not the cause), the scan was not
  exhaustive (`vfs`/drivers read only where a syscall hands them a length), and
  `UFFDIO_ZEROPAGE` tightened with the shared check without its own multi-page
  measurement. Gate `build/gate-item291.log` `GATE_EXIT=0`, both arms in one run:
  stage 1 1338, x86_64 `pass=323`, aarch64 `pass=319`, kunit 41/0 per arm, clippy
  ratchet unchanged. (§6.172, item 291)

- **Item 292 — a symlink target is `PATH_MAX - 1`, not one `NAME_MAX` component, and
  its inode's `size` was an allocation**: the inventory §6.172 said was owed — a hand
  walk of the syscall-facing layers for a length the caller *or the medium* supplies
  that reaches an allocation. The syscall layer came out clean (`MAX_PAYLOAD`,
  `NGROUPS_MAX`, `SEMOPM`, the msg-queue budget, `WAITV_MAX`, `collect_capacity`);
  what the walk found was in `vfs`. Two defects sharing one number, in opposite
  directions. (1) `symlink` read its target through `cstr_arg`, whose limit is
  `MAX_PATH` = 255 — the cap for one path **component**, right for the link's own name
  and wrong for the target, which is a whole path — so a link a real filesystem holds
  was refused by the call that makes it. (2) `Ext4Ino::readlink` allocated from
  `data.size`, a field **in the inode on disk**, on both branches — so a corrupt or
  hostile image named any length it liked, §8.0.2's catalogue exactly. Measured on
  Linux 7.3.4 (`build/scratch/symcap/t.c`): `symlink(4095)` succeeds and reads back
  4095, while 4096 and 4097 are `ENAMETOOLONG` — so the bound is `PATH_MAX - 1`, and
  the creation door refusing past it is what makes the same number safe to bound a
  read against. `mpos_abi::MAX_SYMLINK_TARGET_BYTES` is the leaf constant (both layers
  need it, neither may name the other), **both** `symlink` and `symlinkat` read the
  target through `cstr_limit(.., MAX_SYMLINK_TARGET_BYTES)` while keeping 255 for the
  link's name, and `readlink` refuses a longer on-disk `size` as `Invalid`. **A third
  defect surfaced through the gate and through only one arm**: the first version fixed
  `symlink` alone, the gate reddened only on aarch64 at the 4095 claim on a genuinely
  rebuilt, ungated path, and the missing half was `symlinkat` — same bug, untouched.
  glibc's `symlink()` goes through `symlinkat` and musl's issues `symlink` directly, so
  the x86_64 arm (glibc, wrong door unused) was green and the aarch64 one (musl, wrong
  door used) failed, and a hand probe in an initramfs could not see it either way. `t-readlink` grew four
  claims (4095 accepted and reading back 4095; 4096 and 4097 `ENAMETOOLONG`) — the
  **pair** is the claim, since a `PATH_MAX` version accepts 4096 and a `MAX_PATH`
  version refuses 4095. Counterfactual reddens **exactly one**: the 4095 acceptance,
  with both refusals green — the sharpest shape yet, because the bug was a bound too
  *tight*. Also lowered the `unused_imports` clippy baseline 4 → 3 for
  `mpos-user-tests`, which the ratchet demanded. Named: the **other paired doors were not audited**
  (`mknod`/`mknodat`, `chmod`/`fchmodat`, `utimes`/`utimensat` are the same shape, and
  the gate can only redden for the half a given arm's libc uses), the inventory is a
  hand walk and not a proof, other filesystems' `readlink` hold targets the kernel
  wrote (a reading, not a measurement), and the link's own name limit was not
  re-measured. Gate `build/gate-item292b.log` `GATE_EXIT=0`, both arms in one run
  after the `symlinkat` half: stage 1 1339, x86_64 `pass=323`, aarch64 `pass=319`,
  kunit 41/0 per arm, clippy ratchet unchanged. (§6.173, item 292)

- **Item 293 — the paired-door audit: `mknod` kept a body beside `mknodat`, and the two
  had drifted**: the audit §6.173 named. Item 292 found its defect through a *pair* of
  doors that a per-arm gate alone could not indict, and closed by naming the rest — this
  is that walk, and the method matters more than the result: every legacy syscall is
  Linux's `*at` form with `AT_FDCWD`, so the safe implementation is the one the dispatch
  already uses for `unlink`/`mkdir`/`rename`/`utimes`/`link` — build the argument list
  and call the `at` handler. Per pair: does the legacy name **delegate** or keep its own
  body? Seven do, two did not (`chmod`, `mknod`), and one had drifted. `handle_mknod`
  kept a 114-line duplicate with its own path resolution, and **only the `at` form
  emitted the `IN_CREATE` event** — so a daemon watching a directory heard about a node
  made by `mknodat` and nothing about the same node made by `mknod`. Measured on Linux
  7.3.4 (`build/scratch/doorpair/t.c`): all four doors (`mknodat`, `mknod`, `symlinkat`,
  `symlink`) answer `mask=0x100` naming what was made — the event belongs to the
  *creation*, not the door; on this kernel `mknod` emitted nothing while both `symlink`
  doors were already fine, because item 292's fix and `symlinkat`'s went into one shared
  tail. The fix is a **deletion**: `nr::MKNOD` builds `mknodat`'s argument list, as the
  neighbouring arms do, and the duplicate body is gone — not "add the `emit_child` call",
  because two bodies for one rule is the shape and delegating is what makes a second
  drift unspellable. Pairs checked and fine: `readlink`/`readlinkat` share a tail;
  `chmod`/`fchmodat` are two bodies but identical; `access`/`faccessat` differ by design.
  Pin: `t-inotify` grew eight claims (four doors × made/heard) — and the pin found a
  second bug **in itself**, making its own `inotify_init1` when the host's
  `max_user_instances` was already spent by the earlier sections, so it now reuses the
  main queue. Counterfactual reddens **exactly** `FAIL: the watch hears mknod's entry as
  IN_CREATE`, the other three doors green — the sharpest shape a pair audit can have,
  because the claim is per door. A **third finding** came from the arm that would not
  compile: the pin spelled `SYS_mknod`/`SYS_symlink` unconditionally and aarch64 refused
  it (`E0425`) — arm64 has **no number for either**, which the kernel's own table states
  beside them (`nr_aarch64.rs`: "no arm64 number; unreachable on aarch64"), so the pair is
  claimed where both halves exist and the `at` forms alone where they do not. That
  complements §6.173's rule: a pair is not always a pair on every machine, and the pin
  must say which half it can see. Named: the walk covered the `fs` pairs and not
  `kill`/`tgkill`, `dup`/`dup3`, `poll`/`ppoll`, `select`/`pselect6` or
  `getrlimit`/`prlimit64` (distinct arguments, not the delegating shape); `chmod`'s two
  bodies still agree only by reading; and `IN_CREATE` is one event class of several.
  Gate `build/gate-item293b.log` `GATE_EXIT=0`, both arms in one run: stage 1 1339,
  x86_64 `pass=323`, aarch64 `pass=319`, kunit 41/0 per arm, clippy ratchet
  unchanged. (§6.174, item 293)

- **Item 294 — `dup3` accepted `O_CLOEXEC` and never installed it**: the argument-shaped
  half of §6.174's audit, and the same shape one level down — two doors offering one
  promise in different spellings, where both accepted it and one kept it.
  `handle_dup3`'s doc said "`dup2` with `O_CLOEXEC`", and its body validated the flag,
  dispatched `old == new` and unknown bits to `EINVAL`, then called `install_at` — which
  sets `flags[slot] = 0`, because clearing close-on-exec is exactly what **`dup2`** owes.
  `dup3` inherited the clearing and never undid it, so a caller asking for close-on-exec
  got a plain descriptor and leaked it across `execve` — nothing fails loudly, because a
  descriptor outliving an `execve` is a security property ordinary programs do not notice
  missing. Measured on Linux 7.3.4 (`build/scratch/dupclox/t.c`):
  `dup3(a,200,O_CLOEXEC)` → `F_GETFD = 0x1`; without the flag → `0x0`; `dup2` → `0x0`;
  this kernel answered `0x0` for the first. The fix sets the descriptor's flag **after**
  `install_at` (the only order that works, since the install must clear it for `dup2`) —
  and the first attempt stored `O_CLOEXEC` (`0x80000`, the *open* flag) instead of
  `FD_CLOEXEC` (`1`, the *descriptor* flag), which made `F_GETFD` answer a number no
  Linux returns; the measurement caught it, which is why the pin asserts the **value** and
  not its truthiness. Verified beyond the bit: a probe dups onto 300 with the flag and
  301 without, `execve`s a child that reports both, and the child reads **300 → `EBADF`**,
  **301 → open**. `t-dup3` grew three claims (the flag really set; its complement, `dup3`
  with no flags leaves it clear; and that complement is what makes the claim about `dup3`
  rather than about "the table has a bit") — the program had checked `O_CLOEXEC` was a
  *legal argument* and never that it *took effect*, the same gap §6.172 found in
  `UFFDIO_COPY`'s pin. Counterfactual reddens exactly `FAIL: and the flag is really set
  (F_GETFD = 0x0)`, the complement green. Named: `kill`/`tgkill`, `poll`/`ppoll` and
  `getrlimit`/`prlimit64` are still unaudited, `dup2`'s own clearing is measured but not
  pinned, and `F_DUPFD_CLOEXEC` is a third spelling not looked at. Gate
  `build/gate-item294.log` `GATE_EXIT=0`, both arms in one run: stage 1 1339,
  x86_64 `pass=323`, aarch64 `pass=319`, kunit 41/0 per arm, clippy ratchet
  unchanged. (§6.175, item 294)

- **Item 296 — the flag-to-getter sweep: `EFD_SEMAPHORE` was refused, and two claims were
  pinning comments**: the "flag accepted, effect never checked" gap had produced two
  defects in a row (§6.172's `UFFDIO_COPY`, §6.175's `dup3`), so instead of another pair
  this walks **every flag → getter pair** in one probe — `O_CLOEXEC`/`EFD_CLOEXEC`/
  `SFD_CLOEXEC`/`MFD_CLOEXEC` at `F_GETFD`, `O_NONBLOCK` and its three spellings at
  `F_GETFL`, `O_APPEND` by behaviour, `O_DIRECTORY` by refusal, `EFD_SEMAPHORE` by what a
  read returns. Twelve rows; all correct on Linux (`build/scratch/flagsw/t.c`), and on
  this kernel eleven did and **one did not print at all** because
  `eventfd(5, EFD_SEMAPHORE)` answered `EOPNOTSUPP` and the probe's guard skipped it. The
  refusal had been **pinned as a divergence** in `t-eventfd` with a `host_run` branch —
  the shape AGENTS.md §3 rule 2 forbids dressed as honesty, and it hid a real gap.
  Measured whole on Linux 7.3.4: a semaphore of 3 reads 1, 1, 1 then `EAGAIN`; a plain
  eventfd of 3 reads 3; a read with a 4-byte buffer is `EINVAL`; the mode composes with
  `NONBLOCK`; and a write past `0xfffffffffffffffe` is `EAGAIN`. **That last row
  corrected a second thing and is the more interesting find**: this kernel's `write_at`
  said "Saturating, as Linux's counter is" and a host test asserted two `u64::MAX` writes
  left `u64::MAX` "no wrap" — both wrong about Linux twice (the maximum is one below
  `u64::MAX`, and the overflow is *reported*), so **a comment and a test can agree with
  each other and both be wrong**, the test pinning the comment. `EventFd` gained the mode
  (`with_semaphore`/`is_semaphore`), `read_at` takes one and decrements, `write_at`
  refuses an overflow in both shapes, the door serves the flag, and `fdinfo`'s
  `eventfd-semaphore` word reports the mode. `t-eventfd`'s divergence claim became two
  claims of Linux's answer with **no `host_run` branch** (a semaphore of 3 reads
  `[1,1,1,EAGAIN]`; a plain eventfd of 3 reads the whole counter as the neighbour), the
  saturation host test was rewritten to the measured ceiling and a second host case added
  (host lib **820**). Counterfactual reddens exactly `FAIL: eventfd(3, EFD_SEMAPHORE) is
  served, got EOPNOTSUPP`, the neighbour green. Named: the sweep is one probe and not a
  suite (flags with no getter — `O_TRUNC`/`O_EXCL`/`O_NOFOLLOW` — need behavioural probes;
  `MFD_ALLOW_SEALING`'s "getter" is a refusal), the semaphore's **blocking** shape is
  untested, and `fdinfo`'s semaphore word has no pin. Gate `build/gate-item296.log`
  `GATE_EXIT=0`, both arms in one run: stage 1 1340, x86_64 `pass=323`, aarch64
  `pass=319`, kunit 41/0 per arm, clippy ratchet unchanged. (§6.176, item 296)

- **Item 297 — `O_PATH` was recorded and never enforced, so a descriptor that names was
  reading**: the third flag sweep, over the flags that have **no** getter. It split in
  two, and the first half came out **clean** — seven behavioural rows (`O_TRUNC` empties;
  `O_EXCL|O_CREAT` is `EEXIST` and does not truncate; `O_NOFOLLOW` on a link is `ELOOP`;
  `O_TRUNC|O_RDONLY` truncates despite the mode; `O_TRUNC` on a directory is `EISDIR`;
  `O_EXCL` without `O_CREAT` is ignored; `O_NOFOLLOW` on a regular file is ordinary)
  all answer the same here first try, which is recorded because a sweep that only
  reports its wins is one nobody can trust. The second half did not: `O_PATH` was
  **recorded** (so `F_GETFL` reported the bit, with a comment saying so) and acted on
  **nowhere** — `read(O_PATH)` answered `Ok(5)` where Linux answers `EBADF`, so a
  descriptor whose whole purpose is "name this, do not open it" read the file. Measured
  whole on Linux 7.3.4 (`build/scratch/opath/{t,combos}.c`): `read`/`write` `EBADF`;
  `fstat` works; `F_GETFL` carries the bit; `F_DUPFD` works; no `atime` movement;
  `O_PATH|O_NOFOLLOW` on a symlink is **served and is the *link***; `O_PATH` alone
  follows to the target; `O_DIRECTORY|O_NOFOLLOW` is **`ENOTDIR`**, the kind asked
  first. **Three defects**: enforcing `O_PATH` in `pre_read`/`pre_write`; then the pair
  — `O_NOFOLLOW`'s check was "a link is `ELOOP`" rather than "do not *traverse* one", so
  the one combination that exists to hand out a descriptor **on** a link was refused;
  then the order — the kind question comes before the link one. A trap inside the third
  fix: both flags are **machine-specific** (`O_DIRECTORY` is `0o20_0000` on x86_64 and
  `0o40000` on aarch64, `O_PATH`/`O_NOFOLLOW` moving with them), so they are named in
  `abi.rs` rather than spelled. New pin `t-open-path` (13 claims) in `t-fsflags`' shape
  — the four doors that answer *on* the descriptor, the `atime` non-effect, and the five
  `O_NOFOLLOW` combinations with their two neighbours. Counterfactual reddens **three**
  claims, each a distinct defect — the most informative red in this series. Two stale
  comments corrected with it: `xattr_inode_of_fd`'s "this kernel has no `O_PATH`" is now
  false, so the `EBADF` those three `f*` doors owe is a **named gap** rather than a
  vacuous one. Named: `O_NOCTTY` unswept (its effect is on the session), `O_PATH` as a
  **dirfd** not exercised (the flag's largest remaining use), `O_PATH`'s `EBADF` on the
  other fd-taking calls unpinned, and `O_SYNC` unobservable without a power cycle.
  Gate `build/gate-item297.log` `GATE_EXIT=0`, both arms in one run: stage 1 1340,
  x86_64 `pass=324`, aarch64 `pass=320` (each one more than 323/319 — the new pin is
  in both sweeps), kunit 41/0 per arm, clippy ratchet unchanged. (§6.177, item 297)

- **Item 298 — `O_PATH` as a dirfd, and two probes that measured the libc**: §6.177
  closed with this pairing as the flag's largest remaining use, and the walk is fourteen
  rows over Linux 7.3.4 (`build/scratch/opathfd/t.c`). **One real defect, and it was not
  about `O_PATH`**: `linkat(dirfd, "file", AT_FDCWD, "/abs/dest")` answered **`EXDEV`**
  where Linux links the file, because `linkat`'s filesystem comparison resolved the
  source with `absolute()`, which uses the **calling directory** — the right base only
  for `AT_FDCWD` — so a dirfd-relative source was resolved to a path that does not exist
  elsewhere and the comparison said "different filesystems". The tell was that the same
  call with both sides on the dirfd worked: one side went through the dirfd and one
  through `absolute()`, two rules where there should be one. The fix resolves the source
  through its own base and asks the **node**'s filesystem, the same question the
  descriptor branch already asks. **Two of the three reported mismatches were the
  libc**: `fchmod(O_PATH_fd)` "returned 0" against Linux's `EBADF`, but musl's `fchmod()`
  falls back to `fchmodat(AT_FDCWD, "/proc/self/fd/N")` and **that path is served on
  Linux too** — the same musl binary on Linux answers `0, mode 0600` while the raw
  syscall in both kernels refuses `EBADF`. The probe had compared glibc on one side with
  musl on the other. **Rule added: when a probe's two machines run different libcs, a
  disagreement is a claim about the libc until a raw-door probe says otherwise** — and
  the new claims use raw syscalls for every row where a libc has a fallback. The one
  refusal that was real is `getdents64` over an `O_PATH` descriptor, now refused through
  one `File::is_path_only()` predicate that `pre_read`/`pre_write` already computed
  inline and that `handle_fchmod`/`handle_getdents64` now ask. `t-open-path` grew four
  claims (17 total), raw-doored with the reason at the site. Counterfactual reddens both
  refusals (`got rc=0`, `got rc=48`). Named: the sweep covered twelve doors and not
  `name_to_handle_at` or the xattr `*at` family; `AT_EMPTY_PATH` with an `O_PATH` fd is
  unmeasured; the `EXDEV` fallback only places a node *at* a mountpoint; and
  `fchmod`/`getdents64` are the refusals claimed, not the set. **The first attempt at
  both fixes regressed `t-link` and `t-fsflags` on both arms** — the `linkat` fix asked
  `fs_serving_inode_of`, which places a node only **at** a mountpoint root, so a file
  inside a mount answered `None` and every ordinary `link(2)` became `EXDEV`; the
  working fix composes the source's path from the dirfd's own path and keeps the path
  comparison primary. Recorded because a helper answering a narrower question than the
  caller assumes is a silent wrong answer, not a compile error. Gate
  `build/gate-item298b.log` `GATE_EXIT=0`, both arms in one run: stage 1 1340, x86_64
  `pass=324`, aarch64 `pass=320`, kunit 41/0 per arm, clippy ratchet unchanged.
  (§6.178, item 298)

- **Item 299 — the rest of `O_PATH`'s refusals: seven doors served what nine must refuse**:
  §6.178's gap list said the predicate existed and "each is one line — but a line nobody
  has written is a gap, not a guarantee". This is the rest, and the sentence was right
  about the cost and wrong about the size: **seven more doors** needed it, two not merely
  lenient. Measured raw on Linux 7.3.4 (`build/scratch/opathrest/t.c`): nine refusals and
  two served. `fchown` → `EBADF` (this kernel **0, owner changed**); `fgetxattr`/
  `flistxattr` served; `fsetxattr` served and **wrote the attribute**; `fremovexattr`
  served and **removed it**; `ftruncate` → `EINVAL`; `fadvise64` → **0, advice "taken" on
  a file never opened**; `ioctl` → `ENOTTY`; `fstat`/`fchdir` served on both. **Why it
  mattered beyond the errno**: `fchown` and `fsetxattr` are *writes*, and an `O_PATH`
  descriptor exists so a caller may hold a **name** without holding the right to change
  the thing named — that is why `shm_open`-style and `/proc`-style doors use it — so the
  authority was the real half and the errno only the visible one. The two that refused
  "correctly" were right by accident: `ftruncate` judged its **length** before its
  descriptor, `ioctl` dispatched on the **request** first — the same shape as `linkat`'s
  source-base bug one item over, and the placement became the finding three times (one
  check for the four `f*xattr` doors, which already shared a helper; argument-order for
  `fchown`/`fadvise`/`ftruncate`; and the `ioctl` **entry point**, because a rule every
  one of a hundred doors must remember is a rule some door will forget). `t-open-path`
  grew eight more claims (25 total), all raw syscalls — musl's `fchown` routes through
  `/proc/self/fd` like its `fchmod`, so a wrapper claim would have measured the libc.
  Counterfactual reddens **seven** claims, each with its own wrong answer. Named: the
  read side (`readv`/`pread64`/`preadv`) is covered by construction not by a claim;
  `fcntl`'s `F_SETFL`/lock refusals were not re-read; `mmap` of an `O_PATH` fd unswept;
  and nothing pins the **authority** reading, only the errno. **The first gate run went
  red on x86_64**: the `ioctl` guard used `sched::current_fds()`, which panics on a
  `current` whose slot is momentarily empty, so the payload's own `sh` died of a `#GP`
  with `INIT_EXITED` before one program ran — fixed with a fallible `sched::running_fd`,
  the lesson being that a refusal which can end the machine is worse than the leniency it
  replaces. Gate `build/gate-item299b.log` `GATE_EXIT=0`, both arms in one run: stage 1
  1340, x86_64 `pass=324`, aarch64 `pass=320`, kunit 41/0 per arm, clippy ratchet
  unchanged. (§6.179, item 299)

- **Item 300 — `fcntl`'s own `O_PATH` split: the commands that ask are served, the ones
  that act refuse**: the family §6.179 left for last, whose gap list said "this kernel's
  `fcntl` was not re-read for those two refusals, so the half that is wrong could be in
  either direction". The shape of the answer is why it belongs to the `O_PATH` family
  rather than the lock family: **`fcntl` is a table, and `O_PATH` partitions it.**
  Measured raw on Linux 7.3.4 (`build/scratch/fcntl/t.c`): the commands that **ask what
  the descriptor is** (`F_GETFD`, `F_GETFL`, `F_SETFD`, `F_DUPFD`, `F_DUPFD_CLOEXEC`) are
  **served**, and the six that **act on the file behind it** (`F_SETFL` and the whole lock
  family) are **`EBADF`** — the served ones describe the *description*, the refused ones
  need the *open file* it does not have. This kernel was backwards in one place and absent
  in the other: `F_SETFL` → **0** (stored a status word on a descriptor with no file),
  `F_SETLK` → **0**, `F_GETLK` → **0**, `F_OFD_SETLK` → `EAGAIN` (answered from the lock
  table's own contention). Two checks, and **where they sit** is the whole of it:
  `F_SETFL` refuses inside its own arm before the `O_NOATIME`-ownership edge is computed
  (§6.179's `ftruncate` order), and the six lock commands are **one arm**, so one guard
  covers all of them — before `record_lock`, so the door's answer stays the table's.
  `t-open-path` grew five claims (30 total): the four refusals **and** the `F_GETFD`
  neighbour, because a kernel refusing every `fcntl` on an `O_PATH` descriptor would pass
  the refusals alone. Counterfactual reddens **four**, each with its own wrong answer —
  and the `EAGAIN` on `F_OFD_SETLK` is the one worth keeping, being the lock table's
  answer from a door that should not have reached it. **Named, and the distinction is the
  point**: `F_GETOWN`/`F_SETOWN` (with `F_GETPIPE_SZ`, the lease pair) answer `EINVAL`
  where Linux answers `0` because they are **unimplemented**, not misrefused — Linux
  serves them on an ordinary descriptor too, and serving them needs `f_owner` state this
  kernel does not keep; a door that answers `EINVAL` for a command it never implemented
  is a different defect from one that serves a command whose precondition is missing.
  Also named: the ordinary-descriptor half of `fcntl` is read but not pinned, and
  `F_SETLEASE`'s `CAP_LEASE` authority is unswept. Gate `build/gate-item300.log`
  `GATE_EXIT=0`, both arms in one run: stage 1 1340, x86_64 `pass=324`, aarch64
  `pass=320`, kunit 41/0 per arm, clippy ratchet unchanged — green first try.
  (§6.180, item 300)

- **Item 303 — `O_PATH`'s last nine doors: a descriptor that names could read, copy,
  flush and lock**: §6.180's largest named gap ("`mmap` of an `O_PATH` descriptor is
  `EBADF` on Linux and was not swept"), and one grep for the predicate sized it —
  `is_path_only` had twelve call sites, all path or `*at` doors, and **nine calls that
  read or write an inode through a descriptor** had none. The mechanism is structural:
  they do not pass through `File::pre_read`/`pre_write`, where §6.179 put the rule; each
  calls `Inode::read_at`/`write_at` (or the lock table) directly. A rule at a funnel is
  only a rule for what enters the funnel. Measured whole on Linux 7.3.4 (eight probes
  under `build/scratch/opathmmap/`): twelve doors refuse `O_PATH` with `EBADF` — `mmap`,
  `readahead`, `sync_file_range`, `fsync`, `fdatasync`, `sendfile`, `copy_file_range`,
  `splice`, `flock`, `fallocate`, plus §6.179's `ftruncate`/`fadvise64` — while `fstat`
  and `fchdir` are served. **And the measurement that makes it authority rather than
  tidiness**: `open(path, O_PATH)` asks **no permission on the file** — as uid 1000,
  `/etc/shadow` (`0600`, owner `65534`) refuses `O_RDONLY` with `EACCES` and **serves
  `O_PATH`**, whose descriptor then refuses `read`/`mmap`/`fsync` and answers `fstat` with
  the real `1295` bytes (Linux's `build_open_flags` sets `acc_mode = 0`, `MAY_OPEN`
  alone; the parent's search bit still applies). So the descriptor is a capability to a
  **name** the caller may not open, and this kernel served six contents doors on it:
  `readahead`/`sync_file_range`/`fsync`/`fdatasync` → **0**; `copy_file_range` → **16
  bytes copied**; `splice` → **3 bytes moved then hung**; `flock` → **0 and it held the
  lock** (a second `flock` on the same file got `EAGAIN`); `mmap` → **an address**; and
  `fallocate(O_PATH|O_WRONLY)` → **0 with the file grown from 10 bytes to 4096**. The
  second inversion explains the first: `F_GETFL` reported the whole word plus access bits
  (`0x208001` for `O_PATH|O_WRONLY`) where Linux reports the **fold**
  (`flags &= O_DIRECTORY|O_NOFOLLOW|O_PATH`, `acc_mode = 0` — so
  `O_PATH|O_APPEND|O_NONBLOCK|O_SYNC|O_TRUNC|O_NOFOLLOW` answers `0x220000` and truncates
  nothing, `O_PATH|O_CREAT` on a missing name is `ENOENT`, `O_CLOEXEC` is a descriptor
  flag), and storing the access bits where `may_write` reads them made an `O_PATH|O_WRONLY`
  open *writable* — which is how `fallocate` reached the file. Four more refusals were
  orderings, each measured: `copy_file_range` is fds→mode→flags→kind→len, `splice` is
  flags→len→fds→mode→pipe-kind, `sendfile` is fds→mode→count→dir-kind (its comment's "the
  count is asked first" was true only of the directory question), `fallocate` is
  fd→`O_PATH`→range→mode→mode-word — and two mattered, because `len == 0` short-circuited
  `copy_file_range` to `0` and `splice` to `0` before the `O_PATH` check could run. The
  fix is one predicate in the two primitives (`check_readable` refuses `O_PATH` as well as
  `ACC_WRONLY`; `may_write` refuses it), the open's `want = 0` and folded word, and nine
  call sites placed where the table measured. `t-open-path` **30 → 47 claims**, all raw;
  counterfactual stashes the three kernel files, rebuilds kernel **and** loader, and
  reddens **ten** claims each with its own wrong answer (`size 5 -> 4096` among them).
  Gate `build/gate-item303b.log` `GATE_EXIT=0`, both arms in one run: stage 1 1340,
  x86_64 `pass=324`, aarch64 `pass=320`, kunit 42/0 per arm, clippy ratchet unchanged at
  `162/168/389/272/3/1` — the first run reddened on clippy alone (`unused_mut` 3 → 4 from
  this change's own `let mut want`), fixed by deleting the `mut`. **Named**:
  `linkat(fd, "", …, AT_EMPTY_PATH)` is served on Linux and absent here (a missing door,
  not a wrong answer; `apk` reaches it through `/proc/self/fd/N`); `syncfs` is `ENOSYS`
  rather than misrefused; `fcntl`'s ordinary-descriptor half still unpinned and
  `F_GETOWN`/`F_SETOWN`/`F_GETPIPE_SZ` still unimplemented; and the open-side permission
  rule is measured but not pinned, because claiming it needs a payload at a second uid.
  (§6.182, item 303)

- **Item 304 — the open that asks the file's mode nothing, and the gap list that said it
  could not be pinned**: item 303's own fourth gap closed, and its stated reason was
  false. It read "the claim needs a file the guest's payload may not read — which the
  sweep's root payload cannot make without a second uid"; but `t-proc-access` and
  `t-affinity-rules` have `fork`ed to `nobody` for months and the id exists in both
  images. **A gap closed by naming a prerequisite nobody re-checked** — and it mattered
  more than the sentence suggests, because item 303's nine refusals hold *only because*
  the open asks nothing, while **the sweep's payload is root, where `CAP_DAC_OVERRIDE`
  answers every mode question "yes"** — so a claim made in the payload's own process
  would have stayed green through a regression of the fix's most important half. Measured
  on Linux 7.3.4: the owner's own `0000` file refuses `O_RDONLY` with `EACCES` and
  **serves `O_PATH`**; `/etc/shadow` (`0600`, owner `65534`) does the same for uid 1000;
  a file in a `0000` directory refuses both, so the parent's search bit still applies —
  and the served descriptor answers `EBADF` from `read`/`mmap`/`fsync` while `fstat`
  reports the real `1295` bytes. The pin is four claims and a child: `t-open-path` writes
  `moon` at `0000` and `sun` at `0444`, forks, and the child drops to `nobody` **before**
  opening anything, reporting `rd=<answer> path=<answer>` over a pipe. The claim is the
  **contrast** — `moon` refuses `O_RDONLY` with `EACCES` *and serves `O_PATH`* while `sun`
  is served both ways — so "refuse everything" cannot satisfy it, and a fifth claim keeps
  the fixture honest (the owner still opens `moon` as `O_PATH`, so the refusal is about
  the id and not an unopenable file). 47 → **52** claims. One draft of the claim failed
  **on Linux**: `text.contains("secret path=ok:")` cannot match a line reading
  `secret rd=errno:13 path=ok:3`, so the report is parsed by field now — a claim that
  fails on the correct kernel is worth as little as one that passes on the wrong one
  (§6.102's lesson in its own shape). Counterfactual: `want = if path_only { 0 }` put
  back to always computing the mode, kernel **and** loader rebuilt, pin booted in the
  guest — **exactly one claim red**, `and serves O_PATH on the same file
  ("secret rd=errno:13 path=errno:13")`, with all three neighbours green: the red *is*
  the regression, it is the only claim that moved, and a claim that fires on the right
  line and leaves its neighbours alone is what separates a pin from a signal. Gate
  `build/gate-item304.log` `GATE_EXIT=0`, both arms in one run: stage 1 1340, x86_64
  `pass=324`, aarch64 `pass=320`, kunit 42/0 per arm, clippy ratchet unchanged at
  `162/168/389/272/3/1`. **Named**: the `0000`-directory case is measured but not claimed
  (the fixture would need a directory the payload then has to clean up as `nobody`), and
  item 303's other three gaps stand — `linkat(AT_EMPTY_PATH)` missing, `syncfs` `ENOSYS`,
  `fcntl`'s ordinary half unpinned. (§6.183, item 304)

- **Item 305 — the five door-shaped gaps the `O_PATH` family left open, all in one
  pass**: item 303 named four, item 304 closed one of its own, leaving three of 303's
  — and **one of 303's four was stated wrongly**. `syncfs` was never `ENOSYS`: the
  dispatch arm has existed all along and only the `O_PATH` check was missing, and the
  door is not "regular files only" either — measured, `syncfs(pipe)` is **served**
  because Linux asks the file's superblock and a pipe's file has one. *A gap list is
  a claim about the code; "unimplemented" and "not swept" are different sentences and
  the note had conflated them.* `linkat(fd, "", …, AT_EMPTY_PATH)` — the only call
  that gives a file with **no name at all** one — landed for both descriptor shapes,
  with the four refusals (non-empty name `ENOTDIR`, no flag `ENOENT`, unknown bit
  `EINVAL`, bad fd `EBADF`) and `AT_SYMLINK_FOLLOW` riding along; the man page's
  `CAP_DAC_READ_SEARCH` is *not* observed on a file the caller owns, so the door asks
  what Linux's `vfs_link` asks. **The EXDEV check cost a round and the lesson is item
  298's**: `fs_serving_inode_of` matched both plain nodes to `root_fs` through two
  separately-built `Arc`s and every link returned `EXDEV`, so the comparison uses the
  descriptor's recorded `File::label` — a node inside a mount cannot name its
  filesystem, a path can — with the inode question kept (and *accepting*) for a
  descriptor that has no path, which is the `O_TMPFILE`/`memfd` shape the door exists
  for. `F_GETOWN`/`F_SETOWN` keep the SIGIO recipient in `File::f_owner`, the
  **description's** rather than the process's (a `dup` shares, a second open does
  not), storing what `F_GETOWN` **reports** rather than the argument — a negative
  group target, `0` and `-1` all read back **0**, a positive pid reads itself, and a
  number naming nothing is `ESRCH` whichever sign it carried. `F_GETPIPE_SZ`/
  `F_SETPIPE_SZ` read and set a capacity that moved out of a `const` onto
  `PipeState` (both ends and every `dup` share one number, Linux's
  `pipe_inode_info->max_usage`): default `65536`, below a page rounds up to `4096`,
  `4097` → `8192`, a shrink honoured, past 1 MiB `EPERM`, a non-pipe `EBADF` not
  `EINVAL`. And item 304's own `0000`-directory gap closed — the parent makes the
  directory `0000` **before** the fork so the child never changes a mode it does not
  own, which is what the recorded cleanup objection had imagined as impossible.
  **23 claims across three programs**, raw where the wrapper cannot express the claim;
  counterfactual reverts the five arms, rebuilds **kernel and loader**, and reddens
  **`syncfs` 1 / `linkat` 5 / `fcntl` 12** — each its own wrong answer, which is what
  says these are five pins and not one. Gate `build/gate-item305.log` `GATE_EXIT=0`,
  both arms in one run: stage 1 1340, x86_64 `pass=324`, aarch64 `pass=320`, kunit
  42/0 per arm, clippy ratchet with the host `dead_code` baseline **336 → 340** and
  the measurement on the constant — and **`unreachable_patterns` + `non_snake_case`
  appeared on the first run**, the hazard `abi.rs`'s own doc comment warns about: the
  four new `fcntl` command names were **not imported**, so each arm's pattern was a
  fresh binding matching every value. **Named**: `F_SETPIPE_SZ`'s shrink *below what
  is already buffered* is unmeasured (the capacity is documented as a ceiling on what
  may be queued), and the `F_GETLEASE`/`F_SETLEASE` + `CAP_LEASE` family is still
  unswept. (§6.184, item 305)

- **Item 306 — `O_PATH` as a dirfd, and the dirfd-relative link that compared two
  trees**: §6.179's named gap ("the flag's largest remaining use … the probe only
  used it as a *subject*"). Measured on Linux 7.3.4: **twelve** `*at` doors all
  serve an `O_PATH` dirfd, and this kernel's `base_for` accepts any `Dir`
  description without asking how it was opened — so ten were served by
  construction. The two that were not are the finding: `renameat` and `linkat` with
  a **dirfd-relative** name answered `EISDIR`/`EXDEV` **identically for an ordinary
  descriptor**, while the absolute spelling worked — so neither was an `O_PATH` bug.
  The `linkat` cause came from instrumenting, not reading:
  `LINKDIAG source_path="…/src" =4 dest="/" =0` — the source composed from its
  dirfd, the destination from `absolute(parent_path)`, which uses the **calling
  directory**, so a bare new name (`"."`) composed to `/` and one link inside one
  filesystem was compared across two. **Item 298's bug one door over**; the fix
  composes the destination from `new_base`, and the `O_PATH` spelling then needs
  nothing of its own. **Why `t-link` missed it**: every claim there passes `CWD`
  with an **absolute** name, where both bases are the same string — the comparison
  was vacuously right, and a test that only exercises the spelling where the bug
  cannot appear tests the other spelling. `t-link` **+4**. A **third** finding is
  named as its own item: `renameat2` has no `NodeKind::Dir` arm and answers
  `EISDIR`, deliberately (a subtree's parent back-pointer would have to move) —
  item 307. Gate `build/gate-item306.log` `GATE_EXIT=0`: stage 1 1340, x86_64
  **328**, aarch64 **324** (both up four, the new claims staging on both machines),
  kunit 42/0 per arm, clippy unchanged at `162/168/393/272/3/1`. **Named**:
  `name_to_handle_at`/`open_by_handle_at` and the xattr `*at` family were not
  exercised with an `O_PATH` dirfd, and `fchmodat2` was not asked. (§6.185,
  item 306)

- **Item 307 — renaming a directory: the comment said the subtree would have to
  move, and the tree does not work that way**: `renameat2` had no `NodeKind::Dir`
  arm at all (`_ => return err::EISDIR`, "a directory rename would have to move the
  whole subtree's parent back-pointer"). **The premise is false**: this tree keeps
  no parent pointers — `..` is answered by a *search* (`MountTable::parent_of`,
  bounded, and a `..` it cannot resolve stays put) — so a directory's location
  follows from *which directory holds its name*, and moving the name moves the
  subtree. The sentence that closed this for months described a data structure this
  kernel does not have; what *is* true of it is a **disk**, where an ext4 directory
  stores its parent's inode number in its own first block. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{rename_dir,rename_dir2}.c`): a populated subtree moves
  whole with its mode and contents and the same inode; onto an existing **empty**
  directory it is served and the target's name reaches the source's inode; onto a
  **non-empty** one `ENOTEMPTY` with nothing moved; directory→file `ENOTDIR`;
  file→directory `EISDIR`; into its own subtree at any depth `EINVAL`; onto itself
  `0`. The fix splits where Linux splits — `Inode::rename_dir_into` for the
  filesystem's accounting (default `Unsupported`), the syscall arm for everything
  that does not need the nodes — and the two implementations differ because the
  filesystems do: **ramfs** is two `BTreeMap` edits and the subtree needs nothing
  (the concrete proof of the paragraph above), **ext4** is three writes in
  crash-window order (`dir_add`, then `..` rewritten in place by `set_dir_parent` —
  a four-byte edit, where a remove-and-insert would rewrite the block for one
  number — then `dir_remove`), so a failure between any two leaves **both** names
  rather than a subtree that does not close, which is Linux's own `ext4_rename`
  order. **The guest caught one thing the draft had wrong**: an existing empty
  target must be *replaced*, and neither `rename_dir_into` can do it (both answer
  `Exists` for a name that is there), so the replace is the caller's decision and
  the syscall arm asks `dir.rmdir` first, guarded by the emptiness check that
  already ran — the guest answered `EEXIST` before that line existed. `t-rename`
  **+17**, including the `..` reachability claim that a half-move would pass. First
  gate run reddened on **clippy alone** (`unused_mut` up one on three arms from the
  draft's `set_dir_parent`, fixed by deleting the `mut`); `build/gate-item307b.log`
  `GATE_EXIT=0`: stage 1 1340, x86_64 **324**, aarch64 **320**, kunit 42/0 per arm,
  clippy unchanged at `162/168/393/272/3/1`. **Named**: on ext4 the rename is not
  atomic (no journal transaction — a crash between the adds leaves both names);
  `RENAME_EXCHANGE`/`RENAME_WHITEOUT` remain `EINVAL`; a cross-mount rename is
  `EXDEV` and `move_mount(2)` is not implemented. (§6.186, item 307)

- **Item 308 — `RENAME_EXCHANGE` swaps two names, and neither name moves**: §6.186
  left this (with `RENAME_WHITEOUT`) at `EINVAL`; with the directory rename landed
  the first became servable. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{rex,rex2,rex3,rex5}.c`): two files exchange; onto a
  **missing** name it is `ENOENT` where a plain rename would create it; a **file and
  a directory** exchange and the subtree follows the name; two directories in **two
  parents** exchange; an exchanged directory keeps **the `..` it had**;
  `EXCHANGE|NOREPLACE` and `EXCHANGE|WHITEOUT` are `EINVAL`; a self-exchange is `0`.
  **The `..` row is the finding**: this looks like a rename and is not one — both
  names stay where they are and only the inode behind each changes, so no parent is
  rewritten and §6.186's whole problem does not arise. A first draft answered
  `EXDEV` for the cross-directory case ("the two parents would each have to learn
  what the other lost") and they do not, because nothing left either directory —
  **the same shape of error as §6.186's own, produced by the item that fixed it**.
  Two primitives, because one directory's swap and two directories' swap are
  different accounting: `exchange_children` and `exchange_with`, both defaulting to
  `Unsupported`. ramfs is four `BTreeMap` edits; ext4 is four-byte dirent edits
  ordered for the crash window (clear **both** to `0`, then point each at the other's
  original inode, so an interruption leaves a name pointing at nothing rather than
  two names for one node). **`RENAME_WHITEOUT` stays refused for a measured
  reason**: Linux implements it by leaving a **character device with `rdev 0`**, and
  no overlay mount here could read one (and `mknod` of a char device is `ENXIO`
  because no driver stands behind any number). `t-renameat2` **+17**, including the
  cross-directory directory exchange whose `..` claim a "swap inodes and reparent"
  implementation would fail. Gate `build/gate-item308.log` `GATE_EXIT=0`, both arms:
  stage 1 1340, x86_64 **324**, aarch64 **320**, kunit 42/0 per arm, clippy unchanged
  at `162/168/393/272/3/1` — green first try. **Named**: `RENAME_WHITEOUT` should
  carry `EOPNOTSUPP` rather than `EINVAL` (a small flag-gate change not made here);
  `EXCHANGE` across two filesystems answers `CrossDevice`/refuses a non-ramfs partner
  but is **not pinned** (two different mount types are not a default-sweep fixture);
  `move_mount(2)` is unimplemented. (§6.187, item 308)

- **Item 309 — `RENAME_WHITEOUT`: the support was already there, and the note said
  it was not**: §6.187 left the flag refused with the reason "`mknod` of a char device
  is `ENXIO` because no driver stands behind any number, so creating one would be an
  object nothing in the tree can interpret". **Two of those three clauses are false**:
  `mknod` of a char device *is* served on a memory filesystem (`ENXIO` is what
  *opening* one answers — a different door), and the capability rule for exactly this
  node is already in the tree — `abi::mknod_needs_capability` carries
  `0o020000 => dev as u32 != 0, // S_IFCHR, except the whiteout's 0`, under a comment
  naming it an overlayfs whiteout. Somebody measured Linux's rule and wrote it down;
  **nothing was wired to it.** Third time in this family that a *sentence* rather than
  a reading closed an item (§6.184's `syncfs`, §6.186's parent pointer, this).
  Measured on Linux 7.3.4 (`build/scratch/opathmmap/{wout2,wflags}.c`): the node reads
  `mode 0000`, `S_ISCHR`, `rdev 0`; the move is an ordinary rename (a taken target
  replaced, a missing one created); a **directory** may be whited out and its tree
  travels; **`WHITEOUT|NOREPLACE` is legal** (`EEXIST` on a taken target) while
  `WHITEOUT|EXCHANGE` is `EINVAL` — and the pair rule **cost a round with the probe at
  fault**: a first draft refused `WHITEOUT|NOREPLACE` on the assumption the flags
  partition, and the probe that "confirmed" it reused one fixture directory so a
  leftover target made the answer look like `EINVAL`; **a probe that does not reset its
  state measures its own leftovers.** Which filesystems can answer is the man page's
  own rule and comes out opposite to Linux's list here: ramfs keeps `i_rdev` and can
  make the node, ext4 has nowhere to put one (`Inode::create` refuses a device node
  there for that reason), so a filesystem without support answers **`EOPNOTSUPP`** —
  and the pin asserts *either*, because the sweep's root is ramfs and aarch64's is
  ext4. `t-renameat2` **+13 (41 claims)** — and the pin **skipped its own claims
  twice**: six silently unrun because it wrote a name a previous step had left a
  whiteout node on (`write` on a char device is `ENXIO`), then sixteen more because
  the exchange section leaves `xa` a *directory* (`write` answers `EISDIR`); both are
  the gate's own "a suite that silently never ran leaves its cases out of the count",
  inside a test. Fixed by clearing every fixture **by kind**; stable at 41 across runs
  on both machines. Gate `build/gate-item309b.log` `GATE_EXIT=0`: stage 1 1340, x86_64
  **324**, aarch64 **320**, kunit 42/0 per arm, clippy unchanged at
  `162/168/393/272/3/1` — the first run reddened on **clippy alone**
  (`let_unit_value` ×2 from this change's own `let moved = …` binding a `()`), fixed by
  dropping the binding. **Named**: a whiteout on ext4 is `EOPNOTSUPP` deliberately (no
  `i_rdev`, so an overlay root on ext4 is not bootable); **nothing reads a whiteout
  yet** — no mount here is an overlay, so this writes a format whose reader does not
  exist ("we can write it" is not "we can use it"); and the door does not itself re-ask
  `CAP_MKNOD` (measured as matching, since `dev == 0` needs none, but by construction
  rather than by a check). (§6.188, item 309)

- **Item 310 — `execveat`: the door that runs a descriptor's own file**: found by a
  **census** rather than a gap list — libc's `__NR_*` table minus `nr_x86_64.rs` gave
  **86 numbers the kernel never declares**, most dead (`uselib`, `_sysctl`,
  `query_module`, the `*_module` family, `nfsservctl`) but four calls real programs
  make: **`execveat` (322)**, `ioprio_get`/`ioprio_set`, `tee`, `vmsplice`. Every
  earlier item in this family started from a list somebody wrote; this one started
  from *the ABI's own table minus ours*. `execveat` went first because it is the one
  with **no fallback** — musl's `fexecve` is built from it, `posix_spawn`'s
  file-action path reaches it, and a caller execing an **already-verified descriptor**
  has no other way to say so. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/exat.c`): a relative name plus a dirfd is served;
  `("", AT_EMPTY_PATH)` runs the descriptor's file; **`(-1, "", AT_EMPTY_PATH)` is
  `EBADF`** — the descriptor judged *before* the name, which is the ordering claim (a
  kernel answering `ENOENT` first would pass a naive test and get the refused case
  wrong); `("", 0)` is `ENOENT`; an unknown flag bit is `EINVAL`;
  `AT_SYMLINK_NOFOLLOW` is accepted. The implementation **shares its body** with
  `execve` rather than copying it: `execve` resolves and delegates to `exec_inode`,
  which takes the file, the name to report, and the two register slots the arrays
  arrive in — the only thing the doors differ about — because a copy would be two
  hundred lines of loader logic with one line different and the second copy is how
  two doors come to disagree about a shebang. `base_for` became `pub(crate)` so the
  `*at` door resolves against its dirfd with the same function every other `*at` call
  uses. `t-execveat`: **11 claims, every one a forked child** (a successful exec does
  not return) — the child reports a refusal through a pipe before `_exit`, so the
  parent reads an errno rather than decoding an exit status, and the marker the new
  image prints is the evidence a *different* image ran, which an exit code alone
  cannot give. A drafting defect: the first version called the helper **twice per
  claim** (once in the `format!`, once as the assertion), so each ran twice and the
  failure message could report the second run's reading — fixed by binding once, 22
  forks became 11. **The gate found two things, both of them it working**: the
  dispatch-versus-table check failed ("1 row(s) of disagreement … a served call
  without a row is the review finding that document exists to make") so
  `docs/uapi/01-syscall-table.md` gained its row and its counts moved 301 → 302; and
  clippy caught three `unnecessary_cast` of mine, fixed not baselined. Gate
  `build/gate-item310b.log` `GATE_EXIT=0`: stage 1 1340, x86_64 **325**, aarch64
  **321** (both up one), kunit 42/0 per arm, clippy unchanged at
  `162/168/393/272/3/1`. **Named**: `ioprio_get`/`ioprio_set`, `tee` and `vmsplice`
  are still `ENOSYS`; `AT_SYMLINK_NOFOLLOW` is measured as accepted but not pinned as
  a *difference*; and the census's tail is **not all dead** — `fspick` (433),
  `landlock_*` (444–446), `cachestat` (451), `setxattrat` (463) and the LSM family
  (459–461) are current calls this kernel does not serve. (§6.189, item 310)

- **Item 311 — `ioprio_get`/`ioprio_set`: the block layer's ABI, and four drafts the
  measurement corrected**: the next pair from item 310's census (libc's `__NR_*`
  minus `nr_x86_64.rs`). Neither number was declared, so the dispatch answered
  `ENOSYS`; `ionice(1)` is the caller everybody has. **The measurement corrected the
  draft four times**, and each is a rule a written-from-memory version gets wrong:
  the `IOPRIO_WHO_*` values are **1, 2, 3** (not 0, 1, 2 — the draft would have
  answered `EINVAL` for every real caller); **bits above the 16-bit word are
  truncated, not refused** (`1 << 16` … `1 << 19` all served — the draft refused
  them, reasoning that a mask is a silent lie, but a caller with no fallback from
  `EINVAL` has nowhere to go); **the data is not validated against the class's
  range** (`BE/8` is stored); and **a group or user query answers the strongest
  member, where "strongest" is a max over the class and a *min over the data*** —
  `BE/7` beside `BE/1` answers `BE/1`, which a plain max fails. Plus the two that
  were right and are the security surface: `RT` wants `CAP_SYS_NICE` (measured with
  the host's empty `CapEff`) and the other three classes do not, and retargeting
  another user's task wants it where a same-uid child is served. The state is a
  `u16` on `Task` beside `nice`, kept honestly and **scheduling nothing** (one disk,
  no I/O scheduler — the shape `nice`'s weight has had since it landed). `t-ioprio`:
  **22 claims**, and the group fixture runs in a **session of its own** because a
  group query covers every member while the sweep runs this program as one of dozens
  in one lane's process group — asked in place, a first draft read another program's
  `BE/4` and was right about a membership the test did not mean to name. **Two
  pipe-based drafts of the parent/child sync were worse than the C probe's own
  150 ms sleep** (one reused a pipe in both directions so the child blocked on a
  closed end; the other announced with a byte and still lost the race); the sleep is
  what the measurement itself used. **The gate found two things**: the
  dispatch-versus-table check named the two missing rows (counts 302 → 304), and
  clippy caught an unnecessary cast plus the `DATA_MASK` constant the truncation
  change left unused. Gate `build/gate-item311b.log` `GATE_EXIT=0`: stage 1 1340,
  x86_64 **326**, aarch64 **322** (both +1), kunit 42/0 per arm, clippy unchanged at
  `162/168/393/272/3/1`. **Named**: the word schedules nothing; a foreign-uid
  `ioprio_get` is `ESRCH` (pinned with `4294967294`, not a normal account); and
  **`tee`/`vmsplice`** remain undeclared — item 310's census, still open. (§6.190,
  item 311)

- **Item 312 — `tee`: the pair to `splice` that does not consume its source**:
  the third of item 310's four census calls, and the cheapest because `splice` was
  already here. The number was never declared, so the dispatch answered `ENOSYS`;
  `tee(1)` is the caller everybody has. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{tee1,tee2,tee3}.c`): `tee(pipe, pipe, n)` copies `n`
  bytes and leaves the source **unchanged**; a writable-but-non-pipe side is
  `EINVAL`; an **unopenable** side is `EBADF`; a missing descriptor is `EBADF`; an
  unknown flag bit is `EINVAL` **even at `len == 0`**; a legitimate `len == 0`
  answers `0`; `SPLICE_F_MOVE` and `SPLICE_F_NONBLOCK` are served. **The order cost
  a round and is the finding**: a first draft asked the source's mode, then the
  flags, then both pipe questions, then the destination's mode — and answered
  `EINVAL` for `tee(pipe, O_RDONLY file)`, which should be `EBADF`. The mode is the
  *descriptor's* state and the pipe question is about the *node*; both mode
  questions come first, as `splice` beside it does. The four-combination table is
  the measurement (`tee(RDWR file, pipe)` `EINVAL`, `tee(WRONLY file, pipe)`
  `EBADF`, `tee(pipe, RDWR file)` `EINVAL`, `tee(pipe, RDONLY file)` `EBADF`). The
  transfer is a **peek then a write**, which is the contract itself: nothing is
  taken from the source and what arrives is what a read *would* have handed back —
  and **no new primitive was needed**, because `PipeEnd::peek_once` (added for
  `MSG_PEEK`) already answers a read's three shapes, so an empty source with a live
  writer parks the way a read would and `SPLICE_F_NONBLOCK` is how a caller asks
  for `EAGAIN`. `t-tee` **+17**, including the four-combination table and the claim
  that separates `tee` from `read`+`write`. **The gate caught two of my own
  defects**: the dispatch-versus-table check named the missing row (counts 304 →
  305), and clippy caught two **unreachable match arms** — `Ok(Some(0))` written
  after `Ok(Some(n))` in both results, the arm that *read* as the more explicit
  one. Gate `build/gate-item312b.log` `GATE_EXIT=0`: stage 1 1340, x86_64 **327**,
  aarch64 **323** (both +1), kunit 42/0 per arm, clippy unchanged at
  `162/168/393/272/3/1`. **Named**: `tee` copies through a 4 KiB scratch rather
  than sharing pipe buffers, so `SPLICE_F_MOVE` is accepted and ignored (the shape
  `splice`'s is); a **full destination is `EAGAIN`, not a park** — recorded because
  a caller sees it as a retry loop; and **`vmsplice` (278)** remains undeclared,
  the last of item 310's four and the only one needing `mm` work. (§6.191, item 312)

- **Item 313 — `vmsplice`: the direction the man page does not prepare you for**:
  the last of item 310's four census calls (`execveat` 310, `ioprio_*` 311, `tee`
  312, this). Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{vms1,vms2,vms3}.c`): a pipe's **write** end takes the
  iovecs' bytes, and a pipe's **read** end **drains into** them (returns 5, buffer
  holds the pipe's contents); several iovecs contribute in order; a regular file is
  `EBADF` on either side; `nr_segs == 0` is 0; more than `IOV_MAX` is `EINVAL`; an
  unknown flag bit is `EINVAL`; `SPLICE_F_GIFT` is **served**; a zero-length iovec
  is skipped; an unreadable base is `EFAULT`. **The drain row is the finding**: the
  man page describes the call as grafting pages *into* a pipe — one direction — so
  a probe that only wrote through a write end would have confirmed the man page and
  left the read side unimplemented; the probe that found it called `vmsplice` on
  the **read** end and got the pipe's contents in the buffer. **The man page
  describes the mechanism; the behaviour is the pipe end's.** `SPLICE_F_GIFT` is
  accepted and ignored — Linux grafts the caller's pages so a later `splice` skips
  the copy, and `GIFT` promises never to write them again, while this kernel's pipes
  hold **bytes** so both directions copy (the same shape `tee`'s `SPLICE_F_MOVE`
  takes); that is also why `EFAULT` is asked **before** the transfer. One accessor
  was needed rather than a second answer — `PipeEnd::role_is_write`, because a FIFO
  opened `O_RDWR` is `Role::Both` and a `Both` end is treated as the writing side,
  recorded as the approximation it is. `t-vmsplice` **+17**. **The gate found two
  things**: the dispatch-versus-table check named the missing row (counts 305 →
  306), and clippy caught an unnecessary `&mut` of a copied iovec array. Gate
  `build/gate-item313b.log` `GATE_EXIT=0`: stage 1 1340, x86_64 **328**, aarch64
  **324** (both +1), kunit 42/0 per arm, clippy unchanged at `162/168/393/272/3/1`.
  **Named**: **nothing is grafted** — both directions copy through a bounded
  scratch, so the zero-copy property is absent (a `vmsplice`+`splice` pipeline moves
  bytes twice here); a full destination or empty source answers `EAGAIN`, not a
  park; and `Role::Both` (FIFO `O_RDWR`) is treated as a write, unpinned.
  (§6.192, item 313)

- **Item 314 — the legacy `eventfd(284)` and `inotify_init(253)`: not aliases of
  their modern twins**: a **second** pass over item 310's census, because the first
  skipped these two as "the modern call exists, so the legacy one is the same" —
  which the measurement shows is wrong. On Linux 7.3.4
  (`build/scratch/opathmmap/{legacy,legacy2}.c`): `eventfd(count)` honours the
  counter, comes back `F_GETFL O_RDWR` with `FD_CLOEXEC` clear, and **ignores its
  second argument entirely** (`0x7ffff` served, no effect) where `eventfd2(0, 0x99)`
  is `EINVAL`; `inotify_init()` comes back `F_GETFL` 0 with `FD_CLOEXEC` clear,
  where `inotify_init1(IN_CLOEXEC)` sets it. **The asymmetry is the item**: routing
  both spellings through one handler has exactly two outcomes and both are wrong —
  refuse the legacy caller's ignored second argument (`EINVAL` for a call Linux
  serves), or accept an unknown bit on the modern door (a descriptor whose `F_GETFL`
  disagrees with what was asked for, the §4.2 shape §6.176 closed). The modern door
  must validate; the legacy one must not look at the word at all. **Both numbers are
  x86_64-only** — the asm-generic table aarch64 uses never had the legacy spellings
  — so their aarch64 entries take pseudo-numbers (`64034`/`64035`, the file's own
  convention for 33 such calls) and the pin **states** that on aarch64 rather than
  printing nothing. **Two defects, both mine, both caught by the gate**: the aarch64
  arm first hardcoded x86_64's 290/294 (where aarch64 numbers them 19/26, so the
  *modern* doors looked broken — the claim was about the program, not the kernel,
  the same shape §6.185 warned about), and the number test still asserted
  `EVENTFD == 290` after that name was given the legacy value (the test doing its
  job on a rename; the same commit added this session's five new numbers to that
  list). `t-legacy-fds` **+14**, with the modern doors claimed on both machines as
  the reference point. Gate `build/gate-item314c.log` `GATE_EXIT=0`: stage 1 1340,
  x86_64 **329**, aarch64 **325** (both +1), kunit 42/0 per arm, clippy unchanged at
  `162/168/393/272/3/1`. **Named**: legacy `inotify_init` is unpinned on aarch64
  because it cannot be called there; and the census's other legacy numbers
  (`uselib`, `ustat`, `_sysctl`, `create_module`, the `*_module` family) are dead
  calls on modern Linux too, so `ENOSYS` is right and they stay undeclared.
  (§6.193, item 314)

- **Item 315 — the `pkey_*` trio: a refusal with a measurement behind it**: found
  walking the census after items 310–314 landed five calls. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{pkey,pkey2,pkey3,pkey4,pkey5}.c`): keys numbered from
  **1**, **15** of them then `ENOSPC`, `pkey_free` of an unallocated key `EINVAL`,
  `PKEY_DISABLE_ACCESS`/`_WRITE` 1/2 with the pair served, `pkey_mprotect(key=-1)`
  taking `mprotect`'s own path. **The row that decides it: the protection is real**
  — memory mapped under a `PKEY_DISABLE_ACCESS` key faults a touching child with
  `SIGSEGV`. This kernel has **no PKU at all** — `CR4.PKE` never set, no PTE carries
  a protection-key field, no per-task `PKRU` — so serving these would hand a caller
  a zero for a boundary that is not there (`AGENTS.md` §3.2). Linux runs them only
  under `CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS`, so `ENOSYS` is what a libc
  already reads as "not available here", the shape `memfd_secret` has had since
  item 131. The row is now in the uapi gap table beside it, with the prerequisite
  written out (flags, `CR4.PKE`, per-task `PKRU`, page-table bits). **And a pin that
  was broken before this item touched it**: `t-enosys`'s hosted-run escape read
  `host_run() && r == -1` — "any refusal stands in" — but a current host *serves*
  `memfd_secret` (it answers fd 3), so the clause was always false and the host leg
  failed on two claims working as designed; fixed to `host_run()` alone. That had
  been in the file since `rseq` moved out, invisible because the host stage's report
  is easy to read past (the §6.183 shape: a gap whose stated reason had aged). The
  pin grows the three claims with per-machine numbers (x86_64 329/330/331,
  asm-generic 288/289/290). Two red runs before green, both clippy alone:
  `doc_lazy_continuation` 45 → 50 from a bulleted list inserted mid-sentence in the
  header, fixed by rewriting the paragraph rather than raising a ceiling. Gate
  `build/gate-item315d.log` `GATE_EXIT=0`: stage 1 1340, x86_64 **329**, aarch64
  **325**, kunit 42/0 per arm, clippy unchanged at `162/168/393/272/3/1`. **Named**:
  PKU itself is not implemented (the prerequisite list is its honest scope), and
  `pkey_mprotect`'s `EINVAL`-vs-`EFAULT` ordering for a bad address *and* a bad key
  is unpinned because without PKU there is nothing to pin it against.
  (§6.194, item 315)

- **Item 316 — `rt_tgsigqueueinfo`: the door that names one thread, and the pair
  that must agree**: found by walking the census for a **sibling** —
  `rt_sigqueueinfo` (129) has been served for a long time, `rt_tgsigqueueinfo` (297)
  was never declared. The pair is the point: the first names a **process** and lets
  the group pick a thread that may take the copy; the second names a thread and
  nobody else may. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{tgsq,tgsq2,tgsq3,tgsq4,tgsq5}.c`): `(tgid, tid)` naming
  one thread is served and the payload arrives on it; **a `tgid` from another group
  than the `tid` is `ESRCH`** — two live numbers can still be a malformed request;
  `tgid <= 0` or `tid <= 0` is `EINVAL` (malformed, not missing) where a positive
  number naming nothing is `ESRCH`; `sig == 0` is served and queues nothing;
  `sig > 64` / `NULL` record are `EINVAL` / `EFAULT`; and **`si_signo` in the record
  is not the sender's to choose** — the argument wins. **The pair agreeing is the
  claim the sibling has no analogue for.** Everything else is shared rather than
  restated (the `si_code`/authority rule, the record copy, `may_signal`) because a
  second copy of a signal rule is how two doors come to disagree; the difference is
  the last step, `send_signal_info` putting the copy on the **named thread's own**
  pending set. Two probe defects, both mine: the first probe **died of its own
  signal** (exit 138 — it queued `SIGUSR1` unblocked), and `-1` passed through a
  `usize` becomes `0xFFFFFFFF`, which the first pin run misread as a kernel
  divergence. **And an existing pin that had been failing on its host leg all
  along**: `t-rusage-switch` — `sched_yield` with nothing runnable is a no-op on
  Linux (measured: 200 yields leave `nvcsw` 2 → 2, `nivcsw` 0 → 0), so that claim
  needs the `host_run()` escape it never had; and the involuntary-switch bound of 10
  is **11** in a lane six TCG deep (the host preempts the guest's vCPU while it
  sleeps), so it moved to 25 **with both measurements written beside it** — a miss
  of one under contention is measuring the host, fifty is measuring the kernel.
  Gate `build/gate-item316b.log` `GATE_EXIT=0`: stage 1 1340, x86_64 **330**,
  aarch64 **326** (both +1), kunit 42/0 per arm, clippy unchanged at
  `162/168/393/272/3/1`. **Named**: a **sibling thread** is not pinned as a target
  (the pair's rule is proven with the caller's own numbers), and
  `RLIMIT_SIGPENDING`'s `EAGAIN` is shared with the sibling and not re-pinned.
  (§6.195, item 316)

- **Item 317 — the legacy POSIX AIO family: a context handle, not a ring**:
  `io_setup`/`io_destroy`/`io_submit`/`io_getevents` (206/207/209/208, plus
  `io_pgetevents` 333) — all undeclared, so a libaio caller heard `ENOSYS` from the
  first of them. **This is not `io_uring`**: that is a ring of shared memory (425,
  served since item 115), this is a context handle plus `struct iocb` pointers, and
  the shared prefix is a trap. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{aio,aio2,aio3,aio4}.c`): `io_setup` writes a handle
  (`0`/`NULL` are `EINVAL`/`EFAULT`), `io_destroy` twice is `EINVAL`, a PREAD is
  served with the byte count in the event and `aio_data` returned **verbatim**, a
  bad context is `EINVAL` on every door, and `io_getevents` with too high a
  `min_nr` returns what **is** queued. **Three of those corrected a draft**: `struct
  iocb`'s offsets are not the header's field order (`aio_key`/`aio_rw_flags` share
  the word at 8, so opcode is at **16** and fildes at **20** — a draft read the
  opcode out of `aio_rw_flags` and made every request unknown); a first version
  **served `FSYNC`/`FDSYNC`/`NOOP`** because their names suggest work a synchronous
  kernel can do, and **Linux refuses all three** — a success for a refused call is
  the one thing §3.2 forbids, so the served set is `PREAD`/`PWRITE` as measured; and
  a bad descriptor was queued as an event carrying `-EBADF` where **Linux fails the
  submit**, which matters because an event means *the request ran*. The scope is
  the **synchronous subset** (§6.32's shape for `io_uring`): each `iocb` runs
  inside `io_submit`, a performance shape rather than an ABI one — the observed
  sequence is identical, only *when* the work happened differs. `t-aio` **+23**,
  including the opcode refusals and the **handle-gone** property (a second
  `io_destroy` is `EINVAL`, not a silent success — §6.90's class, which is why
  handles are never reused). **The gate caught three of my own defects**: an unused
  `AioContext::id`, three redundant casts in the pin, and — the one worth
  recording — **the `#[cfg(target_os = "none")]` gate on `io_uring` moved onto
  `aio`** when both declarations shared a prefix in a text replace, breaking the
  host build of the ring. Gate `build/gate-item317c.log` `GATE_EXIT=0`: stage 1
  1340, x86_64 **331**, aarch64 **327** (both +1), kunit 42/0 per arm, clippy
  unchanged at `162/168/393/272/3/1`. **Named**: `PREADV`/`PWRITEV` refused with
  `EINVAL` where Linux serves them given a real iovec (a gap *here*, not in Linux —
  use `io_uring`'s `READV`); `IOCB_FLAG_RESFD`/`IOPRIO` read past and ignored;
  `io_cancel` (210) still `ENOSYS`; and completions are never deferred, so
  `io_submit` may block where Linux's would not. (§6.196, item 317)

- **Item 318 — `getdents`: the directory reader one header byte from its twin**: a
  **sibling**, not a leftover — `getdents64` (217) has been served for a long time,
  `getdents` (78) was never declared, so a program built against an old glibc heard
  `ENOSYS`. Most of the census's remaining numbers are dead calls or whole features;
  this is a second spellling **whose records differ**, which is exactly where "the
  modern call exists, so the old one is the same" is wrong. Measured on Linux 7.3.4
  (`build/scratch/opathmmap/{gd,gd2,gd3,gd4}.c`): the same directory through both
  doors agrees on `d_ino`/`d_off`/`d_reclen` (every name short enough to round
  alike) but differs in layout — **18 bytes of header and no `d_type`** (name at 18)
  against 19 with a `d_type` (name at 19), so a reader using `struct dirent64` reads
  its name **one byte late**. A host unit test pins `reclen_of_legacy` beside
  `reclen_of` at the lengths where the rounding actually diverges (`reclen_of(5)`
  is 32, `reclen_of_legacy(5)` is 24) — a claim that the legacy record is *always*
  shorter is false. Everything else is **shared rather than restated** (`O_PATH`
  `EBADF`, the walk, the position, the short-buffer `EINVAL`): two copies of a
  directory walk is how two doors disagree about a position. **Two ordering
  defects**: the descriptor must be judged **before** the buffer — measured,
  `getdents(-1, NULL, 64)` is `EBADF`, not `EFAULT`, and the draft answered about
  the buffer; and the aarch64 arm of the pin **hardcoded x86_64's 217** where
  asm-generic numbers `getdents64` **61** — **the second time this session** (item
  314's pin records the first), caught by the aarch64 sweep. A third finding shaped
  the pin: the host's ext4 lists `.`/`..` and this kernel's ramfs does not (its
  `getdents64` is the same), so the claim is "every fixture name is listed and
  nothing invented is" — which entries a directory invents is the filesystem's
  answer, not a door's. `t-getdents` **+19**, including the layout, the position,
  all five refusals, and the twin's `d_type` as the contrast; it is **x86_64-only**
  and says so on aarch64. Gate `build/gate-item318c.log` `GATE_EXIT=0`: stage 1
  **1341** (up one, the new unit test), x86_64 **332**, aarch64 **328** (both +1),
  kunit 42/0 per arm, clippy unchanged at `162/168/393/272/3/1` — after two clippy
  rounds of my own (`int_plus_one`, and `dead_code` for `reclen_of_legacy`, allowed
  at the item **with its reason** because its only caller is machine-only). **Named**:
  `getdents` is x86_64-only (pseudo-number on aarch64, stated); `d_off` is the entry
  index rather than the filesystem's opaque cookie; and the `.`/`..` difference is
  the ramfs's existing shape, shared with its twin. (§6.197, item 318)
