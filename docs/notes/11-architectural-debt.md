# §11 Phase 11: Architectural Debt — **AUDIT DONE; STEPS A–G LANDED, AND V-1/V-4/V-13 CLOSED** (D.2b evaluated & rejected as no-value; surviving `drivers ↔ vfs` pair is its own finding)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 11: Architectural Debt — **AUDIT DONE; STEPS A–G LANDED, AND V-1/V-4/V-13 CLOSED** (D.2b evaluated & rejected as no-value; surviving `drivers ↔ vfs` pair is its own finding)

Everything above this phase is about *features*: what the kernel cannot yet do. This phase is
about the *shape* of the kernel that does them, and it opens with a measurement rather than an
opinion, because "bad architecture" is not a finding until something counts it.

`ARCHITECTURE.md` is the companion document: it draws the module graph, the boot sequence and
the syscall path, and its §9 mirrors the findings below. This section is the work list.

**The audit itself was written without editing a file** — every finding below was read out of the
tree, and each says where. What has landed since is marked on the finding and on the step: **A**
(the dead modules and the leftover trace), **E** (the seven pure leaves as crates) and **F** (the
gate's written criterion and the check for it). B, C, D and G are open.

---

### 11.0 How the audit was made

Reproducible in a few seconds, and worth re-running after any structural change:

```
# module-level edges: map every file to its top-level module, collect crate::<mod> refs
python3 - <<'PY'
import os,re,collections
files=[os.path.join(r,f) for r,_,fs in os.walk('src') for f in fs if f.endswith('.rs')]
top=lambda p:(lambda q:q[0][:-3] if len(q)==1 else q[0])(os.path.relpath(p,'src').split(os.sep))
edges=collections.Counter()
for p in files:
    for m in re.finditer(r'crate::([a-z_][a-z0-9_]*)',open(p,encoding='utf8',errors='ignore').read()):
        if m.group(1)!=top(p): edges[(top(p),m.group(1))]+=1
for (a,b),w in sorted(edges.items()): print(f"{a:12} -> {b:12} {w}")
for (a,b) in sorted(edges):
    if (b,a) in edges and a<b: print("CYCLE",a,b)
PY

# god files
find src -name '*.rs' | xargs wc -l | sort -rn | head -12

# orphan modules: a file under arch/x86_64 that mod.rs does not declare
cat src/arch/x86_64/mod.rs
```

The three numbers that came out of it, re-measured after Step A (§11.2):

| Measurement | Result |
|---|---|
| Kernel size | 80 239 lines across 114 `.rs` files (plus `boot/` 1 225 in 4 files and `xtask/` 4 396) |
| Host unit tests | 501 `#[test]` in `src/`, of which 498 run in the host build - the other three are `net/socket.rs`'s, behind the gate V-11 names - and 29 in `xtask/` |
| Mutual module pairs (a cycle of length 2) | **21**, among the eleven modules that appear in one: `arch`, `console`, `drivers`, `mm`, `net`, `pci`, `rand`, `sched`, `syscall`, `vfs`, `uevent` |

The audit was published with 79 095 lines across 116 files, 491 + 25 tests and *23* pairs; the table
above is the re-measured state after Step A. Re-running the script corrected two numbers: the pair
count was one too high (`ARCHITECTURE.md` §6 prints 22 entries before Step A; Step A takes it to 21
because one pair came from a file the compiler never saw, §11.1.7), and the script matches *text*, so
an edge can be a doc comment. Measured with `//` comments stripped — who names whom in code — the
tree had **20** mutual pairs: `uaccess → syscall` is a mention in a doc comment, and `vfs → uaccess`
with it. The ratchet holds the code number — **19** since Step C, which took `sched → net` out and
with it one pair — and its three baselines (re-measured 2026-10-02) are in §11.2 Step G.

That last row is the finding. At module granularity the kernel is one strongly connected
component: there is no layer that can be compiled, tested or refactored on its own. Every
other item below is a *cause* of that number, which is why the fixes are ordered to reduce it
rather than to make any single file prettier.

---

### 11.1 The findings

Severity is about cost-to-change, not about user-visible bugs: none of these is a defect a
program can observe today.

#### 11.1.1 — V-1: the `Inode` downcast chain (severity: high) — **CLOSED**

`src/vfs/mod.rs:453–546` gave the central VFS trait a method per concrete device and network
type:

```rust
fn as_fb(&self) -> Option<&crate::drivers::virtio::gpu::FbDev> { None }
fn as_evdev(&self) -> Option<&crate::drivers::virtio::input::EvdevNode> { None }
fn as_uinput(&self) -> Option<&crate::drivers::uinput::Uinput> { None }
fn as_drm(&self) -> Option<&crate::drivers::drm::DrmCard> { None }
fn as_vsock(&self) -> Option<&crate::net::vsock::VsockSock> { None }
fn as_unix(&self) -> Option<&crate::net::unix::UnixSock> { None }
```

(The list grew to nine while the audit was being written — `as_syncobj_fd`, `as_sync_file` and
`as_packet` are the same shape and arrived with the DRM sync-object and AF_PACKET work.)

This is the textbook abstraction leak: the VFS — supposed to know only that a node has a name, a
kind, bytes and an owner — names six device drivers and three network socket types, and the *syscall*
layer reaches through these to drive a device. Adding a seventh device type means editing the VFS
trait, so neither the VFS nor anything depending on it can compile without `drivers` and `net`. It
was the largest single contributor to the 23-cycle count.

**Closed by Step B1 (§11.2)**: all nine are gone and `src/vfs/mod.rs` names `crate::drivers::` and
`crate::net::` zero times. One type-erased handle replaced them — `core::any::Any` as a supertrait
plus a single upcast — so the question "is this node an `FbDev`?" is asked by the layer that knows
what an `FbDev` is, and adding a tenth device type touches no VFS code.

**What it deliberately did not fix:** the `drivers ↔ vfs` mutual pair stands. `vfs` still names
`drivers` 17 times and `net` 4, from `vcmux`'s render/drm calls, `procfs`/`sysfs`'s PCI config reads
and `p9`'s virtio-9p device — each a *different* finding (V-4's remaining half, the PCI-config and 9p
doors), with its own step. V-1 was the downcast cause and it is closed.

#### 11.1.2 — V-2: device ioctl ABI lives in the syscall layer (severity: high)

`src/syscall/linux/fs.rs` is 6 311 lines and is the second-largest file in the tree. It is not
only path-carrying file calls; it is also the switchboard for every device's ioctl contract:

| Device | Where its ABI is parsed |
|---|---|
| fbdev (`FBIOGET_*`, `FBIOPAN_DISPLAY`, `FBIO_WAITFORVSYNC`) | `fs.rs` around `:3561` |
| RTC (`RTC_RD_TIME`, `RTC_SET_TIME`) | `fs.rs` around `:3677` |
| DRM/KMS (`drm_ioctl` dispatch) | `fs.rs` around `:3751` |
| uinput (`UI_SET_*`, `UI_DEV_SETUP`, `UI_DEV_CREATE`) | `fs.rs` around `:4001` |
| TTY, block, evdev, termios | `fs.rs` throughout |

A driver change therefore lands in the syscall layer, and the driver cannot state its own
contract next to the code that implements it. The dispatch routing counts make the split
explicit: 135 of the 247 names are handled by `fs.rs`, 50 by `proc.rs`, 50 by `posix.rs`, 12 by
`abi.rs`.

#### 11.1.3 — V-3: the scheduler does other subsystems' cleanup (severity: high) — **CLOSED**

`sched/runtime.rs` is 6 397 lines — the largest file in the tree — and on task teardown it
reaches into four unrelated subsystems:

```rust
crate::net::socket::close_sockets_at_exit(&files);
crate::vfs::inotify::close_at_exit(&files);
crate::drivers::virtio::input::close_at_exit(&files);
crate::drivers::uinput::close_at_exit(&files);
```

plus `vfs::locks::release_process` and `vfs::mount::fs_id_of_inode` in the same path, and
`binfmt::shebang`, `vfs::read_file`, `vfs::tty::global` and `vfs::tty::by_dev_id` in the exec
path. The scheduler owns *tasks*; it should not know that inotify exists. Each new
per-open-file resource adds a line here, and each line makes `sched` unbuildable without `net`,
`vfs` and `drivers`.

**Closed.** Step C turned the exit path into a registry (`sched::exit`) each subsystem registers into
from `main.rs`, zeroing `sched → net`/`vfs`/`drivers` there. The edge it left was `execve`'s
`FD_CLOEXEC` pass naming the two input drivers; it now walks a *second* registry, `sched::exit`'s
cloexec list, into which `drivers::virtio::input` and `drivers::uinput` register the same hooks. The
two lists are deliberately not merged — `execve` tells fewer subsystems than `do_exit` because the
process lives on — so the distinction is carried in two typed lists rather than a flag. `sched →
drivers` is 0, `drivers ↔ sched` collapses, and **mutual pairs fell 12 → 11**, the last ratchet drop
of Phase 11 and the precondition for un-gating `sched` for aarch64 (§9.1 stage 6h-adjacent work).
Pinned by a host test that an exit hook stays silent under `run_cloexec` and vice versa, and by
`cargo xtask test`'s 142 user tests still closing CLOEXEC descriptors on `execve`.

