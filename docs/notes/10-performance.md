# §10 Phase 10: Performance

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 10: Performance

One number framed this phase: `ping 127.0.0.1` measured 20 ms on the 100 Hz
build (7.5 ms at the current 250 Hz) against the host's 0.03 ms. The cause
was architectural rather than slow code - every blocking network wait slept a
whole clock tick per round, arrived frames were discovered only by tick-rate
polling because the virtio-net adapter was never interrupted, and an AF_INET
socket had no wakeup source at all (pipes and AF_UNIX already did). Each
subsystem below names the same shape of fix: make the event end the wait.

### 10.1 Network Latency — **DONE**

- Blocking socket calls (`recv`/`send`/`connect`, TCP/UDP/raw) park on their
  own readiness through `block_until_deadline` instead of sleeping a tick per
  round; signals still end them as `EINTR`. Sockets register with the stack
  through `poll_subscribe`, and every poll wakes the registered list.
- `poll(2)`/`select(2)` park the same way, with the tick kept as the bound
  for descriptors that have no waker behind them (uevent, signalfd,
  eventfd). An unclocked `epoll_wait` over a network socket - which
  previously would sleep forever, having no wakeup source - now ends with
  the event (`t-epoll-socket`).
- The virtio-net adapter is armed for MSI-X: the vector table entry is
  programmed by hand and every queue bound to vector 0 through the common
  configuration, whose BAR is 64-bit and sits above 4 GiB (reading only the
  low dword maps ordinary RAM, and register "writes" round-trip through it
  while the device hears nothing). The handler polls, wakes the waiters,
  EOIs the local APIC, and preempts so the woken task runs immediately
  rather than at the next tick. The tick-rate polls all remain, so a machine
  where the interrupt cannot be armed runs exactly as it did.
- `sendto`/UDP/TCP emit immediately after queueing rather than at the next
  poll; smoltcp's clock reads the TSC instead of the tick counter; `xtask`
  builds release by default (the loader gained the `wcslen` its release
  codegen calls for).
- Measured by `t-netlat` (20 ICMP echo rounds to 127.0.0.1): 7.5 ms → 0.3 ms
  average, 74 µs floor - the remainder is slirp's own path through the host.

### 10.2 Context-Switch and Fork Cost — **PARTIAL** (three of four landed, measured)

**Why**: every task switch saved and restored 512 B of FPU state
(`fxsave`/`fxrstor` unconditionally), rewrote CR3 with a full TLB flush, and
`fork` walked the entire address space to mark pages COW with a flush after
every page. A busybox fork+exec paid all three.

| Change | Where | State |
|---|---|---|
| Save FPU state lazily: `CR0.TS` on the task entered, its `#NM` restores it | `arch/x86_64/context.rs`, `idt.rs`, `sched/runtime.rs` | **done** |
| One TLB flush per fork instead of per-page `protect` calls | `sched/runtime.rs`, `arch/x86_64/paging.rs` | **done** |
| Kernel stacks smaller than the fixed 64 KiB per task | `sched/runtime.rs` | **done** |
| Enable PCID (`CR4.PCIDE`), keep per-process TLB entries across switches | `arch/x86_64/paging.rs` | **not done** — the argument is below |

**The lazy FPU.** `mpos_context_switch` no longer touches the 512 bytes of
`FpuState` at the head of each `Context`; it sets `CR0.TS` on the task it
enters, and that task's first floating-point instruction traps into the `#NM`
handler, which clears the bit and loads the image. The saving half stays eager
and moved into Rust (`Table::hand_over_fpu`), because that is the last moment
at which the outgoing task's registers are uncontroversially its own. A task's
`fpu_live` flag says whether the hardware holds its state, so the FPU is written
out only when it was actually used: a program that never executes a vector
instruction - which is most of this image, and all of a shell's `fork`+`exec` -
pays one `mov cr0` per switch and nothing else. `fork`/`clone` read the
creator's image through `Table::inherit_fpu` (registers when live, its saved
block when not) and `execve` claims the registers before `fninit`, because a
task that never used the FPU is running with `TS` set. The property the whole
scheme rests on - that the register file belongs to a task and not to the CPU -
was already pinned by `t-fpu-across-switch`, which stores two distinguishable
bit patterns to `xmm7` across a `sched_yield`; it passes unchanged, and it
exercises the deferred restore on both sides.

**The fork flush.** `parent_space.protect(..) + set_cow_bit(..)` per page became
`set_cow_bit_quietly(..)` per page and one `flush_if_active()` after the loop.
What that removes is 16384 CR3 writes for a parent with 8192 pages, and the
per-page `update_flags` round trip that re-derived the PTE's flags from the
walk's own report - the quiet form sets the COW bit and clears `WRITE` in one
write and leaves the rest of the entry alone, which also stops the fork from
dropping `ACCESSED`/`DIRTY` on the parent's pages.

**What the machine says.** `t-forkcost` (new, and in the suite) runs the *same*
fork/exit/wait loop twice: once with a 32 MiB arena mapped in the parent, and
once after `munmap` has given it back. The second parent has a few pages of
address space instead of 8192, so anything that scales with the mapping shrinks
by three orders of magnitude.

| 220 cycles, 200 timed | `fork` | `wait4` |
|---|---|---|
| 32 MiB parent, per-page flush (the old shape) | 349.0 µs | 7650.8 µs |
| 32 MiB parent, one flush (this change) | 350.9 µs | 7648.9 µs |
| no arena at all, one flush | **17.0 µs** | 7982.8 µs |

Three things fall out of that, and the first two are corrections to what this
section said before the measurement existed.

**The fork flush change is not measurable, and the reason is not subtle.** Sixteen
thousand CR3 writes moved the fork by 0.5% - inside the noise - because a CR3
write is not a TLB flush on this machine: QEMU runs with `-accel kvm`, the
guest's translations are tagged (`VPID`), and loading a CR3 is a tag switch
rather than an invalidation. The change is kept because it is strictly less work
and the smaller of the two shapes, but nobody should read a speedup into it.

**The cost of a `fork` is the mapping; the `wait4` after it was the clock.**
Taking the arena away cuts the fork from 351 µs to 17 µs - twenty-one times -
and used to leave the `wait4` where it was, at *eight milliseconds either way*.
An earlier draft read the second row as the child's exit and its 8192 frame
references coming home (`release_user_frames`), and the third row refuted it: a
parent reaping a child that owned nothing paid exactly the same 8 ms.

**What that 8 ms was, and why it mattered more than anything in this section.**
One tick of this build's 250 Hz clock, on *every* blocking wait, and the cause
was the boot task. `park()` - where the boot task spends the machine's life -
halted the core with `wait_for_interrupt` instead of blocking in the scheduler,
which left a permanently *runnable* task at NORMAL priority: the picker handed
it the CPU, `hlt` stopped the core while holding it, and a task that has halted
can only be taken off it by an interrupt - of which the one that reliably
arrives is the clock. Anything woken while the idle task was in that state (or
behind it in the ready queue) waited for the next tick. A probe in the scheduler
recording the wake-to-resume latency in whole ticks is what named it: 604 of 640
wakes took *exactly one* tick, never two, while the scheduler itself never went
idle, so the task was ready and the picker was choosing somebody else first.
Parking the boot task in the scheduler puts the idle halt back in the
scheduler's own loop, which is where the virtio-net handler's comment already
assumed it was - "in the scheduler's own context ... the scheduler's next turn
picks the woken task on its own". That is the whole fix: one call site.

| `t-forkcost`, 200 cycles | before | after |
|---|---|---|
| `wait4`, 32 MiB parent | 7 648 682 ns | 1 760 ns - 100 µs |
| `wait4`, no arena | 7 980 378 ns | 13 046 ns - 100 µs |
| `fork`, 32 MiB parent | 351 052 ns | 223 983 - 298 868 ns |

The `fork` row is the control and it is the mapping: it did not move. The `wait4`
rows are now the child's own exit and a picker turn instead of a clock tick, and
the *range* is the honest shape of that figure rather than noise in it: a parent
that reaches `wait4` after its child has already exited does not block at all and
reads ~2 µs, one that gets there first blocks and reads ~100 µs on a loaded
machine - and one of them can print *below* the clock's resolution, where a
negative figure is the TSC fraction's noise floor and not a negative time. What
matters for both is the same: they are tens of microseconds against a tick of
4 ms, which is what `t-forkcost` now *asserts* (§10.7). §10.4 carries the same
fix's effect where it was largest.

**Why PCID is not done, and it is a decision rather than an oversight.** Three
things have to be true before `CR4.PCIDE` pays for itself, and only the first is
straightforward. (1) Each address space needs a stable PCID and a way to
invalidate a *recycled* one - manageable, with 4096 of them and 64 task slots.
(2) The kernel half must be mapped `GLOBAL`, or every process carries its own
copy of the kernel's translations in the TLB: the `G` bit is only honoured on
the entry that maps a page, so making the kernel half global means setting it on
the bootloader's huge-page leaves, and `page_flags` deliberately leaves it off
today for exactly this reason. Without it, a PCID switch trades one flush for a
per-address-space replication of the kernel's working set. (3) Every existing
CR3 write has to be re-audited for *which* PCIDs it must invalidate: with
`PCIDE` on, `activate(root)` invalidates the PCID written and leaves the others
alone, so `map_mmio` and the kernel-half table walks - which is where a write
adds a mapping every address space must see - would leave stale entries in every
*other* PCID, which is a wrong translation rather than a slow one. On top of
that, the workload here is `fork`+`exec`, and every `exec` builds a fresh
`AddressSpace` with a fresh PCID, so the reuse PCID is paid for is rare by
construction: the win would be the parent's entries surviving a child's
lifetime, which the table above suggests is not where the time goes anyway.
Recorded rather than attempted, so the next person starts from the audit list
rather than from the idea.