**A related `sched → syscall` reduction (the `abi` leaf).** That edge (Step C/F's
`BASELINE_LOWER_LAYER_TO_SYSCALL` row, recorded at 10) was about *ABI values* the scheduler needs but
does not dispatch — the default `umask`, the `clone` flag bits and the resource-limit model. They are
arch-neutral (asm-generic `CLONE_*`/`RLIMIT_*` carry the same number on x86_64 and aarch64), so they
moved to a new `crates/abi` leaf that `sched` names downward and `syscall::linux::{nr,abi}` re-export
rather than redefine. `MMAP_BASE` moved to `crate::mm` (an address-space fact beside `USER_LIMIT`)
with a per-arch value: x86_64 keeps `0x7f00_0000_0000`, aarch64 gets one derived from
`paging::USER_END` (`1 << 48`) as *this* kernel's choice, to be measured against Linux's aarch64 mmap
base at stage 7. `sched → syscall` fell 10 → 1 (the ptrace/stop block's `abi::uregs::COUNT`), and
**it is 0 now** (§13.2): that field count is the machine's (`hal::Contexts::ABI_REG_COUNT` — 27 on
x86_64, 34 on aarch64), the block's `PTRACE_*`/`SYSCALL_TRAP` words moved into the same leaf, and the
`target_arch` gate that stood in for the dependency is gone — so the edge and the `sched ↔ syscall`
pair are empty on both machines. The leaf is `#![no_std]`, host-tested (`clone` values and the
`setrlimit` soft/hard/root rule pinned), and added to the host-test crate list in
`xtask/src/selftest.rs` and the `ARCHITECTURE.md` §1 graph.

#### 11.1.4 — V-4: the terminal emulator and console live inside the VFS (severity: medium) — **CLOSED** (the `vfs ↔ console` pair is broken; emulator + VC vocabulary + `vtmode` + active-console number in the `term` leaf; see §11.2 Step D)

`crates/term/` (the `mpos-term` crate) holds the cell grid, the key handling and an embedded 4 KiB `vga8x16.bin` font.
`src/vfs/tty.rs` (2 359 lines), `src/vfs/pty.rs`, `src/vfs/devpts.rs` and `src/vfs/vcmux.rs`
implement the console, and `src/console.rs` — the kernel's own logging console — reaches *into*
the filesystem layer to draw:

```rust
crate::term::with_screen(active_grid(), |g| g.feed(bytes));
crate::drivers::video::render::notify();
```

The direction is wrong: a terminal emulator is not a filesystem. `/dev/tty` being a node is a
filesystem concern; how a cell is painted and which of six virtual consoles is active is not.

#### 11.1.5 — V-5 and V-9: user-pointer validation owned from below (severity: medium)

`drivers/drm.rs:595,741` calls `crate::syscall::buffer_ok(...)`, and `net/socket.rs` /
`net/unix.rs` call `crate::syscall::buffer_ok` and `err_of` dozens of times. A driver validating
a *user* pointer is an inversion — the driver is below the syscall layer in every other respect
— and it exists because the helper happens to live there. The helper itself is fine; its home
is not.

The re-audit at the AF_PACKET/sound landing measured the inversion's bulk differently: most of the
~110 below-`syscall` references were not the two helpers but the **errno constants** — `net/*` (100+),
`sched/*` (17), `vfs/epoll.rs`/`vfs/timerfd.rs`, `drivers/virtio/sound.rs` (7) and `uevent.rs` each
reaching up for a minus sign. That half is **closed**: the table lives at the crate root as
`crate::err` (`src/err.rs`, re-exported through `syscall` so the layer's paths are unchanged), and
`uevent`'s second upward edge — `self_cred` reading the task table — moved to
`net::socket::task_identity`, an allowed `net→sched` edge, with `uevent::bind` taking the preferred
port id as a parameter so the bus stays host-testable. What *remains* of V-5/V-9 is exactly
`buffer_ok`/`err_of`: 24 `err_of` calls in `net/socket.rs`/`net/unix.rs`, 2 `buffer_ok` calls in
`drivers/drm.rs`, plus `net/*`'s pointer checks. B3 below is that move.

#### 11.1.6 — V-6: a temporary trace in the dispatch hot path — **DONE**

`syscall/linux/mod.rs::trace_msg` was self-labelled:

> TEMPORARY (udevd worker-channel hunt): ... Remove once the site is known.

It fired on every failed `sendto`/`recvfrom`/`sendmsg`/`recvmsg` and printed up to 60
`[msgtrace]` lines from inside the dispatcher. The hunt it belonged to is recorded as finished
in §4.1's udevd note, so it was leftover scaffolding standing in the one path every syscall
crosses. **Removed, and the four arms are plain calls again** (§11.2 Step A).

The class is worth watching, because it happened twice: the same hunt later left a `[recvtrace]`
wrapper around `net::socket::handle_recvmsg` (the original renamed `recvmsg_inner`, a logging body
around it, and `console.rs`'s `DMESG_CAP` temporarily raised so the ring would not evict the lines).
It was still there when the session ended, with the bug found by reading eudev's source instead: the
failing `recvmsg` was `EINVAL` for a *zero-length iovec* (§4.1). **Removed**, and the wrapper
collapsed back into one function; the per-task cost of the shape is that a diagnostic in the
dispatcher or the socket layer is easy to leave, and neither is in anybody's diff when the session
ends.

#### 11.1.7 — V-7: orphan and duplicate machine modules — **DONE**

| File | Was |
|---|---|
| `src/arch/x86_64/serial.rs` (119 lines) | **not declared in `arch/x86_64/mod.rs`** → never compiled. A near-duplicate of `drivers/serial.rs` (324 lines), same doc header |
| `src/arch/x86_64/qemu.rs` (23 lines) | Not declared → never compiled. **Byte-identical** to `drivers/qemu.rs` |
| `src/arch/x86_64/pci.rs` | Declared, compiled, **referenced nowhere**. A stale earlier copy of `drivers/pci.rs` (discovery-only, one function per slot, bus 0) |
| `src/arch/x86_64/rtc.rs` vs `src/drivers/rtc.rs` | Both live and both used — but the names collided and the split was not obvious from either name |

Three modules named `pci`, two named `rtc`, two named `serial`, two named `qemu`; two of the
twelve were dead and one was a duplicate, which is how a reader learns to distrust the tree.

**All three files are deleted** with the module declaration; `drivers::rtc` is renamed
`drivers::rtc_dev` (the node), leaving `arch::x86_64::rtc` as the clock that owns the name. The
`console.rs` module header now names `drivers::serial`.

*One number fell out of the deletion.* §11.0's extraction reads every file under `src/`, including
the two orphans the compiler never saw: the `arch <-> vfs` pair existed **only** because
`arch/x86_64/serial.rs` mentioned `crate::vfs`. Deleting it takes the count 22 → 21 — an audit by
text search measures the tree, not the build, which is the same lesson V-7 is about.

#### 11.1.8 — V-8: the aarch64 configuration points at a missing file — **DONE**

`.cargo/config.toml` set `-C link-arg=--script=arch/aarch64/linker.ld`; only
`arch/x86_64/linker.ld` exists. The target could not link. §9.1 calls aarch64 "under
development", which understates it: there is no linker script, and `src/arch/mod.rs` says the
modules will be declared "when they are written rather than now".

**The profile is gone** (`[target.aarch64-unknown-none]` in `.cargo/config.toml` and the target in
`rust-toolchain.toml`), with a comment saying what it was and where the port's real prerequisites are
(§9.1). Re-adding it is one line next to a linker script that exists: the configuration no longer
advertises a target that cannot be built. Writing the linker script now would have been a claim about
a port nobody has started, and this step is subtraction.

#### 11.1.9 — V-10: the handoff ABI is written twice (severity: accepted)

`boot/src/handoff.rs` and `src/boot/handoff.rs` are separate definitions of one record, and
`BOOT_STACK_CANARY` is spelled out in `src/board/x86_64.rs` as well as in the loader. This is deliberate
— the loader is a UEFI application on a different target and cannot link the kernel crate — and
it is documented on both sides. Recorded so it reads as a decision rather than an oversight.

#### 11.1.10 — V-11: a host test module behind a never-host-compiled parent (severity: low) — **DONE**

`src/lib.rs` gates `pub mod net` on `target_os = "none"`, so the module is absent from every
host build — while `src/net/socket.rs` annotated a test module
`#[cfg(all(test, not(target_os = "none")))]`, i.e. "run me on the host". The annotation was only
ever evaluated in a build that did not include the file, so those three tests had never run.
`net`'s doc comment says its tests "are the conformance programs that run on the machine",
which is true of the rest of `net` and was false of this file.

**Settled by Step F, and the reading that settled it was of what the three tests pinned rather than
of how to make them run.** They were `send`/`recv` on a `socketpair`, the `sendmsg`/`recvmsg` shapes on
the same channel, and its two refusals (`EISCONN` for a destination, `EAGAIN` for `MSG_DONTWAIT` on an
empty one) — every one pinned, line for line and in more detail, by `user/tests/src/bin/t-sockmsg.rs`,
a program the sweep *does* boot (its header names the caller it exists for: sudo's monitor channel).
The module is deleted, the pins are where they always were, and the criterion and its mechanical
check are §11.2 Step F's.

#### 11.1.11 — Sizes, for the record

| File | Lines |
|---|---|
| `src/sched/runtime.rs` | 6 397 |
| `src/syscall/linux/fs.rs` | 6 311 |
| `src/net/socket.rs` | 3 642 |
| `src/vfs/ext4/mod.rs` | 3 391 |
| `src/syscall/linux/proc.rs` | 3 039 |
| `src/syscall/linux/abi.rs` | 2 903 |
| `src/drivers/drm.rs` | 2 371 |
| `src/vfs/tty.rs` | 2 359 |
| `src/vfs/procfs.rs` | 1 996 |
| `src/vfs/mount.rs` | 1 681 |
| `src/main.rs` | 1 625 |

Long files are a symptom here, not the disease: `ext4/mod.rs` is long because a filesystem is
large, and splitting it would buy nothing. `sched/runtime.rs` and `syscall/linux/fs.rs` are long
*because each holds several subsystems' concerns*, which is what 11.1.2 and 11.1.3 are about.
That distinction is what the plan below is built on: split by responsibility, not by length.

#### 11.1.12 — V-12: the in-flight refactor, and the green baseline it was blocking (resolved)

This item was written while the tree did not build: an in-flight refactor (the `user/abitest`
programs moving into `syscall/` and `integration/` subdirectories, plus edits across `vfs/ext4`,
`vfs/tty.rs` and the syscall layer) left `cargo test -p mpos --lib --target
x86_64-unknown-linux-gnu` **failing to compile** — `Tty`/`TtyOutput` unresolved in `vfs/tty.rs`
and a borrow error in `vfs/ext4/super.rs:501` — so no step below could begin until it finished,
because every acceptance criterion here is "the host tests still pass".

**It is finished and committed, and the baseline is green:** `cargo xtask test` reports `498
passed` host unit tests, `29` from xtask, `=== SUMMARY pass=120 fail=0` from the guest suite and
QEMU exit `33`. The refactor is what the batch of commits ending at this one carries: the suite
split into two tiers, plus the session's kernel work (seccomp's filter mode, the terminal's window,
ramfs link counts, the ext4 host tests, a cross-task user copy that could reach a supervisor page,
and the boot task's park that cost a clock tick on every wake). Step A may begin; its own acceptance
criterion is checkable because of this run.

---

### 11.2 The remediation plan

Ordered so each step is independently verifiable, behaviour-preserving, and leaves the machine
bootable. Every step states its acceptance criterion as something a test can decide.

The dependency direction to converge on (also in `ARCHITECTURE.md` §9):

```
drivers, net, blk, pci, rand, uevent   ->  may use arch, mm, sched, sync
      vfs, sched                       ->  may use mm, arch, sync, blk, pci
     syscall                           ->  may use vfs, sched, mm, net, drivers
       main                            ->  may use everything
```

Two hard rules: **nothing below `syscall` names `syscall`**, and **nothing outside `vfs` names
the VFS's concrete device types**.

---

#### Step A — Subtract: dead modules, the leftover trace, the phantom target (V-6, V-7, V-8) — **DONE**

*No behaviour change. Do it first: it is a deletion, and it removes the clutter that makes the
rest of the audit hard to read.*

| Change | Where |
|---|---|
| Delete `src/arch/x86_64/serial.rs`, `src/arch/x86_64/qemu.rs` (uncompiled) and `src/arch/x86_64/pci.rs` (compiled, unreferenced) | `src/arch/x86_64/` |
| Delete `trace_msg` and its call site from the dispatcher | `src/syscall/linux/mod.rs` |
| Either add `arch/aarch64/linker.ld` or drop the `aarch64-unknown-none` rustflags and target, so the configuration stops naming a file that does not exist | `.cargo/config.toml`, `rust-toolchain.toml` |
| Disambiguate the colliding names: `drivers/rtc.rs` → `drivers/rtc_dev.rs` (the *node*), leaving `arch::x86_64::rtc` as the clock; `drivers/qemu.rs` becomes the only `qemu` | `src/drivers/` |

**Acceptance:** `git grep 'arch::x86_64::serial\|arch::x86_64::qemu\|arch::x86_64::pci'` returns
nothing (the one stale doc-comment mention in `console.rs` is corrected); the host test suite and
a boot are unchanged apart from the absence of the `[msgtrace]` lines; `.cargo/config.toml` no
longer references a nonexistent path.
**Risk:** the only real risk is deleting something referenced through a macro. Grep first; for
two of the three files the absence of a `mod` declaration is proof enough.

**Done, and what the run said.** All four rows landed, one commit, no behaviour change. The three
files and `pub mod pci;` are gone; `trace_msg` and its four call sites are gone, so the socket arms
are plain calls again. The aarch64 profile went from **both** files rather than the linker script
being written (§11.1.8). `drivers/rtc_dev` is the rename, done with `git mv`, and its module header
says why the name carries `_dev` (`arch::x86_64::rtc` is the clock). `git grep` for the three old
paths returns nothing; the host suite is still **498 passed, 29 in xtask**; `cargo xtask test` ends
`=== SUMMARY pass=120 fail=0` and QEMU exit **33**, checked by running it. The one number that
moved is the pair count, 22 → 21 (§11.1.7: the extraction reads files the compiler never saw).

---

#### Step B — Give devices a seam the VFS can hold without naming them (V-1, V-2, V-5) — **DONE (B1, B2, B3)**

The largest step, and the one that pays for the others: it removes the largest cause of the
23-cycle count.

**B1 — Replace the nine `as_*` downcasts with one type-erased handle. — DONE.** The plan offered a
`vfs::DevNode` carrying an `Arc<dyn DevOps>` with `read`/`write`/`ioctl(cmd, arg)`, installed at boot,
or a single `as_any`/`downcast_ref` pair over `core::any::Any`. **The second was taken.** `DevOps`
does not fit the nine: six are device downcasts the syscall layer uses to reach an ioctl *the device
already owns* (B2 gave every family its own `ioctl`), and the other three are `net` recovering its own
`UnixSock`/`VsockSock`/`PacketSock` from an `Inode` it minted — not an ioctl question, so a `DevOps`
covering them would be a god-trait. Worse, `dev().ioctl(...)` *reorders the dispatcher*: every device
would answer `ENOTTY` for every family it does not own instead of the dispatcher routing by family
first. That is the plan's own named top risk, "this touches every device path at once", and it buys
nothing the downcast does not.

The shape actually taken, in `src/vfs/mod.rs`:

```rust
impl dyn Inode {
    pub fn as_any(&self) -> &dyn core::any::Any { self }
}
pub trait Inode: Send + Sync + core::any::Any { ... }
```

Two properties make it nearly free. `Any` is implemented for every `'static` type already, so
none of the tree's 52 `impl Inode for` blocks grew a method — and it could not have been a default
body on the trait, because coercing `&Self` to a trait object needs `Self: Sized`, which a `dyn
Inode` is not. And the body is a plain trait upcast (stable since Rust 1.86), so it is the
vtable's own supertrait entry rather than a lookup. Call sites read
`inode.as_any().downcast_ref::<FbDev>()`, which answers `Some` exactly where the `as_*` method it
replaced answered `Some` — the same `TypeId` comparison the old override (`Some(self)`) and
default (`None`) made, just moved from the trait into `core`.

Migration followed the plan — one device per commit, fbdev first and DRM last — seven commits: fbdev,
evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the DRM card's three node types (`as_drm`,
`as_syncobj_fd`, `as_sync_file`) together. Each deleted its `as_*` method and `impl` overrides, so the
ratchet fell as it went: `vfs/mod.rs -> drivers|net` 9 → 8 → 7 → 6 → 5 → 4 → 3 → 0, and the constant
in `xtask/src/depgraph.rs` is now 0. Every commit got a bare-metal build, fmt, clippy on both targets
and the full gate; five of the seven have their device's conformance program in the default sweep
(`t-fbdev`; `t-evdev`/`t-ioctl`; `t-unix`/`t-unix-dgram`/`t-unix-scm`; `t-drm`/`t-drm-args`/
`t-drm-lease`/`t-drm-sync`; `t-prime`). Two do not: AF_PACKET's is `t-rawicmp`, which `tests.toml`
puts in `external` (run by name, `--groups external`, green); AF_VSOCK has **no** conformance program
— a test boot deliberately gets no host vsock bus — so its guarantee is the type check plus the
equivalence above, and saying so is better than implying a test ran.

**Acceptance — met.** `crate::drivers::` and `crate::net::` do not appear in `src/vfs/mod.rs`;
`cargo xtask depgraph` prints `vfs/mod.rs -> drivers|net 0/0` and `Within the ratchet: no edge has
appeared and no count has grown.`; and the full gate on the finished tree (2026-10-02, after all
seven device commits) is:

```
Unit tests: the kernel and its leaf crates (host)   528 passed (kernel) + 161 in the twelve
                                                   leaf crates + 47 xtask = 736 passed,
                                                   0 failed
self-test guest: === SUMMARY pass=140 fail=0 (140 built)
self-test guest: 10 of 150 user tests left out by tests.toml
self-test guest: exit 33 (33 is the clean debug exit)
```

Two of the original criteria are **not** met, deliberately: `wc -l src/syscall/linux/fs.rs` did not
drop from B1 (it *rose*, by the imports that now name the device types — the 212-line drop was
B2's, and a downcast is not a line count), and the mutual pair count stayed 18 *then* (D.2a later
took it to 17 — see Step D), because `drivers`
and `net` still name `vfs` and `vfs` still names them from `vcmux`/`procfs`/`sysfs`/`p9`. Those
references are V-4's and the PCI-config/9p doors' — different findings, different steps; B1 closed
the downcast cause, which is what V-1 was.

**B2 — Move each device's ioctl contract down to its driver.** For each family in 11.1.2
(fbdev, DRM, RTC, uinput, tty, evdev, block), move the request-number constants, struct layouts
and validation into the driver module as `pub fn ioctl(req, arg, from_user) -> isize` (or a small
table), leaving `syscall/linux/fs.rs` with the dispatch to that entry point. The *pure* parts stay
host-compiled and keep their tests; `drivers/drm.rs` already carries most of the DRM UAPI, so it
is closer to this shape than it looks.

**B3 — Move the user-pointer helper out of `syscall`.** `buffer_ok`/`err_of` answer "is this range
readable/writable by the current task", an `mm` question; the plan was `mm::user::{validate, copy_in,
copy_out}` called by `syscall`, `net` and `drivers`. (Done ahead of it: the errno table moved to
`crate::err` (§11.1.5), and `uevent`'s task-table reach went with it (V-14), so B3 owns exactly
`buffer_ok`/`err_of`.)
**LANDED (this pass) — V-9 closed.** The two helpers moved to a crate-root leaf `src/uaccess.rs`
(`buffer_ok`/`user_accessible`/`MAX_IO` plus the machine-gated `err_of`), mirroring `err`'s
relocation. The shape is `crate::uaccess`, not `mm::user`: `err_of` maps a `VfsError` (a `vfs` type)
and `buffer_ok` walks `arch::paging` — neither is an `mm` concept — and a root leaf lets any layer
below `syscall` reach it without naming the ABI layer. The 99 below-`syscall` call-sites in
`net/{socket,unix,packet,vsock}.rs` and `drivers/drm.rs` now name `crate::uaccess::*`; `syscall`
re-exports the same names, so its ~350 internal sites and `CallerScan` are unchanged. V-5's `drm` half
is closed the same way (the `sound.rs`→`syscall::linux::abi` edge is V-2's ioctl-ABI concern, not
B3's). This is the prerequisite for B2 (kd/evdev 接受-拒绝下沉到驱动), now unblocked. Verified:
bare-metal build green + host suite `570 passed` + bare-metal clippy clean; the
`cfg(target_os = "none")` split of `err_of`/`user_accessible` moved intact, applying V-11's lesson.
**B2 (evdev 下沉到驱动) — attempted, destabilized by a latent DMA bug, root-caused, then LANDED.** Sinking
`evdev_ioctl` into `EvdevNode::ioctl` (mirroring the `'d`→`drivers::drm::drm_ioctl`
precedent, with fd-resolution kept at the syscall boundary) was byte-for-byte the same logic and
compiled for both targets, yet the guest panicked *reproducibly* mid-suite: the `assert!` in
`mm::virt_to_phys` (`src/mm/mod.rs:97`) fired on a user address from a `write`-path caller
(`virt_to_phys(0x3f…): not an address from the direct map`), aborting before the run ended cleanly.
Reverting the move restored `=== SUMMARY pass=135 fail=1` with no panic. A behaviour-preserving
relocation changes no ioctl answer, so this was a **latent memory bug** *exposed* by the code-layout
change — and the caller was located, not guessed: of `virt_to_phys`'s four live call sites, the one
on a `write` (nr 1) path with a buffer pointer is the virtio DMA helper `drivers/virtio/hal.rs:59`
`virt_to_phys(buffer.as_ptr())` (the framebuffer callers are GPU-only, `free_frames` gets a
direct-map address back) — the virtio-net TX / sound DMA path, the same area as the in-flight
`t-dhcp`/`t-dsp` reds. **Resolved the same session:** the sound driver's wrong PCI id
(`0x1055`→`0x1059`) and its user-address DMA hand-off were fixed, the gate reached
`=== SUMMARY pass=136 fail=0`, and the evdev relocation was re-applied on that clean tree — bare-metal
build green, host suite `570 passed`, guest `pass=136 fail=0` with `t-evdev`/`t-ioctl`/`t-seat` all
green, proving `EvdevNode::ioctl` answers identically to the old switchboard. **B2-kd also LANDED:**
the KD accept/reject serve moved to `Tty::kd_ioctl` (the console device in `vfs/tty.rs`), with
fd→tty→vc resolution and the DRM `card_grabbed_by_current()` check kept at the syscall boundary and
`owns_card` handed *down* as an argument. Because the card answer comes in as a parameter, `vfs::tty`
never names `drivers`, so no forbidden `vfs→drivers` edge is created, and the KD decode leaves the
switchboard (V-2) exactly as evdev's did. `request_arg` (its only user) went with it.
Verified guest `pass=136 fail=0` with
`t-ioctl`/`t-seat`/`t-tty-eintr`/`t-tty-vtime`/`t-tty-query` green; an intermediate `pass=134 fail=2`
was the `t-netlat`/`t-times` timing-flake pair (green on the re-run). The `'V'` family (`vt_ioctl`)
still sits in `fs.rs`:
it too reads live console state in `vfs/vcmux`, and would follow the same hand-off shape; left as-is.
The lesson stands: a behaviour-preserving relocation can only be trusted once the gate is green on the
*current* tree, since a layout shift unmasks unrelated latent bugs.