### 10.3 Syscall Path — **PARTIAL** (the cheap half landed)

**Why**: every scheduler turn and every clock tick scanned all 64 task slots
for an armed interval timer whether any process had armed one, and every
user-buffer operation walked four page-table levels per page.

| Change | Where | State |
|---|---|---|
| A global "any itimer armed" flag; `check_itimers` skips the scan when clear | `sched/runtime.rs` | **done** |
| Validate user buffers with the copy itself (fault handler catches `EFAULT`) | `arch/x86_64/paging.rs`, `syscall/mod.rs` | **not done** — the argument is below |

**The itimer flag.** `ITIMER_ARMED` is set by `itimer_swap` - which is the only
place a timer is ever armed, because no task inherits one and `exec` clears them
- and recomputed *by the scan itself* when it runs. That ordering is the point:
a task that exits while armed disarms a timer without going through
`itimer_swap`, so a flag that was only ever "set on arm" would stay set for the
rest of the boot and quietly restore the full scan. Recomputing costs one extra
scan after the last timer goes away and makes the flag wrong only in the
direction of running the scan - never in the direction of skipping a timer that
is due. Only `ITIMER_REAL` is tracked, because it is the only one that is a
wall-clock deadline the scheduler has to honour.

**Why the fault-driven copy is not here.** The plan's premise does not survive
reading the code. There is no pre-walk separate from the copy: `read_user` and
`write_user` translate one page at a time *as they copy*, through the direct
map, and a read is therefore one walk per page, not two. Only the write
direction walks twice, and the second walk is the copy-on-write probe
(`is_cow_page` before `translate`) - which is a handful of loads against the
page-table entries the walk just touched, next to a `memcpy` of the same page.
Deferring all of it to the CPU's own address translation would mean the kernel
dereferencing a user pointer, and then a `#PF` taken in **ring 0** on a bad
pointer has to become `EFAULT` rather than a panic: that needs an
exception-table/fixup mechanism (identify the faulting RIP as inside the copy
routine, redirect the trap frame to a landing pad, resume), and getting the RIP
range check wrong turns a real kernel bug into a silent `EFAULT`. The safe shape
is kept: the kernel never dereferences a user pointer, so a bad one cannot fault
at all. What is left as a possible future is the one-walk dedup of the write
path, which is a strict reduction in work with no measurable claim attached.

### 10.4 Pipes and Per-Frame Allocations — **DONE** (all three landed; the rate pair awaits two gate runs)

**Why**: pipe reads and writes moved bytes one at a time through a `VecDeque`
(`pop_front`/`extend(iter().copied())`), and the network path allocates and
frees a fresh `Vec` per frame in each direction.

| Change | Where | State |
|---|---|---|
| A ring buffer with slice copies for pipe data | `vfs/pipe.rs` | **done** |
| The raw receive temporary sized from the actual datagram, or on the stack | `net/socket.rs` | **done** |
| A pool of frame buffers behind the smoltcp device seam | `net/pool.rs` | **done** |

**The pipe.** `PipeState::data` is now a [`Ring`] over a `Vec` whose live bytes
are `bytes[start..]`: storage taken lazily, reclaimed when the reader catches the
writer, and the dead prefix dropped with one `drain` on the pass where the
arrival would otherwise pass `CAPACITY`. A read is one slice copy and a write is
one `extend_from_slice`, where the old shape was 8192 deque operations for a
`PIPE_BUF` round trip. `t-pipecost` (new, and in the suite) measures it in two
phases, because the two turn out to answer different questions:

| 16 MiB through one pipe | before | after |
|---|---|---|
| one process, 4096-byte units (data path only) | 61.7 ms - 259 MiB/s | 6.1 ms - **2611 MiB/s** |
| two processes, 64 KiB chunks (a pipeline) | 2047.5 ms - 7 MiB/s | 2047.4 ms - 7 MiB/s |

The first row is the change: ten times the throughput, which is what two slice
copies per 4096 bytes instead of 8192 deque operations buys.

**The second row was the finding, and it was bigger than the change.** A
two-process transfer did not move at all when the ring landed - 2047.5 ms to
2047.4 ms - and the arithmetic said why: 2047 ms over 256 chunks is **8.0 ms per
64 KiB chunk**. The reading taken at the time was "two ticks per handover", and
the count was right while the unit was wrong: a chunk is not one handover but
*two blocking waits* - the writer waiting for room in a full pipe, the reader
waiting for data in an empty one - so the figure was **one tick per wait**, and
one tick was the cost of *any* wake on this machine (§10.2). The bytes were
never the cost. `t-pipecost` is the instrument, and it was deliberately in the
suite so the fix had a number waiting for it:

| 16 MiB through one pipe | before | after |
|---|---|---|
| one process, 4096-byte units (data path only) | 6.5 ms - 2 478 MiB/s | 1.6 ms - **10 206 MiB/s** |
| two processes, 64 KiB chunks (a pipeline) | 2047.3 ms - 7 MiB/s | 1.6 ms - **10 168 MiB/s** |

The second row is the whole point of the pair: the same 16 MiB, through the same
pipe, now crosses 256 task boundaries at *the same speed as the copy that
crosses none*. Nothing about the pipe changed for this - the fix is the boot
task's park (§10.2) - and it is the clearest statement of what a wake used to
cost, because the pipeline was previously 1 300 times slower than the data path
it was built out of.

**The raw receive.** `raw_recv` filled a fresh `alloc::vec![0u8; 1500]` and then
copied it into the caller's buffer. It now receives straight into `out`: no
allocation, no second pass over a full MTU, and the "packet larger than the
caller's buffer" case is `recv_slice`'s own `Truncated` - the same `EMSGSIZE`,
detected before anything has been allocated for it. Behaviourally identical for
every packet the MTU admits, which is all the old 1500-byte temporary could
hold either.