**Plan acceptance, for the record:** no `crate::drivers::`/`crate::net::` in `src/vfs/mod.rs`, no
`crate::syscall::` under `src/drivers/` or `src/net/`, `fs.rs` materially below 6 311, and every host
test plus `cargo xtask test` green, one device per commit (fbdev first, DRM last). The met/not-met
detail is above.

---

#### Step C — Let subsystems own their per-open-file resources (V-3) — **DONE**

The plan: turn `close_at_exit` ×4 in the scheduler into a **registry** of hooks each subsystem
registers at boot, walked on task exit through nothing but the scheduler's own trait; a subsystem that
registers nothing costs nothing.

For the exec path, decide per edge: `binfmt::shebang` and `vfs::read_file` are genuinely needed to
load an image, so accept that `vfs` edge and document it as allowed (or hold a narrow `ImageSource`
trait). Per-open-file resource ownership is somebody else's; reading an executable is the
scheduler's. Plan acceptance: `runtime.rs` names no `net`/`drivers`/`vfs(inotify|locks)`, a task
holding a socket, inotify watch, evdev grab and uinput device is still cleaned up (`t-inotify`,
`t-evdev`, `t-filelock`, `t-unix-scm`), the file falls from 6 397 lines, and the teardown order is
pinned by a host test on the registry.

**Done, and it took the first mutual pair out of the graph.** The registry is
`src/sched/exit.rs`: an append-only list of `fn(&[Arc<File>], owner: u32)` hooks, walked from
`runtime.rs`'s exit path before the batch is parked for dropping (the reference counts still say
which description is the last one until then, which is what `vfs::file::for_each_last` reads).
Registration happens in `main.rs`, in the order the resources have to go, because the order the
list is walked in *is* the registration order: locks first (a lock is keyed by `(device, inode)`
and the inode is reached *through* a descriptor), then the state keyed by the open file
description — the socket, the inotify watch, the evdev reader position, and the uinput device a
program made for itself. `main.rs` is the wiring, so it is allowed to know all five; the
scheduler knows none of them.

Each subsystem's hook now takes the same two things the scheduler can offer. Two of them had to
say so in their signature rather than being wrapped — `net::socket::close_sockets_at_exit` was
`pub(crate)`, which a binary crate cannot see, and is `pub` now — and `vfs::locks` gained
`release_on_process_exit`, which is the loop that used to sit in the teardown: *which of these
descriptors names an inode this process still holds a lock on* is a locking question, and
`vfs::mount::fs_id_of_inode` is a vfs one.

**Measured, and the number that matters.** `sched → net` is **gone** (1 → 0), which is what
removed the `net ↔ sched` pair: `net → sched` is 114 references of ordinary parking and stays,
but a one-way edge is not a cycle. Mutual pairs 20 → **19**, and the ratchet constant moved down
with it — a reduction is a commit that lowers the baseline, which is what makes the ratchet read
as progress rather than as a fence. `sched → vfs` also fell 26 → 22 (locks, mount and inotify),
and `sched → drivers` 4 → 2.

**The acceptance is met on the exit path and not on the exec path, deliberately.** The two
remaining `crate::drivers::` references in `runtime.rs` are `execve`'s `FD_CLOEXEC` pass, which tells
the input drivers to forget a reader position keyed by an address the new image will reuse. That is a
*different event* with a *different* pair of subsystems to tell, and Step C's own text says the exec
path should decide per edge rather than be abstracted for symmetry — so the `drivers ↔ sched` pair
stands, named in `depgraph.rs` beside the rule, rather than disappearing behind a mechanism that
would run the socket, watch and lock hooks on an event that is not a process ending.

**Pinned by the registry's own host test**
(`hooks_run_in_registration_order_and_an_empty_list_is_not_a_special_case`): two hooks that record
what they were handed, asserted to run in registration order with both the batch and the owner, plus
the empty list as a no-op. It is one test rather than two on purpose — the registry and the recorder
are both process-global, and the first draft's second test failed for exactly that reason. The
end-to-end gate stayed the guest's: `t-inotify`, `t-evdev`, `t-filelock` and `t-unix-scm` all report
`=== PASS`, and the whole sweep is `=== SUMMARY pass=140 fail=0 (140 built)` with QEMU exit `33`.

---

#### Step D — Move the terminal emulator out of the VFS (V-4) — **CLOSED** (emulator + VC vocabulary + `vtmode` + the active-console number all in the `term` leaf; the `vfs ↔ console` pair broken by D.2a; D.2b evaluated and rejected)

**Plan:** create `src/term/` (grid, keys, font, escape parsing) and `src/console/vcmux`, leaving in
`vfs` only filesystem facts (`/dev/tty*` and `/dev/pts/*` nodes, the line discipline, `termios`);
`console` depends on `term`, and `vfs` on neither. Acceptance: `vfs/` has no font asset, escape parser
or `vcmux`; `crate::vfs::term` is gone; `console.rs` does not reach into `vfs` for the grid; and
`cargo xtask run --mode mirror` / `Ctrl-A 1..6` behave as before. Risk: `vfs/term/vga8x16.bin` is
`include_bytes!`-ed, so a missed path is a compile error.

**Half landed, and the half that landed was a crate.** `src/vfs/term.rs` and `src/vfs/term/` (blit,
cursor, font, keymap and the 4 KiB `vga8x16.bin`) became `crates/term` — `mpos-term`, a leaf that
cannot name the kernel — re-exported as `crate::term`. That is the plan's `src/term/` with the
enforcement added: `crate::vfs::term` no longer exists, `vfs/` holds no font asset and no
escape-sequence parser, and the three `vfs/term` test modules moved and run in the new crate (32 pins,
unchanged).

The one place this differs from the plan's own sketch is *who moves*: the plan said `console`
"then depends on `term`", and the extraction made that literal — `console.rs` (3 references),
`drivers/video/render.rs` (3) and `drivers/virtio/input.rs` (2) now name `crate::term`, which is
what takes 3 references off `console → vfs` (4 → 1) and 5 off the largest single edge in the tree
(`drivers → vfs`, measured 71 by the ratchet afterwards). `vfs::tty` reaches for the same crate,
which is right: the line discipline and the grid must agree about the cursor (§7.1b's whole
finding is that they must not each keep one).

**What remains is the mux, and it is not mechanical.** `vcmux.rs` looks like the same shape and is
not: it reaches *out* to `console::write_bytes`/`vc_output`/`grids_live`, to
`sched::{spawn_permanent, block_until_deadline, wake, send_signal_process}` and to
`drivers::{video::render, drm::master_held}`. Moving the file to `console/` would exchange a
`vfs → console` edge for a `console → vfs` one and leave the pair where it was; what had to move
first is the shared vocabulary — the console count and the KD mode words are console facts that
`pty`/`tty`/`vtmode` happened to define.

**D.1 — the shared vocabulary moved. LANDED (this pass, green).** `VC_COUNT` (was
`vfs::pty::VC_COUNT`) and the `KD_TEXT`/`KD_GRAPHICS`/`K_RAW`/`K_XLATE`/`K_MEDIUMRAW`/`K_UNICODE`/
`K_OFF` mode words (were `vfs::tty::*`) now live in `crates/term/src/vc.rs` as `crate::term::vc::*`.
`term` is a leaf that `vfs`, `console` and the drivers already depend on, so naming it is downward for
all of them, which is what lets the next sub-commit move the `vcmux` *file* without re-creating the
pair. A pure relocation: `drivers/video`, `drivers/devices` and `drivers/virtio/input` stop naming
`vfs::pty`/`vfs::tty` for these constants; `vfs::{tty,pty,vcmux}` name `crate::term::vc` instead.
Verified behaviour-preserving — bare-metal build green, host suite unchanged at `528 passed`,
`cargo xtask depgraph` reports "no edge appeared and no count has grown" (`crate::term` stays out of
the mutual-pair set, `vfs/mod.rs → drivers|net` stays 0), and the full gate ends
`=== SUMMARY pass=140 fail=0`, exit **33**, with
`t-seat`/`t-tty-eintr`/`t-tty-vtime`/`t-tty-query`/`t-ioctl`/`t-drm-lease` all `=== PASS`.

**D.1b — the handover table moved too. LANDED (this pass, green).** `vfs::vtmode` (the pure
`VT_AUTO`/`VT_PROCESS` release/decision table — `VtMode`, `Decision`, `Ack`, `Promise`, `Signal`,
`Release`, `Table`, `RELEASE_TIMEOUT_TICKS`) is a self-contained state machine that named no kernel
module at all (only `alloc::vec`), so it is a leaf a host build can argue with. It now lives at
`crates/term/src/vtmode.rs` as `crate::term::vtmode`, the same leaf that holds the VC vocabulary and
claims "which of the six virtual consoles is active" as its business. `vfs::vcmux` and `vfs::tty` name
it there; `vfs/mod.rs` no longer declares it. Host coverage preserved: the 17 `vtmode` tests moved
from the kernel's `-p mpos` run (528 → 511) into `-p mpos-term` (32 → 49), and `cargo xtask depgraph`
still reports no edge appeared and no count grew.

**D.2a — the pair is broken. LANDED (this pass, green), and the premise corrected.** The plan assumed
the pair stood on the mux's `vfs → console`/`vfs → drivers` *calls*. The measurement says otherwise: a
mutual pair collapses the moment *either* direction hits zero (`xtask/src/depgraph.rs:104-110`), and
`console → vfs` was a **single** reference — `src/console.rs`'s `active_grid()` reading
`crate::vfs::vcmux::active()`. So the cheap inversion was that one inbound read, not the mux's
outbound calls. The active-console number — a lone `AtomicU32` — moved from `vfs::vcmux` to the `term`
leaf (`crate::term::vc::active`/`set_active`); `console.rs` and the drivers now read it there, and the
mux writes and reads it there too. `console → vfs` = 0, the pair collapsed to a one-way
`vfs → console`, and **mutual pairs fell 18 → 17** — the first number Phase 11 has lowered since Step
C, ratcheted by `BASELINE_MUTUAL_PAIRS = 17`. Behaviour-preserving: bare-metal build green, host
suites unchanged (`511` kernel + `49` term + `47` xtask), `cargo xtask depgraph` reports
`console ↔ vfs` gone and no new edge, and the full gate ends `=== SUMMARY pass=140 fail=0`, exit 33,
with `t-seat`/`t-tty-*`/`t-drm`/`t-fbdev` all PASS.

**D.2b — evaluated and REJECTED. Step D CLOSED at its acceptance criterion.** The remaining item was
relocating the `vcmux.rs` *file* out of `vfs` (into `console`), inverting its outbound `vfs → console`
(`write_bytes`/`vc_output`/`grids_live`) and `vfs → drivers` (`video::render::notify`/`drm::master_held`)
calls in the hand-down/`Waker` shape Step B2 established. Measuring it this pass showed no sound,
valuable landing, so it is not done:

- **The move would regress the layering.** `console` is a designed leaf — `AGENTS.md` §3 lets it name
  only `arch`, `boot` and `term`. `vcmux` names `vfs`(`pty`/`tty`), `drivers`(`drm`/`video`) and
  `sched`; relocating it to `console` turns `console` into a mid-layer depending on all three, the
  opposite of V-4. A bare move first *re-creates* `console → vfs` from the mux's 7 `pty`/`tty` calls
  (`lookup`/`pairs`/`master_ready`/`take_master` and `tty::global().receive_byte`), undoing D.2a.
- **A mux-only inversion yields zero ratchet gain.** Measured: `console ↔ vfs` is **already gone**;
  `vfs → console` is 8 references but only 5 are in `vcmux` (the other 3 are `tty.rs` (2) and `ext4`
  (1)), so the mux cannot zero the edge; `vfs → drivers` is 17, of which only 7 are `vcmux` (the other
  10 are `procfs`/`sysfs`/`p9` device-state reads). The surviving `drivers ↔ vfs` pair is not
  `vcmux`'s to break — it is the procfs/sysfs/9p door, a different finding (§11.1).
- **The cost is real:** the change threads boot-registered hooks through the seat/DRM/render pump — the
  subsystem whose "nearly-right" regressions (untagged devices, a dropped pointer) this repo has
  repeatedly paid for — for no measurable improvement.

Per §4 ("refuse rather than approximate", "high cohesion — locate the module that owns the concept"),
Step D is **closed**: the `vfs ↔ console` cycle is broken, the terminal emulator, VC vocabulary,
`vtmode` and the active-console number all live in the `term` leaf, and the `vcmux` file stays in `vfs`
where its pty/tty orchestration belongs. The residual `drivers ↔ vfs` pair is recorded against its own
finding, not against Step D.

---

#### Step E — Split `fs.rs` along its real seams (follow-up to B2) — **LANDED**

After B2 the file should be much smaller. What remains is still two jobs: the path-carrying calls
(`open`, `stat`, `getdents64`, …) and the device-ioctl entry point. Split into
`syscall/linux/file.rs` and `syscall/linux/dev.rs`, or leave it once B2 has removed the
switchboard — whichever the size says.

**Acceptance:** the dispatch routing counts are unchanged, no handler changed meaning, host tests
unchanged.