**The frame pool: three allocations, one of them out of reach.** The three the
plan names for the network path are the TX build, the RX copy and the raw
temporary; the raw one went first (above), and the seam's two remain - now
served from a pool rather than freed (§10.4's last item, landed with the
instrument that preceded it). One known cost stays deliberately: virtio-drivers'
inner `TxBuffer` allocates per send *below* `NetDevice::transmit`, across a crate
whose `Transport` trait is not object-safe (AGENTS.md §5's trap), so it is out of
this change's scope and the before/after figure is attributed to the seam's two
Vecs only - nothing here claims more.

**The instrument landed first, as demanded.** `net/pool.rs` counts every frame
at exactly the two seam points the pool reuses - one relaxed add when a
frame enters `NicDevice::receive`, one when `FrameTx::consume` builds an
outgoing frame (Linux's `tx_packets` counts frames handed to the driver, so a
refused send still counts) - and the door is Linux's own `/proc/net/dev`, which
this kernel did not have until now despite what `docs/uapi/03` claimed. The
file carries Linux's exact column widths (measured off a live `lo:` row, pinned
by procfs's host render test), an honest zeroed `lo` row - loopback runs on
smoltcp's internal device and never crosses this seam, so zeros are the truth
rather than invented numbers - and one named divergence appended past the last
standard column: `mtu <n> pool=<hits>:<misses>`. `t-netstats` (default group,
both machines) sends a 256-datagram UDP burst to slirp's gateway and asserts
the deltas exactly - `tx_packets` +256, `tx_bytes` +256·(1400+42, the whole
Ethernet frame: measured against a diagnostic frame, 62 bytes for a 20-byte
datagram) - because one
datagram within the MTU is one frame through this seam; the datagrams-per-second
figure is printed for §10.4's before/after ledger and follows `t-pipecost`'s
"reported, not asserted" rule. The harness refuses a sweep that staged the
program but saw no `NETSTAT` line.

**The pool itself.** Sixteen slots per direction, each 2048 bytes - the width
the driver's own receive ring already pre-allocates, so a slot is one of the
buffers the machine proves it wants - filled once at `net::init` and bounded at
**64 KiB resident** worst case before the first allocation exists (§10's rule-4
discipline: no user steers count or width). A frame wider than a slot - an MTU
raised past 2034 by `ip link set ... mtu` - falls back to a fresh allocation and
is counted as a miss rather than growing the bound; an exhausted list does the
same, so a leak anywhere upstream degrades into "the pool earns less", never
into a wrong answer. `FrameTx::consume` puts back at the site of last use, right
after the driver has copied the frame out; the RX direction has exactly one
`Drop` guard, `PendingFrame`, and the guard travels *into* the token (`FrameRx`
holds it), so a frame's slot comes back from one place whichever way the frame's
life ends — the stack reading it in `consume`, or smoltcp taking the token pair
and dropping it unread. The pin is the
warm-up claim in `t-netstats`: a second identical burst must move `pool_hits` by
at least 256 - the counterfactual being `fits_pool`'s bound off by one, which
would make every full-MTU frame a silent miss and leave hits where they were. A
host test could not run that pin: `pool.rs` lives behind the machine-gated
`crate::net` (the trap at `net/socket.rs`'s tail note), so the policy is pinned
by the program that boots.

**The one bug the pool landed with, and why the machine went silent (measured).**The first draft's handover left the frame in `NicDevice::pending` and handed the
token a buffer it had emptied, so every `receive` after the first one returned a
token pair over an *empty* frame instead of polling the driver. smoltcp reads an
empty frame as "a packet was processed, try again" (`iface/interface/mod.rs`,
`socket_ingress`), and `Interface::poll` loops on exactly that answer - so `poll`
never returned, and it was running inside the stack's critical section, which on
this uniprocessor machine means the clock interrupt was off too. What the gate
saw: the sweep reached `t-epoll-socket`, printed its first four claims, and then
produced no further byte for fourteen minutes (QEMU alive, the serial file's
offset frozen, the guest's own idle loop never reached). Three changes came out
of it. The handover now *clears* the slot (`Option::take`), which is the part the
next `receive` depends on. The guard rides into the token as described above, so
both endings return the slot from one `Drop` and `FrameRx::consume` no longer
carries an explicit `put_rx` - fewer owners, not more. And a zero-length frame
from the driver now answers `None` ("nothing arrived") rather than a token, since
that is the same infinite loop entered from the other side; a fabricated empty
token pair is never a truthful answer. The claim that caught it is
`t-epoll-socket`'s: a parked `epoll_wait` woken by an ICMP reply, which cannot be
woken once the stack has stopped seeing frames.

**And the same program's burst failed for a second reason *and* a third, which took five probes to
separate.** The first was the shared-state leak above (the MTU), a real cross-test bug whose
fix stands. With the link at 1500 the burst's datagrams *still* never reached the seam, and the
remaining cause turned out to be a **property of the stack under test**, not a fault:
`smoltcp`'s `socket_meta.rs` carries `DISCOVERY_SILENT_TIME = 1000 ms` — a socket whose dispatch
failed because the neighbour was unknown is **skipped by the egress for a full second**, and the
probe it fired (one ARP request, which is exactly what the seam showed once per burst) is rate
limited with it. A burst that begins inside that second therefore sees every `sendto` refused
(`EAGAIN` since the split above, `EMSGSIZE` before it) while the socket's send buffer fills —
2000 refusals and four frames at the seam, which reads *exactly* like a stack that stopped.

What the probes eliminated, in order, each a temporary print at the seam and once again removed:

1. the MTU (the leak above — fixed, and the refusal persisted at 1500);
2. pacing (the same burst with 1 ms between *accepted* sends behaved identically — the pause was
   on the wrong side of the wedge);
3. the ARP exchange itself: the request is well formed (`sender=10.0.2.15 target=10.0.2.2`), the
   reply arrives (`opcode=2`, dst **our exact MAC** `52:54:00:12:34:56`, src `52:55:0a:00:02:02`,
   target `10.0.2.15`), and the frame is *consumed by smoltcp* (printed inside the token's
   `consume`);
4. the interface's state throughout: `ip_addrs=[10.0.2.15/24]`, `has_10.0.2.15=true`, `up=true`;
5. the socket the entry names: in the polled fabric (`home=Eth`), **bound** (`*:32768`, the
   implicit bind), and `can_send()` **true** — which is what finally closed the argument: with
   room and a fitting size, `PacketBuffer::enqueue`'s only remaining refusal is the split free
   window, i.e. the socket *holds* datagrams the egress is not taking, and the egress was not
   taking them because its own rate limit had not expired.

The fix is at the *test's* end, not the kernel's: `t-netstats` **primes the link** — one datagram
per turn until the *datagram's* own frame (not the 42-byte probe's) appears at the seam, bounded at
the silent time plus slack — and re-baselines the counters afterwards. The same measurement
corrected one of its claims: the seam counts the *whole* Ethernet frame, so the byte assertion is
`+42` (Ethernet 14 + IP 20 + UDP 8), not `+28` — read off a diagnostic frame that was 62 bytes for
a 20-byte datagram. `t-netlink` also puts the MTU back now, which is the rule the repo states for a
check that changes shared state. **What remains a named kernel gap** is the answer a caller gets
inside that second: `EAGAIN` where Linux's sender would queue the packet behind the ARP entry and
*succeed* (see `docs/uapi/` §5.2) — the honest refusal this kernel gives until the TX ring has a
completion event to wait on.

| 512 UDP datagrams through the eth0 seam | before (fresh `Vec` per frame) | after (pool serving slots) |
|---|---|---|
| datagrams/s, x86_64 TCG | `12138` over the whole run's window, `13204` over the first burst's | `12213` and `11768` over the whole run's window on two runs, `13515` over the first burst's |
| datagrams/s, aarch64 TCG | `20372` over the whole run's window, `22678` over the first burst's | `19807` and `19652` over the whole run's window on two runs, `22699` over the first burst's |
| `pool=` word | `0:1036` (x86_64) and `0:1100` (aarch64) — every frame a miss | **`1036:0`** (x86_64) and **`1098:0`** (aarch64) — every take of the run was a hit, the counts being the two bursts' 1024 plus each machine's priming and ARP frames |

**The rate pair is measured now, and it says no** — which the plan allowed for
("the numbers are allowed to say 'no'"). The *before* column is one `cargo xtask
test` run per machine with the pool reverted to the pre-pool seam (a fresh `Vec`
per frame, the patch reverted after the run); the *after* column is two green
runs of the same command: the same machine reports `12213` and `11768` for
identical code, a **3.9 %** spread, and the other `19807`/`19652`, 0.8 %. The
before figures sit *inside* the after column's own spread — on aarch64 the before
figure is even the higher of the two. So the pool buys **no** throughput at this
burst size: 512 frames' worth of 2 KiB allocations is a few hundred microseconds
against a burst whose rate is set by smoltcp and the virtio ring, and the
instrument cannot resolve a difference that small against its own jitter.

What the other two columns do establish, and what the change is actually worth:
the steady path **never allocates** (1036 takes, 1036 hits, no miss on x86_64),
and the allocation it replaces is bounded at **64 KiB resident** before the first
frame exists rather than whatever a burst of a user's datagrams would ask for
(§10's rule-4 discipline). The counterfactual comes free with the before run: with
the pool reverted, `t-netstats`' warm-up claim — "a warmed pool serves a
full-MTU frame from a slot" — **fails** (`pool=0:1036`, the first burst's hits
unchanged), which is the pin doing its job; the rate keeps being printed either
way, so the failing run still yields its number. Both figures come from operator
runs of the same command (`cargo xtask test`, whose report prints the
`self-test (netstat)` line this table quotes).

**And the send buffer's *blocking* half, measured whole on the host before it was built**
(§5.1 item 27). The gap was named in this section's own entry — a blocking UDP send answered
`EAGAIN` where Linux waits — and the reason it gave was that there is no event to wait on, because
the driver takes no interrupt for a TX completion. That reasoning is wrong about the *shape* of the
wakeup source rather than its existence: the stack's wake list is woken by **every** poll (the
background service task each tick, `irq_service` on a device interrupt), and a poll is exactly the
event that drains a socket's send buffer. The state was measured on a current host
(`.workbuddy/probe-udpblock*.c`, Linux 7.2.9) in the one shape that fills the buffer without root —
an **on-link neighbour that never answers ARP**, whose queue holds the first datagram and whose skb
keeps its `sk_wmem_alloc` charge, so the buffer fills behind it exactly as it would behind a slow
device, and the neighbour gives up about three seconds later:

| the host's answer, `SO_SNDBUF` at its floor (2048 asked, 4608 reported) | measured |
|---|---|
| 1400-byte datagrams accepted before the buffer is full | **3** (the fourth's truesize overruns the soft limit) |
| the fourth *blocking* send | parked **3072-3098 ms**, then **returned the datagram** |
| the *non-blocking* send at the same point | `EAGAIN`, latency 0.001-0.004 ms |
| a signal inside the park, handler with no `SA_RESTART` | `-1/EINTR` at **exactly the alarm's 300.0 ms** |
| the same, handler with `SA_RESTART` | the send **restarted** and completed at the neighbour's timeout |
| a datagram wider than the whole buffer | `EMSGSIZE` at **65508**, accepted at **65507** — the maximum UDP datagram, not `SO_SNDBUF` |
| a datagram wider than `SO_SNDBUF` but under the maximum | **taken** (5600 bytes into a 4608-byte buffer) — the soft limit is a limit a single datagram may exceed |

The signal measurement carries its own instrument lesson, which is why the pipe control exists: the
first probe used glibc's `signal()`, which defaults to `SA_RESTART`, so its "no restart" arm was
silently *restarting* and both arms returned success after the full park. A control — a blocking
write to a full pipe, whose interruptibility is textbook — answered `EINTR` at exactly 300.0 ms with
the same setup, which proved the delivery was sound and the UDP park really was restarting. The
corrected probe then measured `EINTR` at 300.0 ms for the UDP send too. **A wait that cannot be
interrupted is a wait that cannot be measured.**

The guest's pin is `t-udpblock`, and its numbers are the same shape on a smaller buffer: **2**
datagrams of 1400 bytes fit (the payload ring is 4 KiB), the non-blocking send is `EAGAIN` at
0.004 ms, the blocking send parks and is answered `EINTR` by a 100 ms alarm at **98.3 ms**, the same
send under `SA_RESTART` waits the bound out and answers `EAGAIN` at **3096.2 ms**, and a fresh socket
still sends afterwards. The bound is `DRAIN_WAIT_MS` (three seconds, the window Linux's own neighbour
resolution takes): past it the answer is `EAGAIN`, because smoltcp *keeps* a datagram its egress could
not dispatch — an unknown neighbour's silent second, a datagram the MTU cannot carry, a link taken
down — where Linux's device drops the frame and releases the charge. That is the one divergence the
pin encodes, and it is the direction that makes the park safe: its worst case is the behaviour this
pass replaced.

**The counterfactual** is the condition, not the park: `udp_room_for`'s payload-window half removed
leaves the metadata-only predicate that spun at 97% CPU on the host, and the guest pin's blocking
claim then answers `EAGAIN` at once instead of parking — which is the old behaviour, and is what the
`wait lasted the alarm's delay` claim catches.

### 10.5 Filesystem and Block Read Path — **PARTIAL** (the verifiable one landed; the disk-backed instrument now stands too)

**Why**: ext4's cache was a 512-slot direct-mapped array that thrashes on large
working sets, path resolution reads a directory block per component on a miss,
and each 4 KiB write leaves for the device as eight 512-byte synchronous
requests. Write-through stays: crash visibility is a deliberate semantic, and
only the read side is being tuned.

| Change | Where | State |
|---|---|---|
| Set-associative cache with victim selection | `vfs/ext4/cache.rs` | **done** |
| A **demand run** past the block being asked for, in one device call | `vfs/ext4/cache.rs`, `crates/blk`, `drivers/ahci.rs` | **done, measured** (below) |
| virtio-blk's half of the same: a wide `max_transfer` behind a reachability test that proves it | `drivers/virtio/{blk,mmio}.rs`, `crates/blk` | **done, measured** (below; the first machine's own figure is named) |
| virtio-blk **interrupt-driven completions** — the mask a device call holds | `drivers/kunit.rs` | **measured, re-scoped** (below): the cost is the device's latency, µs–ms per call here, and only one of the two machines' clocks can see it |
| virtio-blk **multi-request pipelining** | — | **dropped** (below): one call is already one transfer (`e228275`) and nothing above asks for two in flight |
| A SATA probe that ends in an answer instead of a budget | `drivers/ahci.rs`, `crates/ahci` | **done, measured** |

**The probe's cost, before and after.** Not a throughput change but the same
ledger, because it is a device wait that the boot pays: on a `q35` machine whose
built-in SATA controller has no disk on it, the gap between `virtio-blk: vda, ...`
and `blk: no data partition ...` in the boot log was **28.412 s** and is now
**0.001 s**, measured on the same image copy with a timestamped serial log
(`-machine q35,sata=off` gives the same 2 ms, which is what named the probe as the
cost). The healthy path is unchanged and still measured - a command on a port
that really has a device retires at try 0-2 - and `t-ahci` is the pin that says
so. `docs/notes/` §7.3 holds the register dump, why a count of MMIO reads is still
the shape rather than a tick deadline, and the two gaps that were left open there
and are now closed.

**The cache.** Four-way set-associative, least-recently-used within a set, over
fast-lazily-allocated slots - and the set count kept at the 512 that the original
`ls` measurement justified, so the change is *only* about collision and not about
size. A block's set is `block % CACHE_SETS`; an empty way takes it first, and
otherwise the way whose block was asked for longest ago, dated on every access
including hits (dating only misses would make the policy "first loaded" rather
than "least recently used"). Slots no longer allocate their 4 KiB buffer at mount
- a slot takes its buffer the first time it holds a block - which is what makes
`CACHE_SLOTS` 2048 total slots (8 MiB, if a filesystem ever touches all of them)
affordable, and which also removes a fixed 2 MiB per mount that used to be paid
whether or not anything was read.

The instrument is a host unit test, not a clock, because a cache's quality is a
*count*: a mock `BlockDev` that counts reads per block and answers with a pattern
derived from the block number, so a slot handed back for the wrong block fails as
content rather than as slowness. The numbers are the same on every machine and
under TCG or KVM, which is why they are worth writing down:

| Trace | direct-mapped (before) | 4-way LRU (after) |
|---|---|---|
| four blocks a set-count apart, accessed round-robin, 4096 accesses | **4096 reads** (0% hits) | **4 reads** |
| a 2048-block sequential scan | 2048 reads | 2048 reads |
| one hot block read between 2048 scan blocks | <=5 reads of it | <=5 reads of it |

The first row is why the ways are there: a working set of four blocks that share
a set - four groups' inode tables, say - made a direct-mapped cache *not a
cache*, with every access evicting what the next one wanted. The second and third
rows are the traces that must not regress, and the second is also the honest
limit of this change: a sequential scan has no reuse in it, so no cache can help
it, and the two runs read the same 2048 blocks either way. 63 ext4 host tests
cover the driver through the new cache, including a byte-for-byte read of a large
file and a mount of an image the host's `mke2fs` made.

**Why the other two are not done, and it is one reason between them.** They
cannot be measured *well*, and they are each other's prerequisite. `BlockDev::read_at`
is capped at one 4 KiB page per call by its own contract - "drivers bounce through
a single page-sized, physically contiguous buffer" - so readahead cannot ask the
device for fewer, larger reads; it can only issue the same reads earlier, and
this kernel's block driver is synchronous and polled, so "earlier" overlaps
nothing. Readahead becomes worth having exactly when item 3 gives the device
asynchronous completions it can be pipelined behind, and until then it is a
prefetch into a cache that the reader is about to miss anyway.

The measurement gap was the real prerequisite, and it is worth stating plainly:
the test machine used to boot the conformance suite *without a filesystem on its
disk*, so no `t-*` program exercised the ext4 read path at all - the numbers
above existed only because the cache policy was pure enough to drive from a host
test. Either item would have been landed blind. **That gap closed with the entry
below**; what remains true is the ordering argument above — readahead's best
number still needs pipelining to overlap behind.

**The prerequisite landed (2026-10-11): the disk-backed instrument, in three parts.**

* **A filesystem on the test disk.** `xtask/src/disk.rs::build_ext4_image` lays
  one 84 MiB ext4 volume by hand (`mkfs.ext4 -d`, journal and metadata checksums
  off — the shapes this kernel's driver reads) carrying three files:
  `pattern.bin`, 64 MiB of `(block + byte) % 251`; `cached.bin`, 4 MiB of the same
  generator; and `hello.txt`. **Two generated sizes, because the instrument asks
  two questions**: 64 MiB is eight times the cache's 2048 slots, so a sequential
  scan of it must miss every block; 4 MiB is *half* the cache, so it fits, and a
  second pass over it must cost no device reads at all. The first draft asked the
  second question of the first file, where the answer is arithmetically forced to
  be "no" — the gate measured it (`cold=16387 warm=16384`, i.e. a warm pass that
  re-read every block because it could not be held), and `cached.bin` is that
  measurement's fix. The fixture's identity moved with it: `existing_or_build` now
  keys on the file's **size** rather than on its existence (the rule
  `ensure_sata_image` already used), so a tree holding the older two-file image
  rebuilds rather than answering a `t-diskbench` that expects a file inside it.
  The SATA backing file grew with the fixture —
  `SATA_TEST_BYTES` is now 96 MiB, sparse on the host so an untouched sweep pays
  nothing for it —   because a copy longer than the device's capacity is refused
  sector by sector (`ahci`'s `check_span`), which would read as a broken driver.
  Two pins moved with that number rather than being left to rot: `t-ahci`'s
  capacity claims, which now name 96 MiB (`98304` in `/proc/partitions`' 1 KiB
  column, and the read one sector past sector 196608 answering empty), and the
  disk's *state* — a sweep lays it fresh before every boot
  (`disk::reset_sata_image`), because `t-ahci`'s claims about the bytes *around*
  what it wrote ("and is still zero") are claims about a disk nothing else has
  touched, and the backing file outlives the run. `t-ahci`'s scratch sectors
  (4000 and 6000) sit inside the region this instrument's filesystem later
  occupies, harmlessly: every reader of that filesystem lays its own copy first,
  and the fresh lay is what keeps one run from reading the last one's writes.
  It rides each machine's own payload channel to the guest at
  `/dev-sda-fs.img`: an appended member of the x86_64
  test initramfs (`selftest::append_test_members`'s `extra` — absolute member
  name, which is how the boot's tar reader spells an absolute path), and a
  staged entry on the aarch64 sweep medium (`run_disk_sweep`'s `entries`, named
  from its already-rooted tree, which grows `SWEEP_DISK_BYTES` by the fixture
  when it rides). One guest path on both machines, so one pair of programs runs
  on both; the two spellings are pinned equal by
  `both_payload_channels_lay_the_fixture_at_one_path`. **The path is at the root
  and not under `/dev` where it started**: both boots mount a `devtmpfs` over
  `/dev`, so the first draft's file was shadowed the moment that mount landed —
  measured on the pair's first gate run, where the harness staged the fixture and
  both programs reported `ENOENT`. The harness's pin grew the rule with it (the
  member is an absolute name *outside* `/dev`).
* **`t-ext4-mount` copies it onto `/dev/sda` through the block door** — 1 MiB
  chunks through `write(2)`, which is the AHCI write path at volume, before the
  read path is judged — then mounts `-t ext4 /mnt/ext4test` and checks the tree
  back: `hello.txt`'s text, the pattern file's size, and its first two blocks'
  bytes against the generator. Content, not just speed. It leaves its mount
  standing, and nothing depends on that either way: the mount table is per-task
  (§6.3), the two programs of this instrument are separate processes, and *the
  runner walks `/bin/t-*` in name order* — `t-diskbench` is staged **before**
  `t-ext4-mount`, which the first gate run of the pair measured when
  `t-diskbench`'s mount found a disk still made of zeros. So each program lays
  its own copy through one shared loop
  (`user/tests/src/lib.rs::lay_ext4_fixture`) and mounts its own view:
  order-independent set members rather than a sequence, with `t-diskbench` still
  checking what the path is actually *on* (`statx`'s `st_dev` against the root's)
  and reporting which case it ran in.
* **The device-read counter, and the door it comes through.** `crates/blk` gained
  `Counted`: a wrapper `add_disk` puts around every registered device, counting
  *calls* per disk with relaxed atomics. The wrapper is at the seam rather than
  in either driver because every path to storage passes through a registered
  `BlockDev` — a cache miss, a partition window, a `dd` on `/dev/sda` — and a
  count taken anywhere else counts less than the machine does. `part::Window`
  deliberately wraps the counted device, so a partition's accesses are the
  disk's too (pinned: `a_window_over_a_counted_device_shares_the_count`; the
  counterfactual pin for the wrapper itself is
  `the_wrapper_counts_calls_and_forwards_every_access` — without those two tests
  a constant zero could pass every claim below).
  The counters reach userland through `uname(2)` on an **armed boot only**: the
  command-line word `mpos.diskstat` (`board/flow.rs::DISKSTAT_WORD`) arms the
  boot task's UTS namespace, `release` then answers the private magic
  `0x6d706f7300000001` (`abi::DISKSTAT_OSRELEASE_MAGIC`) and `version` carries
  `<name>:<reads>:<writes>,` rows rendered live at call time. A *private* ABI
  word, disclosed in `docs/uapi/01-syscall-table.md` §1.0's sub-gap table: no
  real kernel's osrelease is a number, nothing that parses the field can collide
  with it, and there is no other door — `/proc/partitions` is column-parsed by
  tools like `fdisk`, and this kernel has no sysfs attribute store a
  `/sys/class/block/*/stat` could live in. Arming is a boot-step fact with no
  syscall door, so userland can neither trigger nor spoof the protocol; the flag
  rides the namespace copy, so a `unshare(CLONE_NEWUTS)` inside an armed boot
  still reads real numbers (pinned by `an_armed_diskstat_protocol_survives_the_copy`).
  The renderer — magic word, `#1 ` prefix, one `<name>:<reads>:<writes>,` row
  per registered disk, live at call time — lives in `syscall::linux::abi`, where
  the host suite reaches it:
  `the_rows_name_every_registered_disk_and_track_live_reads` pins the door's
  *shape* against mock devices before any QEMU has run.
* **`t-diskbench` is the number.** A cold sequential pass over the 64 MiB file,
  then a cold pass and a warm one over the 4 MiB file, with the counters read
  before and after each through the door. Its claims: the big file's cold pass
  ≥ 16384 device reads (its own blocks — 64 MiB is eight times the cache's 2048
  slots, so every block misses whatever the policy does); the fitting file's
  cold pass ≥ 1024 (its blocks, which fit); and its warm pass == 0 (**the
  regression pin the cache policy has on the machine**, not only against the
  mock). Both stay in the **default** sweep for that reason — they are exact
  counts, not timings, as stable as the mock's were. The content check rode
  `t-ext4-mount`.
* **The harness reads the numbers back.** `t-diskbench` prints one line,
  `DISKSTAT sda cold=N warm=M`; `sweep::diskstat_readings` greps it out of the
  serial log (last word per disk wins) and both report paths print it beside the
  verdict — rule 6's before/after figure, automated. An armed run whose
  instrument left no reading behind is *refused* (the same rule the kernel's own
  suites are held to: a silent absence must not read as coverage). And the
  arming follows the staged list itself: each arm asks its own selection — the
  set it has already computed by the time it writes a boot line — whether
  `t-diskbench` is in it (`selftest::diskstat_for_staged`), and only then puts
  the word on the line; move the name out of the default group and the fixture,
  the arming and the reading all follow automatically, leaving no half-state.
  `sweep::in_default` answers the same membership question from `tests.toml`
  alone, and `run_tests` refuses a sweep configuration that ever leaves either
  instrument out of a plain run. The word the sweep writes and the word the boot
  reads are pinned equal by
  `the_diskstat_word_matches_the_kernels_constant` (xtask cannot name the kernel
  crate, so the pair is checked as text).

**What the instrument measured on its first green runs** (`cargo xtask test` — x86_64
`pass=166 fail=0`, aarch64 `pass=175 fail=0`): `DISKSTAT sda cold=16387 warm=0 smallcold=1024` on
**both** machines — the cold pass over the 64 MiB file costs **16387** device reads, exactly its
own 16384 blocks plus **three** metadata reads (the superblock, the extent tree and the inode's
own block, and no more: the layout is named in this section and the count agrees with it), and the
*fitting* file's warm pass costs **0**, which is the cache policy's pin on the machine rather than
against the mock. The pair the readahead work will be measured against is therefore (16387, 0): a
64 MiB sequential read is worth 16387 device reads today, and every name in `vfs/ext4/` that
changes that number has an instrument that will say so. What the numbers *cannot* say yet is how
much of the 16387 a readahead or a pipelined request would remove — that needs the change, and the
instrument is what makes it countable.

With the instrument standing, the two deferred items keep their order: readahead
(`vfs/ext4/`) can now be counted before and after on a real card, and virtio-blk
pipelining (`drivers/virtio/blk.rs`) likewise — each still wants the other for
its *best* number, but neither is blind any more.

**Before either is written: readahead cannot move the count, and the reason is the
transfer bound.** `cold=16387` is
already `16384 + 3` — every block of the file once, plus three metadata reads —
so a sequential pass has no read left to remove; readahead fetches blocks
*earlier*, not fewer, and on a full pass the prefetch is consumed exactly once.
And `Counted::read_at` counts **calls**, with `crates/blk`'s `PAGE_SIZE` (4096)
doc'd as "the largest buffer `read_at`/`write_at` is handed in one call" — one
ext4 block. So the count is `blocks + metadata` by construction, and the only
thing that changes it is one device transfer moving **more than one block**:
prefetch a run *and* hand it to the driver in a single call.

That is nearer than the trait's bound suggests, because the bound is the block
layer's, not the drivers'. `src/drivers/ahci.rs` already moves up to
`BOUNCE_PAGES * PAGE_SIZE` = **128 KiB in one command** — the bounce is a *list*
of pages and the PRDT is a scatter-gather table — and its own comment calls that
"a bound on one command, not on a request". virtio-blk hands its whole buffer to
the crate in one call
(`request_read` → `add_notify_wait_pop(&[req], &mut [data, resp], …)`); whether
the crate's queue splits that by `seg_max` internally is the one thing to check
before the bound is raised.

So the item is re-scoped rather than started: **a wider transfer** (ext4 fetching
a contiguous run into the cache in one call, with the trait's bound raised to a
size each driver really serves in one command), not readahead alone.

**What was implemented on top of that, and what it measured.** The item is a
*demand run*. `crates/blk`'s trait gained `max_transfer()` — the width a device
moves in **one transfer**, defaulting to `PAGE_SIZE` so a driver that says nothing
keeps the old contract (and both wrappers a mounted filesystem stands behind,
`Window` and `Counted`, forward it, which the first run of this change proved is
not a formality). `AhciDisk` answers its `MAX_TRANSFER` (128 KiB, the PRDT's
`BOUNCE_PAGES`). `vfs/ext4/cache.rs` gained `slot_run`: load `[block, block+want)`
in one device call into a scratch buffer bounded by `RUN_BLOCKS` = **16 blocks**
(64 KiB, the tighter of the two bounds), then scatter it into the per-block slots,
so every block of the run is resident and the next request for one of them is a
hit rather than a second read. `Inode::read_at` asks for the run its own request
needs — and `contiguous_run` walks the extent tree *in memory* to find how far the
device blocks are consecutive, so a file stored in scattered blocks takes exactly
the path it always did.

It is a **demand** run and not a readahead: `want` is what the caller's own request is about to
touch, so a random read of one block asks for one block and pays nothing, and
there is no heuristic to get wrong. A readahead past the request (Linux's
`file_ra_state`) would buy nothing here — the pass is full-rate either way — and
would add device reads to a random pattern that §10.5's instrument cannot see.

The numbers, `cargo xtask test`, **identical on both machines** (which is what a
count is for):

| instrument | before | after |
|---|---|---|
| `cold` — a 64 MiB sequential read | **16387** | **1027** (16384/16 plus the three metadata reads) |
| `smallcold` — the fitting file's cold pass | **1024** | **65** |
| `warm` — the fitting file's second pass | 0 | 0 |

**…and the figure was not reproducible from the commit that recorded it (found
2026-10-12, while extending the same width to the other transport).** The change above
landed in `3be89aa`, whose message and this section both say `AhciDisk` answers its
`MAX_TRANSFER` — and the committed tree does not: `impl BlockDev for AhciDisk` carries no
`max_transfer` at all, so the SATA disk answered the page default and the measured
filesystem's run width collapsed back to one block. The *wrappers*' forwarding landed
(`Window`, `Counted`, both host-pinned); the *driver's* override did not, and the first gate
run afterwards is therefore the counterfactual rather than a confirmation: x86_64
`=== SUMMARY pass=168 fail=1`, `=== FAIL t-diskbench exit=1`, with
`FAIL: the cold pass reached the device for the file's blocks, in runs`,
`FAIL: and in runs of at least eight blocks each` and
`FAIL: and its blocks came off the device in runs, because they fit the cache only now`, and
the instrument's own line reading `DISKSTAT sda cold=16387 warm=0 smallcold=1024` — the
"before" column of the table above, exactly. The override is restored in `20697b7` — one
method answering `MAX_TRANSFER` — and `t-diskbench`'s three run claims are its pin: red
without it, green with it, on both machines. The same experiment had been run *deliberately*
as that change's own counterfactual and reverted; what the committed tree showed was the same
number arriving on a tree where nobody had done it. The lesson is the one this file's trap
list keeps meeting in a new shape — a number written down is a claim about the tree that
*made* it, and `3be89aa`'s own "`pass=169 fail=0`" described a working copy whose `ahci.rs`
hunk never reached the commit, so the commit and its record disagreed for a day while nothing
in the tree could tell them apart. `cargo xtask test`, run on the committed tree, is what
told them apart.

**The first run of this change measured nothing at all, and the reason is the
lesson worth keeping: a wrapper is a driver to its callers.** The chain a mounted
filesystem sees is `ext4 → Window → Counted → AhciDisk`. `Window` forwarded
`max_transfer` (it must — the measured filesystem lives on a *partition*), and
`Counted` did not, so the disk's 128 KiB promise was replaced by the page default
one layer *below* the filesystem and `cold` came back as 16387. Nothing about the
cache was wrong; the width had been answered by a wrapper that had never been
asked. Both wrappers' forwarding now has a host test in `crates/blk`, and a
partition's window over a wide disk is one of them.

`t-diskbench`'s claims moved with the measurement, which is rule 1's carve-out:
"the cold pass reached the device for the file's blocks" was `cold >= 16384` — one
call per block, the behaviour this change removes — and is now "**in runs**"
(`0 < cold < 16384`) plus "runs of at least eight blocks each" (`cold <=
16384/8`). The exact figure is *reported* rather than asserted, because how wide a
run the device serves is the kernel's business; what the harness pins is that a
run happened at all and that no block was read twice. The counterfactual is the
claim's own proof: with `AhciDisk::max_transfer` put back to the page default for
one run, `cold` returns to **16387** and all three run claims fail.

**The root disk joined it (2026-10-12) — and the prerequisite this section first named for
it was already met.** An earlier draft of this entry said the root disk (`vda`) still read
one block per call and that the fix was a **physically-contiguous multi-frame bounce**, on the
argument that a descriptor carries one physical address so a wide buffer has to be contiguous —
"which a kernel `Vec` of 32 pages is not". Reading the crate and this kernel said
otherwise, twice over, and both are checkable in-tree:

* **The crate does not split a wide call.** `VirtIOBlk::read_blocks` is
  `queue.add_notify_wait_pop(&[req.as_bytes()], &mut [buf, resp.as_mut_bytes()], …)`, and each
  slice element becomes exactly **one** descriptor (`queue.rs`'s `InputOutputIter` →
  `Descriptor::set_buf` → `Hal::share`). So the open question §10.5 left here — "whether the
  crate's queue splits that by `seg_max` internally" — is answered no: one call is one
  request is one transfer, and `Counted`'s call count is not flattered by a wide buffer.
* **A kernel `Vec` of 32 pages *is* contiguous here.** `mm::heap`'s slab sends every
  allocation past its largest class (`class_of` → `MAX_CLASS`, 2 KiB) to `alloc_large` →
  `alloc_pages(order)` — a contiguous frame block, one order, addressed at `phys_to_virt`.
  The run scratch `vfs/ext4/cache.rs` reads into is a 64 KiB `Vec`, i.e. exactly that.
* **And what the device actually needs is weaker than contiguity — it is the direct map.**
  `HalImpl::share` answers with `mm::virt_to_phys(ptr)`, a *subtraction* from the hhdm
  offset. The direct map is affine, so any VA range inside it *is* one physical run with
  the right base address. A bounce would therefore have copied every read of the busiest
  disk — the first machine's root — to satisfy a condition this kernel's memory already
  satisfies.

So the change is the width plus the test that says when it holds, and not a bounce:
`crates/blk::direct_map_run` walks the buffer's pages through the machine's own page tables
(`hal::translate`) and answers true only when each page maps to *its own* address minus the
offset and is not user-accessible; `src/drivers/virtio/mod.rs::device_reaches` is that test
against this kernel's tables, and both transports ask it before handing a buffer down.
`BLK_MAX_TRANSFER` (32 pages = 128 KiB, the SATA disk's own figure, so the two disks a
machine can have answer one number) is what `max_transfer` now returns on both. The pointer
test it replaced was `ptr >= hhdm_offset()`, which is *necessary and not sufficient*: an MMIO
window or the kernel image is mapped in the kernel half, above the offset, and not *at* it —
so a wide buffer there would have been handed to the device at an address computed from the
wrong thing — a page's worth of it today, 128 KiB of it on the wide path. The PCI
transport keeps its 2 KiB stack bounce for the callers that need it (the syscalls hand user
pointers down; a kernel stack is itself a legal single descriptor), and the MMIO transport —
which has no bounce and never had a test at all — now refuses such a buffer by name instead
of panicking one layer down in `virt_to_phys`'s assert.

**Where it is measured, and where it is not.** A sweep's `vda` is not the same disk on the
two machines, and that decides the pins. The first machine's sweep boots QEMU's virtual-FAT
ESP (`qemu.rs`'s `Boot::Esp`): a read-only VFAT image, mounted by nothing, read three times
at boot by `find_filesystem` — so the PCI transport's wide path has **no counter in its own
sweep**, and its own figure is a named gap (the behaviour is live under `cargo xtask run`,
whose root is the installed ext4 image on `vda`; what is missing is a boot that counts it).
The second machine's sweep boots the sweep medium's ext4 root on virtio-**MMIO**, read by
every `exec` — and `t-diskbench` already reads it in the pass that lays the fixture:
`/dev-sda-fs.img` is a *file on that root*, so copying it to `/dev/sda` is a cold sequential
pass over the boot disk. Nothing new is read for the measurement; the counter is read around
the copy, and the figure comes out on a second line (`DISKSTAT vda bootcold=N bootblocks=M`),
reported by `xtask` beside the first.

| instrument | with the page default (the counterfactual) | with `BLK_MAX_TRANSFER` |
|---|---|---|
| `bootcold` — the boot disk's pass over the fixture file, 21504 blocks of which 17420 reached the device | **17420** (one call per block fetched) | **1093** |
| `cold` / `smallcold` / `warm` — the SATA disk's three passes, which this change does not touch | 1027 / 65 / 0 | 1027 / 65 / 0 |

The two `bootcold` figures are each other's check: at one block per call the pass costs one
call per block it *fetched* (17420 of 21504 — the rest of an 84 MiB file was still in the 8 MiB
cache when the copy began), and at 16 blocks per call the same 17420 blocks cost
`17420/16 + 4` metadata reads = **1093**, which is what the wide run measured. Its claims are
the cold pass's shape: `0 < bootcold < bootblocks`, and the discriminator, `bootcold <=
bootblocks/8`. The counterfactual run (`BLK_MAX_TRANSFER` put back to `PAGE_SIZE`, aarch64
`=== SUMMARY pass=177 fail=1`) is what says the second claim is the one that decides: it reads
`FAIL: and in runs of at least eight blocks each on vda` beside a *passing* `in runs` claim,
because one call per block still covers fewer calls than the file has blocks — a wide run is
what the tight claim asks for, and the loose one only says a cache was involved. The content
half is not this program's: the fixture it copies *off* the boot disk is mounted on
`/dev/sda` and its files' bytes are checked against their generator (`t-ext4-mount`, and this
program's own reads), so a wide read that returned the wrong run would fail on content rather
than on a count.


**The last row, measured and then re-scoped (2026-10-12): what a device call masks,
and which machine can see it.** `VirtIOBlk::read_dma` and `AhciDisk::read_at` both
hold the port inside `critical_section::with`, and the crate's wait is
`while !self.can_pop() { spin_loop(); }` — so for as long as a call takes, the machine
runs nothing else and no timer fires. `src/drivers/kunit.rs`'s boot suite
(`block.what_a_read_costs_the_clock`) measures that in the machine's own clock, on
every registered disk, at `Phase::Arch`:

| instrument | x86_64 (KVM) | aarch64 (TCG) |
|---|---|---|
| control — 12 ms of spinning, nothing masked | `3/3 ticks` | `3/3 ticks` |
| the reachability walk over the 64 MiB buffer | `61 µs` | `711 µs` |
| one 64 MiB read of `vda` (virtio) | `1 ms` (1 ms masked), `1/0 ticks` | `8 ms` (8 masked), **`2/2`** |
| one 64 MiB read of `sda` (SATA) | `3 ms` (3 ms masked), `1/0` | `42 ms` (41 masked), **`11/10`** |

Three findings, and the third is why the row moves rather than lands:

* **The mask's size is the device's latency, and nothing else.** 64 MiB is the
  largest single allocation the frame allocator has an order for (`MAX_ORDER`), so it
  is the largest one call can be; QEMU answers it from the host's page cache in
  1–42 ms. What the kernel *asks* for is far smaller: a filesystem run is 64 KiB in
  one call, 1/1024 of that span — **~8 µs of mask on the first machine and ~41 µs on
  the second**, against a scheduling quantum of 4 ms.
* **A mask moves the tick count only past the second period it spans, and on one
  machine it moves nothing at all.** The first draft of the case measured a perfect
  `3/3` reading 4 MiB pieces (0.75 ms each): the PIT is periodic, so a masked window
  *defers* a period — one interrupt arrives when the mask ends — and `m` periods
  masked cost `m − 1` ticks. Then the aarch64 arm measured `11/10` across a 41 ms
  mask, which is not noise but the machine's timer: `arch/x86_64/timer.rs`'s `tick`
  is `TICKS.fetch_add(1)`, so a mask is *lost from the count for ever*, while
  `arch/aarch64/timer.rs`'s adds the periods its own counter says have passed, so it
  **catches up**. §7.3's "a deadline in ticks would never expire" is still true
  *inside* the mask on both machines — no handler can run in there — but what the
  mask leaves behind is machine-shaped, and the case asserts the loss where it can be
  observed and prints the figures where it cannot.
* **So the row is a structure change with no number behind it here.** The mask is
  real and it is proportional to the *device's* latency: µs per filesystem read on
  these machines, and seconds in the one place the wait is unbounded — §7.3's probe
  on an empty controller, **28.412 s** in one mask, fixed there by ending the probe
  early rather than by not masking. A machine whose device is slow (real silicon, or
  a boot whose reads are not served from the host's cache) pays it in a unit a task
  feels, which is what §10.6 fixed for the serial: ~5 ms a line, with a ring the
  device interrupts. So the row's halves land differently. **Interrupt-driven
  completions** are a structure change whose justification is the unbounded case and
  whose pin is `drivers/kunit.rs`'s claim — dormant on these machines while every
  call is under a tick period, and firing the day one is not. And **multi-request
  pipelining is dropped**, not deferred: one call is already one transfer
  (`e228275`), the filesystem above is synchronous, and readahead — the only caller
  that would want two requests in flight — was re-scoped away in `9aa4f18` for the
  same reason (it cannot move the count).

The instrument is a boot suite and not a `t-` program for a reason the same work
measured: a userland window around a block read is mostly the work *around* the
device call — the devfs block window's allocation and zeroing, the copy out to the
caller's buffer — and those parts are preemptible, so a sibling task runs during
them. On a 32 MiB raw read that is 30–50 % of the window, the same order as the mask
itself, and a window that has to beat its own confounds by a factor is not a pin.

**The first machine's figure landed (2026-10-24): the PCI transport is counted in its own
sweep now, over the disk it had there all along.** The named gap above closes not by
changing what a sweep boots but by asking `t-diskbench` a fourth question: it looks for the
registered disk that is neither its fixture's (`sda`) nor the root's, and makes one bounded
raw pass over it — 64 MiB at 1 MiB `pread`s, capped so the pass stays bounded whatever the
device's size. The machine whose root is the boot archive has exactly one such disk: the
boot medium itself, the virtual-FAT ESP, which arrived over virtio-blk **PCI** and which
this boot mounts nothing on and reads for nothing but its own probes. That is the
transport the other two readers never reach — the ext4 runs are the AHCI driver's and
`bootcold` is the second machine's MMIO — and a raw pass over it is pure transport, because
no filesystem sits on it and no cache is in the way: the block node's sector window is a
kernel `Vec` in the direct map, and every 1 MiB window is one `read_dma`. The measured
figure, `cargo xtask test` on x86_64: `DISKSTAT vda sparecold=64 spareblocks=16384` — 64
calls for 64 MiB, one call per window, reproduced exactly across runs, with the SATA disk's
own figures unmoved beside it (`cold=1027 warm=0 smallcold=65`). The claims are the cold
pass's shape (`0 < calls < blocks`, and `calls <= blocks/8`), so a PCI transport that fell
back to the 2 KiB bounce — `device_reaches` answering false, or the walk broken — reads
`sparecold` at four times the block count and fails both; the second machine, whose only
virtio disk *is* its root, prints the skip line and claims nothing; and the figure rides
out of the sweep beside the other two readers (`sparedisk_readings`, printed as `vda
(spare disk): sparecold=64 of 16384 blocks`). What stays unmeasurable in a sweep is the
*production* root's own figure — the installed ext4 image on `vda` under `cargo xtask run`
is a different boot — but the transport's width is no longer a named gap: the
counterfactual is the bounce, and the claims catch it.

Gate: x86_64 `pass=205 fail=0`, `mpos-kunit 32 passed, 0 failed`, exit 0. The aarch64 arm
builds and claims nothing: its sweep hangs mid-run on this host today (§6.23), on HEAD
with this tree stashed as well.

### 10.6 Serial Output — **DONE**

**Why it mattered**: UART transmit used to be polled with the console lock held;
on real hardware a 60-byte line costs ~5 ms of machine-wide stall (QEMU's
emulated 16550 never blocks, which is why it does not show here). The kernel's
own hot paths log nothing, so this only bit on the day they did - which is what
a `bat` over a minified stylesheet turned into a machine that looked dead.

| Change | Where |
|---|---|
| A transmit ring drained by the TX-held interrupt; `write` only enqueues and parks on a full ring, re-arming before *every* park so a write larger than one drain burst cannot sleep unannounced | `drivers/serial.rs`, `console.rs` |
| The framebuffer console's serial **mirror** is best-effort: it hands bytes to the ring and returns, so a host terminal that cannot keep up drops bytes rather than throttling the window or the console mux | `console.rs` (`serial_mirror_now`) |

### 10.7 Sub-Tick Sleeps — **LANDED on the first machine** (the second's clock is the named gap)

**CLOSED, and the landing's own debt named and paid (2026-10-24, §6.24).** The
"second's clock is the named gap" line above outlived its truth by one day: the
second machine's clock was never the problem - the *second clock* was. The
deadline conversion this landing added at the park's boundary left a dozen
callers naming deadlines against the tick **counter**, and a park that answers
one clock a deadline named on the other is a self-sustaining spin that freezes
the tick counter itself (the aarch64 sweep freeze; mechanism, closure and the
both-machines gate figures in §6.24). The rule the landing's shape implied and
the tree now enforces: a wait is named on the clock its park answers -
`since_boot_nanos` - and converts at construction. Every sub-tick figure in
this section's tables survives the fix unchanged (the x86_64 numbers are the
same park, one door narrower).

**The wake latency was filed here and did not belong here; it is fixed.** Two
measurements said a *woken* task - not sleeping on a deadline, but parked in
`block_until` and made runnable by an event - did not resume for what looked
like two ticks:

| measured | before | after | instrument |
|---|---|---|---|
| `wait4` reaping a child, whatever that child owned | ~8.0 ms, unchanged when the child's 32 MiB address space is taken away | 1.8 µs | `t-forkcost` (§10.2) |
| a 64 KiB pipe handover between two processes | 8.0 ms per chunk, 7 MiB/s | 6.1 µs per chunk, 10 168 MiB/s | `t-pipecost` (§10.4) |

Neither was a sleep rounding to a tick, which is what this section is for: the
woken task was *ready* and the picker was not choosing it. §10.2 has the cause
(the boot task's `park()` halting the core while runnable), the fix and the
numbers, including the probe reading 604 of 640 wakes at exactly one tick.

**The bound is written, in the one place it is honest.** `t-forkcost` asserts
that *both* of its `wait4` rows - each a 200-round average - are under one clock
tick, which is the claim the wake fix owed. `t-pipecost` still only *reports*,
deliberately: its figures are throughputs, and a bound that holds under KVM can
flake under TCG.

**Why the rest of this section existed**: the tick was the smallest unit a sleep
could name. At 250 Hz that is 4 ms, and `nanosleep(1 ms)` took 4 ms because there
was no way to wake a task between two PIT interrupts. Linux's `nanosleep` ends at
the instant the caller named; a program that polls on a 1 ms timeout paid four
times its own interval here. Reading time was never the gap - `since_boot_nanos`
already resolves below the tick from the TSC - the *wake* was.

**What landed, and in which unit.** The scheduler's deadline is now a
**nanosecond** one. `Scheduler::now`/`Sleeper::deadline` moved to
nanoseconds-since-boot, `advance` is handed `hal::since_boot_nanos()`, and the two
public doors are `sched::sleep_ns` (relative) and `block_until_nanos` (absolute);
`sleep_ticks` and `block_until_deadline` keep their tick-denominated signatures
and convert at that one boundary (`hal::ns_per_tick()`), so no caller had to
re-derive its own arithmetic. `nanosleep(2)` takes `block_until_nanos`, which is
also what makes its `rem` an honest nanosecond remainder instead of a tick count
(`timespec_from_ticks`, now unused, was deleted rather than left dead).

**The wake is a resource of the machine, so it is a capability.** `hal::Clock`
gains `SUB_TICK_WAKE` (default `false`) and `arm_wake_ns(Option<u64>)` (default: do
nothing). The scheduler's `arm_next_wake` runs on every turn, after `advance`:
it takes the **minimum** of the sleep heap and, when that instant falls before the
next tick boundary, asks the machine to wake it there; otherwise it cancels. Taking
the minimum every turn is what keeps the coalescing trap shut - an arm that merely
pushed a new instant could postpone somebody else's deadline, and *recomputing* the
earliest is idempotent by construction.

**On the first machine** that is the local APIC's count-down timer, on vector
`0x40` (`lapic.rs`): one-shot, divide-by-one, unmasked, at a count measured in
**timer counts per millisecond**. `arch/x86_64/idt.rs`'s `apic_timer` handler does
no bookkeeping of its own - it EOIs the APIC and calls `kernel::preempt()`, so the
scheduler's next turn is the one place that reads the clock, moves it and wakes
whoever is due. The PIT stays armed underneath it: a lost one-shot costs a tick's
latency and cannot stall the machine.

**The calibration is in the unit the deadlines are written in.** The timer is
started from its maximum with its interrupt masked, and both its count and
`since_boot_nanos` are sampled twice; the two deltas give counts per millisecond
directly, so no third rate (the TSC's, the PIT's) can disagree with the timer at
the instant that matters. The first draft measured counts per TSC cycle and scaled
by the clock's own TSC rate - two calibrations that each drift being one more way
to arm the wrong instant than this needs. Three further guards: a measurement under
4 000 counts, or with no clock movement, is not trusted; a rate outside
1 MHz..4 GHz is refused; and a failure is retried on later turns (three failures
before the timer is written off), because one miss under load is not evidence.

**The bug this section cost, and how it read.** Some boots had *no* one-shot at
all and every sample measured one tick. It was found by the pin refusing to be
satisfied: `t-sleep-subtick` reported `25/50 inside one tick` - but that count was
the pin's own threshold at 4 000 µs, and the samples were 3 804..4 179 µs, i.e.
*all* tick-aligned. The cause was that the APIC was never enabled for delivery:
`IA32_APIC_BASE` bit 11 and the spurious-vector register's bit 8 were not set, so
the timer counted down and never raised its vector, and one failed calibration
latched "not ticking" for the rest of the boot. `lapic::init` now enables delivery
(base MSR bit 11, TPR 0, SVR `0x1FF`) and the IDT carries a spurious-vector
handler, because enabling delivery must not turn a race into an unhandled vector.

**Measured, same image, before → after** (x86_64 under TCG, `cargo xtask test`):

| trace | before | after |
|---|---|---|
| fifty `nanosleep(1 ms)` | **200.0 ms** (median 4 001 µs; samples 3 804..4 179 µs) | **52.7-58.6 ms** (median 1 042-1 126 µs; 50/50 under half a tick) |
| one `nanosleep(25 ms)` | 28.4 ms (seven ticks) | 25.1-25.3 ms |
| `nanosleep(200 ms)` interrupted by a 20 ms alarm | `EINTR` at ~20 ms | `EINTR` at 17-19 ms, `rem` 181-183 ms, `rem + elapsed` = 200 ms within a tick |

The pin is `t-sleep-subtick` (the fifty-sample distribution, the longer wait that
must not be cut short, and the interrupted sleep's remainder). Its counterfactual
is exact: with `SUB_TICK_WAKE` back to `false` the same program is
**`FAIL t-sleep-subtick`, 8 runs of 8**, with a median of 4 001 µs.

Gate: x86_64 `pass=200 fail=0 (213 built)`, `mpos-kunit 32 passed, 0 failed`,
exit 0 - twelve consecutive green runs after the two fixture fixes below. The
second machine compiles the whole change (`cargo build --release --target
aarch64-unknown-none`, clippy clean) but its sweep could not boot on this host
(`egl-headless,gl=on` needs a DRM render node and this machine has no `/dev/dri`),
so its gate has not been re-run for this change and no aarch64 figure is claimed.

**Two fixtures had to be repaired, and both are the kind that could hide a bug.**
They are part of this landing because the accurate clock is what exposed them.

* **A pipe with two readers.** `t-exit-group`'s sibling-child fixture wrote one
  pid into one pipe, and both the leader and the parent read it - so whichever read
  ran first consumed the four bytes and the other side saw end-of-file. The gate
  measured *both* outcomes on the same tree: `child=9 waitpid=7 status=0x100` when
  the parent won (the leader read nothing, took `_exit(1)` and its exit_group is
  what handed the child on - so the fixture silently never exercised
  `exit_group`'s walk) and `child=0 waitpid=7 status=0x700` when the leader won (the
  two claims failed). It is two pipes now, the parent's copy written first, so the
  leader's exit_group cannot cut the write the parent needs off. The fixture is
  deterministic: `leader=7 child=9 waitpid=7 status=0x700` on every run.
* **A budget written as iterations × a sleep.** Six programs waited in
  `for _ in 0..400 { ...; sleep(5 ms) }` loops, which was eight seconds of wall
  clock only while a sleep was a whole tick. `t-exit-group`'s waits are now a
  wall-clock `WAIT_MS = 8_000` (its own comment says so); the same shape remains in
  `t-clone-exit`, `t-pidfd`, `t-recvmsg-iov`, `t-tid-dir` and `t-waitid`, which pass
  today but are the next place this will bite. A fixture budget is wall clock, and
  it must not be spelled in the clock's granularity.

**Named, and not claimed**: the second machine has no sub-tick wake - its sleeps
still round up to a tick, `SUB_TICK_WAKE` is `false` there by default, and
`t-sleep-subtick` sits in its `gaps` rather than being weakened for it (`docs/`
§13 records what a machine's own timer would take). The periodic timer scans stay
tick-quantised on purpose: `check_itimers` and `check_posix_timers` compare
`hal::ticks()`, so a POSIX timer's expiry still rounds up to a tick - only waits
with a *deadline* became sub-tick. `timerfd` and the TCP retransmit pacing still
sleep in ticks (`sleep_ticks(1)`), which is a tick of latency, not a wrong
answer. And a one-shot is one core's: nothing here is SMP-aware, as nothing else
is.

### 10.8 The build's own cost: an incremental rootfs, measured before and after — **LANDED** (xtask)

The change: `cargo xtask rootfs` used to answer *any* touch of `rootfs.toml` with a full reinstall
— GPT, both filesystems, `apk add --initdb` for thirty-odd packages over the network — because the
staleness rule was one mtime comparison (`utils::newer(rootfs.toml, stamp)`). Editing one package
name paid that whole bill. The mechanism now has three parts, each its own commit:

* **Two content hashes in the stamp** (`xtask/src/rootfs.rs`, `StampRecord`). The shape keys
  (mirror, branch, repos, filesystem, disk geometry) decide a fresh lay; the package lists plus the
  two overlay trees' digests decide the delta side. A v1 or unparseable stamp resolves towards the
  full install — the direction that costs one rebuild and cannot be wrong about what the image holds.
* **A per-machine download cache**: every `apk add` carries `--cache-dir build/apk-cache/<arch>`,
  and the index refresh is conditional — install from the warm cache first, retry with
  `--update-cache` only when resolution fails. An offline host with a warm cache installs.
* **The diff itself**: read `lib/apk/db/installed` (the tree's own truth, not a trusted record),
  add what the configuration names and the tree lacks, remove what the previous build *owned* and
  the configuration no longer names. A package in the tree but in no stamp belongs to nobody's
  diff — that is the safety property, pinned by `a_delta_installs_only_the_new_names_and_removes_only_owned_ones`.

**Measured before.** (To be filled by the operator's run on this machine: `time cargo xtask rootfs`
on a current image after appending one name to `rootfs.extra_packages` — the old path reinstalls
everything; the new path prints `Installing 1 package(s): <name>`.) The gate ran green around the
change without these numbers, so this entry records the mechanism and leaves the figure named rather
than invented — AGENTS §3 rule 6 asks for before/after, and the honest form of "not yet measured"
is this line, not a made-up number. The counterfactual pins are the host tests above; the timing
figure lands here on the first run that takes it.

### 10.9 The 9p share's message size — **LANDED** (client-side, measured)

QEMU printed this on every boot that mounted the host share:

```
qemu-system-x86_64: warning: 9p: degraded performance: a reasonable high msize
should be chosen on client/guest side (chosen msize is <= 8192).
```

It is QEMU's own threshold on the **client's** `Tversion` ask, and the client asked
8192 (`src/vfs/p9.rs`'s old `MSIZE`). Every 9p read and write chunks by the
negotiated size (`msize - header`), so the number is a request count: a 64 MiB
read is 8192 requests at 8 KiB and 512 at 128 KiB.

**Measured on the same wiring** (`.workbuddy/p9bench-boot.sh`: a throwaway
initramfs that mounts the share and runs `dd if=/mnt/share/bench.bin
of=/dev/null bs=1M count=64`, QEMU 11.1.2 under TCG):

| client asks | server answers | warning | 64 MiB read |
|---|---|---|---|
| 8192 | 8192 | yes | 0.255365 s — 250.6 MB/s |
| 131072 | 131072 | no | 0.071335 s — 897.2 MB/s |
| 131072 | 131072 | no | 0.064902 s — 986.1 MB/s |

The negotiation was read off QEMU's own trace (`-trace enable=v9fs_version
-trace enable=v9fs_version_return`), not inferred from the warning: the return
carries the granted size, and it equals the ask. The same trace showed the
server is not the limit at 128 KiB.

**The mechanism** is one constant in the wire half, `PROPOSED_MSIZE`, used for
`Tversion` and as the starting value of the per-mount `msize`; the server's
answer still overwrites it, as the protocol allows. The per-request buffer is
allocated at this size, which bounds the cost by the constant and not by
anything a user steers.

**A measurement worth keeping about QEMU**: its fsdev has **no `msize` option**
in 11.1.2 — `qemu-system-x86_64 -fsdev local,id=x,path=/tmp,security_model=none,help`
lists none, and passing one is `Invalid parameter 'msize'` (QEMU refuses to
start). The familiar `-virtfs ...,msize=131072` advice found on the wiki is for
configurations whose fsdev accepts it; here the client's ask is the only knob.
The first attempt at a "server-side" comparison was invalid for exactly this
reason, and the giveaway was a serial log that had not been rewritten.

**Pin**: the host unit test
`vfs::p9::tests::the_proposed_msize_is_above_the_degraded_line` — the request is
a constant, so the test is the only place a regression can be caught without a
boot; the negotiation and the throughput are this measurement's. Counterfactual:
with `PROPOSED_MSIZE` back at 8192 the test is red and the warning returns.