**Landed.** The size said split (B2 shrank the ioctl arms but the file was still 6833 lines). It went
to a directory module `src/syscall/linux/fs/` rather than two top-level files, because that keeps
`syscall/linux/mod.rs`'s ~95 `fs::<handler>` references byte-for-byte identical: `fs/mod.rs` holds the
module header, the one helper both halves call (`sync_nonblocking`), and re-exports the entry points
(`pub(super) use file::*; pub(super) use ioctl::*;`), so the dispatch still names `fs::handle_read`,
`fs::handle_ioctl`, … unchanged. `fs/file.rs` (5523 lines) is the path-carrying handlers and their
private helpers (`Xattr`, `base_of`, `record_lock`/`lock_take` with both cfg bodies moved together,
the `flock_off` submodule); `fs/ioctl.rs` (1281 lines) is `handle_ioctl` and the contiguous per-family
routers (`family_nr`/`vt_ioctl`/`kd_ioctl`/`fs_flags_ioctl`/`uinput_ioctl`/`evdev_ioctl`/
`ifreq_index`/`ifreq_flags`). The seam is unusually clean: the entire device-ioctl footprint is one
contiguous region, and exactly one helper crosses it. `fs` is machine-only
(`#[cfg(target_os = "none")] pub mod fs`), so the kernel host build never compiles it — hence the
bare-metal build and QEMU gate rather than the host suite. Pure relocation:
`cargo build --target x86_64-unknown-none` green with clippy back to its 29-warning baseline,
`cargo xtask depgraph` unchanged (mutual pairs 17/17 — `fs` lives inside `syscall`, so no module edge
moves), host suite unchanged at 511, and the full gate ends `=== SUMMARY pass=140 fail=0`, exit **33**
— the path/file and every ioctl-family conformance program (`t-ioctl`, `t-drm`, `t-fbdev`, `t-tty-*`,
`t-seat`, `t-hwclock`, …) pass on the relocated handlers.


---

#### Step F — Make the target gate a rule, not a habit (V-11 + the `ARCHITECTURE.md` §8 drift) — **DONE**

*The one step in this phase that is pure rule-making: it changes no behaviour and removes no
cycle, and it exists because the gate's failures are silent.*

**Plan:** settle the `net` case (host-compile it or delete the unreachable test module and say so);
write the criterion down in `AGENTS.md` §3 and `src/lib.rs` — *a module is host-compiled when its
logic touches no hardware, no port I/O, no virtio ring and no real clock, and its dependencies must
be host-compiled too, transitively*; and add a mechanical check. **Acceptance:** the check fails on
today's `net/socket.rs` and passes after the fix; `cargo test -p mpos-xtask` covers it.

**Done, and the three answers the step asked for.**

*Which way `net` went, and why it is the cheap way rather than the lazy one.* "Host-compile it, or
delete the module and say so" are not equivalents, and the tie-breaker was coverage rather than
effort. Making `net` host-compilable means a device seam for `drivers::virtio::net::VirtioNet` and a
clock seam for `arch::timer` (10 machine references across `net/mod.rs`, `net/socket.rs` and
`net/vsock.rs`, plus `smoltcp` moved out from under the `target_os = "none"` dependency) — real
Phase-11 B work, not Step F work. The three tests, read instead of assumed, turned out to be
`t-sockmsg`'s subject: `user/tests/src/bin/t-sockmsg.rs` pins `send`/`recv` both ways, the
`sendmsg`/`recvmsg` gather and its "no address, no control data, no flags" answer, `MSG_DONTWAIT` on an
empty pair as `EAGAIN`, and a destination on a connected pair as `EISCONN` — the same four claims, in a
program that boots. The module is deleted and §11.1.10 records where its coverage lives; a comment at
the end of `net/socket.rs` says the same and names the check that keeps a fourth from being written.

*The criterion is written down in one place and named from the others*: `src/lib.rs`'s header carries
it (with the transitive half and the two ways it fails silently), `AGENTS.md` §3 points at it and keeps
its "when you gate a module, check what gates its tests" instruction, and `ARCHITECTURE.md` §8 states
it and records V-11's closure.

*The check is `xtask/src/gates.rs`, and it is pinned against itself.* `dead_host_tests` walks
`src/lib.rs`'s `mod` declarations, resolves each to `dir/name.rs` or `dir/name/mod.rs` the way the
compiler does, and carries the gate down — so a file under an ungated-but-gated-child module is caught
as well as one declared gated directly. It runs at the head of `cargo xtask test`, so the failure is a
named error rather than a test count nobody reads. Its four unit tests include the counterfactual this
kind of checker needs: a two-file tree with the *same*
`#[cfg(all(test, not(target_os = "none")))]` in one gated and one ungated file, where the first must be
reported and the second must not. Two more pin the gate arithmetic — the ungated sibling, and the boot
handoff's own `#[cfg(all(any(target_os = "none", test), target_arch = "x86_64"))]`, which names the
host by way of `test` and so must *not* read as machine-only.

*Verified in the counterfactual order the plan asked for:* with the check in place and the module still
present, `cargo test -p mpos-xtask` failed on exactly one violation — `src/net/socket.rs`, gated by
`crate::net::socket`; after the deletion the same command is `42 passed`, the bare-metal build is green
with the same single pre-existing warning, and the host kernel suite is unchanged.

---

#### Step G — Keep the graph from regressing — **DONE**

**Plan:** add a `depgraph` subcommand (or host test) that re-runs §11.0's extraction and asserts no
lower-layer edge into `syscall` (`drivers`, `net`, `blk`, `pci`, `rand`, `uevent`, `vfs`, `sched`),
no `crate::drivers::`/`crate::net::` from `vfs/mod.rs`, and the cycle count at or below a recorded
baseline — a **ratchet**, not a cliff, stored as a dated constant next to the check.
**Acceptance:** `cargo xtask depgraph` reports both counts and fails when either worsens.

**Done — and one correction to the method, which is the interesting part.** The ratchet is
`xtask/src/depgraph.rs`, reachable as `cargo xtask depgraph` and also run as a host test, so it is
inside `cargo xtask test` rather than beside it. It measures the tree the way §11.0's script does —
every file under `src`, each mapped to its top-level module, every `crate::<mod>` reference an edge —
with **one deliberate difference: `//` comments are stripped before matching.** That is what makes the
number mean "who names whom". §11.0 matches text, which is why the audit counted three edges that are
prose: `uaccess → syscall` is a doc comment (and `vfs → uaccess` with it), and the third pair
`arch ↔ vfs` came from a file the compiler never saw (§11.1.7's deleted `arch/x86_64/serial.rs`).
Measured both ways on this tree: **23** mutual pairs by text, **20** in code. The ratchet holds the
code number, because a rule that fires on a sentence is a rule people learn to word around.

**The rules are ratchets rather than absolute refusals.** Each constant names the call sites that
keep it non-zero and the step that removes them:

| Rule | Measured 2026-10-02 | What kept it there | Removed by |
|---|---|---|---|
| lower layer → `syscall` | `drivers` 1, `sched` 10, the rest 0 | `drivers/virtio/sound.rs`'s one `use crate::syscall::linux::abi` (V-5's remainder); `sched/runtime.rs`'s `crate::syscall::linux::{abi, nr}`, `MMAP_BASE` and `DEFAULT_UMASK` | B2 (the ioctl ABI), B/C (the mapping base and the umask are scheduler state) |
| `vfs/mod.rs` → devices | 9 | the nine `Inode::as_*` downcasts (V-1) | **Done** (B1): 0 |
| mutual pairs | 20 | the whole `vfs ↔ sched ↔ syscall ↔ net ↔ drivers` cluster | B, C, D |

`arch` and `main` are deliberately **not** in the first rule: the `syscall` gate exists to enter
the dispatcher and `main` is the wiring, so both edges are the design. A layer with a baseline of
zero — `net`, `blk`, `pci`, `rand`, `uevent`, `vfs` — fails on its *first* reference, which is
where the ratchet earns its name.

Four unit tests pin the instrument itself, because a measurement that cannot tell two shapes apart
agrees with whatever it is pointed at (§12.1 A8's lesson, in a different currency): a reference in
a doc comment is not an edge; a cycle of two is reported once and a one-way edge is not a pair; a
service naming `syscall` with no baseline fails; and the `vfs/mod.rs` rule is silent at its
recorded number and fails one past it. The check has never been red on this tree, and it has been
shown able to go red on each of the three rules.

---

### 11.3 Order, and what is deliberately not in this plan

**Do this, in this order:**

1. ~~**V-12** — finish or revert the in-flight refactor.~~ **Done**: the tree builds and the
   whole gate is green (§11.1.12), so everything below is now verifiable.
2. ~~**Step A** — pure subtraction, ~5 files, one commit, no behaviour change.~~ **Done**
   (§11.2 Step A): the three dead modules, the `[msgtrace]` trace and the aarch64 profile are
   gone, and the pair count it was measured with fell from 22 to 21.
3. ~~**Step B** — the device seam, one device at a time.~~ **Done** (§11.2 Step B): B3 moved the
   user-pointer helpers to the `uaccess` leaf, B2 put every device family's ioctl contract next to
   the state it answers from, and B1 (fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the
   DRM family last) deleted the nine `Inode::as_*` downcasts, taking `vfs/mod.rs → drivers|net` 9 → 0.
   The pair count did not move with it — that is V-4's and the PCI-config/9p doors' share.
4. ~~**Step C** — the resource registry.~~ **Done** (§11.2 Step C): the exit hooks live in
   `sched::exit`, registered by `main.rs`, and the `net ↔ sched` pair is gone.
5. ~~**Step D** — the terminal emulator move.~~ **Done** (§11.2 Step D): D.1/D.1b/D.2a moved the VC
   vocabulary, the pure `vtmode` table and the active-console number into `crate::term`, collapsing
   `vfs ↔ console` — **mutual pairs 18 → 17**; gate green `pass=140 fail=0`. **D.2b was measured and
   rejected** — `console` is a leaf the mux's `vfs`/`drivers`/`sched` naming would violate, and a
   mux-only inversion moves no ratchet. The surviving `drivers ↔ vfs` pair is its own finding.
6. ~~**Step E** — the `fs.rs` split.~~ **Done** (§11.2 Step E): still 6833 lines, so it split into
   `src/syscall/linux/fs/{mod,file,ioctl}.rs`; `mod.rs` re-exports the entry points so the
   `fs::<handler>` dispatch is unchanged. Gate green `pass=140 fail=0`, depgraph 17/17.
7. ~~**Step F** — the gate criterion and the check for it.~~ **Done** (§11.2 Step F): the criterion is
   in `src/lib.rs`/`AGENTS.md`, the dead test module is deleted with its coverage named, and
   `xtask/src/gates.rs` fails the sweep on the shape.
8. ~~**Step G** — the ratchet.~~ **Done** (§11.2 Step G): `cargo xtask depgraph` measures the graph
   with comments stripped and holds three recorded baselines; it also runs as a host test.

Steps A, E, F and G were mechanical (a day each); B, C and D needed the device/scheduler/console paths.
**Phase 11 is closed except one surviving mutual pair: `drivers ↔ vfs`, the `procfs`/`sysfs`/`p9`
device-state door (§11.1), not any Step.** Nothing here had to land before Phases 1–10 — but every
feature lands *cheaper* after B and C, which is the argument for having done them first.

**Deliberately not in this plan:**

- **Rewriting the `Inode` interface.** The trait itself is fine; it is the nine device and socket
  methods that hung off it that were wrong. B1 removed them, added one type-erased handle the
  caller names a type through, and stopped there.
- ~~**A generic HAL trait forest.**~~ **Reversed — see §11.4.** What this note rejected was a
  trait per function; §11.4 lands *six* traits for the six hardware concerns the kernel has,
  with the platform tag owned by `hal` so the seam adds no dependency cycle. The reasoning
  against indirection still holds and is honoured: the forwards are static, there is no `dyn`
  and no allocation, and a call monomorphises to the direct one it replaced.
- **A message bus or an async runtime.** The kernel is uniprocessor and synchronous by design,
  and `ARCHITECTURE.md` §7's locking rules exist because that is what makes the design legible.
  This phase is about *who names whom*, not about adding indirection.
- **Splitting files because they are long.** `vfs/ext4/mod.rs` (3 391 lines) stays as it is.
- **Restructuring `src/main.rs` (1 625 lines).** The boot sequence, the milestone checks and the
  panic handler do belong in the binary; if it ever becomes worth splitting, the checks move to
  `src/main/checks.rs`, and that is a tidiness commit rather than an architecture one.
- **Fixing V-10.** Accepted, documented, and cheaper to live with than a shared crate across two
  targets would be.

### 11.4 The HAL: a machine seam under `arch/` — **CLOSED (S1–S5 LANDED)**

Phase 11 is "who names whom"; this is the largest remaining piece of it, deferred above until
now. Goal: everything above the machine layer compiles against *capability traits* instead of
`arch::x86_64::*`, so a second architecture is `arch/<arch>/` and nothing else (§9.1/§9.2), and
no architecture-specific type crosses the boundary without a compile error.

**Shape** (`src/hal/mod.rs`): six traits for the six hardware concerns — `Clock`, `WallClock`,
`Cpu` (landed), then `Interrupts`, `Paging`, `Contexts` with associated types — plus the platform
tag `Current` and static one-line forwards. The tag is `hal`'s own type and `arch/<arch>/`
implements the traits *for* it, so the seam is a one-way `arch → hal` arrow and adds no cycle:
`cargo xtask depgraph` measured **17 → 15** mutual pairs on S1 (`ARCHITECTURE.md` §6).

**What it does not abstract, and must not.** The Linux ABI *is* the register layout:
`user_regs_struct`, the signal frame, the `syscall`/`iretq` gate are x86_64 by definition (on
aarch64 the same rows are `x0..x30`/`pc`/`pstate` behind `svc`). That stays in `arch/<arch>/`;
wrapping it would be the private dialect `AGENTS.md` §3 rule 3 forbids. "No leakage" means a
port writes `arch/<arch>/` and touches no shared file — not that a register frame becomes
neutral.

**Stages**, each its own commit and green alone (AGENTS §3 rule 7):

| Stage | Contents | Status |
|---|---|---|
| S1 | `hal` + `Clock`/`WallClock`/`Cpu`/`Current`/forwards; the `arch::timer`/`rtc`/`idle`/`halt` spread rerouted | **landed** — pairs 17 → 15 |
| S2 | `Interrupts` (`Line`, `unmask`/`mask`/`eoi`, `init_message_signalled`, `net_msi_vector`): `serial`, `virtio/net`, `lapic` | **landed** — pairs 15 → 14; gate `pass=142 fail=0` exit 33 |
| S3 | `Paging` (`Space` assoc + `AddressSpaceOps`, `install`/`map_mmio`/`translate`/`user_accessible`/`is_cow_page`/`write_user_u32`/roots/switch/`phys_for_virt_of`); `PageTaken` moves to `mm::vm`; migrates `sched`, `mm`, `uaccess`, `syscall`, `main` | **landed** — pairs 14 → 14 (a swap: `arch ↔ mm` out, `hal ↔ mm` in); gate `pass=142 fail=0` exit 33 |
| S4 | `Contexts`: `Context`/`Fpu`/`UserFrame` as associated types behind `ContextOps`/`FpuOps`/`UserFrameOps`; `sched` and the ABI layer stop naming `arch` | **landed** — pairs 14 → 12 (`arch ↔ sched`, `arch ↔ syscall` gone); gate `pass=142 fail=0` exit 33 |
| S5 | close the door: the depgraph rule is an allow-list (`arch`/`hal`/`main`/`lib`, everything else at 0), and `arch/x86_64`'s submodules are `pub(crate)` behind a four-item facade | **landed** |

S2–S4 touch the trap, page-fault and context-switch paths, so each needs the full QEMU gate,
not host+clippy alone. S1 was pure re-routing — host tests, both clippy targets and the depgraph
were the whole measurement, and its own reds (none) would have been a compile error, not a
machine bug.

**S4's design finding, found by measuring rather than by planning it.** The plan said the
register-frame knowledge would move into `arch/<arch>`. Measuring what is left showed why only half
can: after S1–S3 exactly two modules still name `crate::arch::` — `sched` (17, the saved `Context`,
`FpuState`, `UserContext` and the switch/FPU/bootstrap calls) and `syscall/linux/proc.rs` (4:
`gdt::user_selectors` twice and the `UserContext` literal). The frame's **constructor** can move down
— building a frame from the gate's trap record is a pure machine fact, and moving it removes the
`SegmentSelector` from the ABI layer. But its conversion **into** the Linux `user_regs_struct` array
(`abi::uregs`, `context_regs`/`context_from_regs`/`capture_frame`) cannot: `arch` naming
`crate::syscall::linux::abi` is an `arch → syscall` edge, the one direction this design forbids and
the depgraph's first rule exists to catch. So the array conversion stays in the ABI layer, and the
frame type exposes the operations that layer needs (`to_regs`/`from_regs` with the array as an
associated `Regs` type, `ip`/`sp`/`set_result`, `forked`); `ContextOps` hands out its `Fpu`
(`fpu()`/`fpu_mut()`, with `type Context: ContextOps<Fpu = Self::Fpu>` tying the associated types
together) because `sched` stores `Context` and pokes its FPU half on every switch.

S4's accepted shape: `hal::Contexts` with `Context`/`Fpu`/`UserFrame` behind `ContextOps`/`FpuOps`/
`UserFrameOps`, the aliases and `FPU_BYTES` as crate-level consts in `hal` (an associated const in an
array length needs `generic_const_exprs`), the frame *constructor* in `arch`, and the ABI array
conversion where the ABI is. That takes both counts to 0 and lets S5 close the modules.

---

### R.11.E (moved from ROADMAP's status board) — the leaf crates, each with what it cost and what it proved

**The seven pure, downward-only leaves** (`err`, `pci`, `date`, `binfmt`, `cred`, `uts`, `seccomp`) moved out of `src/` into `crates/*` as workspace members, re-exported from `src/lib.rs` under their old names (`pub use mpos_err as err;`). The point is enforcement: a leaf crate cannot name the kernel, so the downward-only rule (AGENTS §3 rule 5) is a compile error for those seven, not a convention. `xtask test`'s host stage names each crate with `-p` so the moved pins keep running (635 in-kernel + 50 in crates = the 685 the suite ran before). `mpos-pci`'s fixtures (`Device::fixture`, `test_lock`) reach the kernel's `/sys`/`/proc` tests through a `test-helpers` feature only a dev-dependency enables, so the shipped binary carries no scaffolding. Held back were `gpu` and `evdev`/`uinput`, whose waiters took `sched::TaskId` + `sched::wake`; both have since left — `gpu` two entries down, `evdev`/`uinput` with the same treatment.

**Three more of the same kind**, extracted once the seven above had shown the pattern: `mpos-term` (the terminal emulator — grid, cursor, blitter, keymap, font; a cell grid is not a filesystem fact, §11.1.4), `mpos-ahci` (the AHCI port decode, whose only reader is machine-only) and `mpos-gpu` (the virtio-gpu wire ABI, the DRM blobs, the cursor blend and the sync-object state machine). `crate::term`, `crate::ahci` and `crate::gpu` resolve through `src/lib.rs`'s re-exports, and the call sites that *did* move are the ones whose edge was the point: `console`, `drivers/video/render` and `drivers/virtio/input` now name the `term` crate rather than `vfs::term`, which is 3 references off `console → vfs` and 5 off `drivers → vfs`. The pins that travelled with them run in their own crates (`xtask test`'s host stage names them), so the host total only grows by the test the next entry added.

**`mpos-gpu` is the one that paid for itself twice**: it sat inside the strongly connected component by a single edge — `gpu/sync.rs` woke a parked waiter by calling `sched::wake` — so the crate boundary forced that edge to become a parameter, `gpu::sync::Waker`, implemented by `drivers::drm` where the task table is and by a recording struct in the tests. The wake had been behind `#[cfg(target_os = "none")]`, which is why no host test had ever asserted it; there is one now (`submitting_a_fence_wakes_every_parked_waiter_and_nobody_else`), and `t-drm-sync` keeps pinning the same claim on the machine end to end. This is the shape `evdev`/`uinput` took in the next entry, and the reason it is written down here rather than only in the code: the abstraction is small, and what it buys is the difference between a rule a reviewer enforces and one the compiler enforces.

**The input policy pair followed the same seam**: `mpos-evdev` (the record window and its clients) and `mpos-uinput` (the request vocabulary a program declares a uinput device with, which depends on the evdev crate for one constant — the size of a `struct input_event`). `src/evdev.rs`'s one kernel dependency was `sched::policy::TaskId` in its waiter list, and it never called `sched::wake` itself — the driver did — so the fix is `mpos-gpu`'s in a smaller size: `WaiterId` is an opaque number there, and `drivers/virtio/input.rs` converts at the four call sites it owns. `evdev` leaves the module graph entirely as a consequence: its former `→ sched` and `→ vfs` edges are gone from `cargo xtask depgraph`'s table, and what remains is the two crates being *named* by `drivers` and `syscall`, which is a cross-crate edge downward. Twelve leaf crates now, all but the input pair host-compiled and every one of them unable to name the kernel.

**The block layer seam is the thirteenth**: `mpos-blk` — the `BlockDev` contract, the disk registry and the GPT/MBR decode — moved out of `src/blk` into `crates/blk`, re-exported as `pub use mpos_blk as blk;` so every `crate::blk::` caller is unchanged. It was already host-compiled and host-tested (13 tests), and the module's one kernel reference — `part.rs` sizing its transfer buffer from `mm::PAGE_SIZE` — is now `mpos_blk::PAGE_SIZE`, the bound belonging to the trait that states it rather than the frame allocator that sizes the same hardware fact. `blk` leaves the module graph as a source: `drivers`/`uevent`/`vfs` still *name* it (a cross-crate edge downward), it names nothing back, and no mutual pair moved (17/17).

**The HAL's stages S2–S5.** §11.4's table records each stage's capability, pairs and gate, and its S4
finding records the register-frame split; beyond those: S2 has the serial driver name
`Line::{Timer,Serial}` rather than PIC bit 4 and the net driver ask `net_msi_vector` rather than name
`idt::NET_IRQ_VECTOR`; S4's duplicated register-file field order (`arch/x86_64/context.rs` and
`syscall/linux/abi.rs`) is checked every boot in `M1`, which asserts nineteen named indices and the
count against the ABI's table; S5's ratchet added the third rule — `crate::arch::` references from a
non-machine layer (`sched` 17, `syscall` 4, both may only fall); S5 made `src/arch/mod.rs` a
four-item facade (`kernel_sections`, `breakpoints_caught`, the two register-table constants) and
exposed the dead `gdt::user_code`/`user_data`, now gone. Gate for S2–S5:
`pass=142 fail=0 (142 built)` exit 33.

---

### R.11.F — the two largest files stop being single files: a split by responsibility, and what a mechanical move quietly dropped

`sched/runtime.rs` (9 099 lines) and `drivers/drm.rs` (8 593 lines) were the two largest files
in the tree, and the two §11.1.11 named as long *because each holds several subsystems'
concerns* rather than because its subject is large. Both are directories now, one child per
responsibility, with `mod.rs` keeping the module's own header and re-exporting the same names:

- `sched/runtime/` — `task.rs` (the task table, `Task`, `Context`/`Stack`, `Layout`, the
  scheduler loop, the timer and rlimit scans), `child_mm.rs` (the `Mm` carrier, the page-fault
  resolvers, the user-memory and mmap accessors), `child_signal.rs` (delivery, stops, the wait
  queues and the child-event table), `child_exit.rs` (`spawn`/`fork`/`clone`/`vfork`/
  `exec_replace`, exit, reaping and the orphan handover), `load.rs` (image loading, the `#!`
  chain, the pid-1 handoff), `trace.rs` (ptrace) and `procfs.rs` (the snapshots `/proc` reads).
- `drivers/drm/` — `abi.rs` (the UAPI vocabulary: ids, request numbers, caps, flags),
  `state.rs` (`STATE` and the object tables), `ioctl.rs` (`drm_request`, `arg_shape`, the
  dispatch), `card.rs` (the two device nodes), `props.rs` (KMS enumeration, the property doors,
  the atomic commit), `fb.rs` (buffers and framebuffers), `flip.rs` (page flip and present),
  `sync.rs` (`SYNCOBJ_*`), `master.rs` (master, magic, leases) and `virtgpu.rs`
  (`DRM_VIRTGPU_*`).

Nothing outside either module changed name: `drivers::drm::drm_ioctl` is
`pub use ioctl::drm_request as drm_ioctl`, `sched::runtime`'s export list is the same list, and
every `crate::` reference in the tree still names the module it named before.

**What the mechanical move dropped, and what caught it.** A move cut along blank-line blocks is
not behaviour-preserving by construction, and this one lost two things. Two rows of
`arg_shape` — `NR_MODE_GETGAMMA | NR_MODE_SETGAMMA => (32, true)` and
`NR_CRTC_GET_SEQUENCE | NR_CRTC_QUEUE_SEQUENCE => (24, true)` — went with the block they sat in,
and their absence is *invisible to the compiler* because a `match` has a catch-all: the gamma
pair was still refused (`EINVAL`, the refusal's own answer) and the sequence pair still fell to
`ENOTTY`, so only the *ordering* Linux's ioctl layer guarantees was gone. `t-drm-args` holds
exactly that ordering — "a poisoned GETGAMMA argument is EFAULT before the refusal", "a poisoned
CRTC_GET_SEQUENCE argument is EFAULT, not ENOTTY" — and the first gate run came back
`pass=170 fail=1` with those two claims as the only red, which is the pin doing the job rule 6
keeps it for. The second loss was dead and therefore silent in the other direction: a private
`Mm::vma_take_range` with no caller anywhere at HEAD, which is why no test could have missed it.
Both are restored, and the restore is what turned the gate green (`pass=171 fail=0`), which is
the counterfactual: the two rows are what those two claims rest on.

What made the loss *findable before* the gate — and the reason to write it down — is two cheap
multiset comparisons over the old file and the concatenation of its children: the **identifier**
counts, and the set of **match-arm patterns**. Both are blind to indentation, visibility and
`super::abi::`-style path rewrites, and both point straight at what is missing: the identifier
pass showed `NR_MODE_GETGAMMA`/`NR_MODE_SETGAMMA`/`NR_CRTC_GET_SEQUENCE`/`NR_CRTC_QUEUE_SEQUENCE`
each down by exactly one and `vma_take_range`/`take_range` gone entirely, and the arm pass
showed the two rows and nothing else. A *symbol-name* comparison — the obvious first check, and
the one that was run first — does not catch either, because neither a dropped arm nor a dropped
private helper changes the set of names the file declares.

**Both arms, measured.** x86_64 `=== SUMMARY pass=171 fail=0 (171 built)`, exit 0, with the
kernel's own `mpos-kunit 32 passed, 0 failed`; aarch64 `=== SUMMARY pass=170 fail=0`, the same 32
cases, exit 0. Host unit tests and the xtask suite run inside that sweep; clippy reports 0 errors
on `x86_64-unknown-none`, `aarch64-unknown-none`, the host lib and the aarch64 loader;
`cargo xtask depgraph` is within the ratchet — a move *inside* a module cannot add an edge, which
is the one thing this refactor never had to argue for.

**Where the name lives now.** The live pointers moved with the split: the file tables in
`docs/modules/sched.md` and `docs/modules/drivers.md`, `ARCHITECTURE.md`'s carrier examples, the
two README trees, and the ~20 source comments that named `drivers/drm.rs` or `sched/runtime.rs`
as the home of a symbol. `docs/notes/`'s own citations were deliberately left alone — they record
what was measured when, which is the precedent Step E's `fs.rs` split set and the reason a
`§N.M` pointer is a routing address rather than a file path.
