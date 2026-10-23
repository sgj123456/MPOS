# board

**Home:** `src/board/` · **Owns:** this machine's boot wiring, its devices and its milestone checks, and the five steps every port's `init` runs.

## What it owns

`src/board/` is the kernel's *wiring* layer: everything between "a machine is running on the
loader's tables" and "a userspace owns the machine". `src/main.rs` enters it through `board::start`,
the same `start` on both ports; `mpos_arch::arch::boot` calls back into it three times -
`milestone`, `devices`, `init`. The layer exists because those answers *name* `vfs`, `blk`, `net`
and `drivers`, all above the machine layer: `mpos-arch` cannot see any of them (`AGENTS.md` §3,
downward-only), so the boot's remaining work cannot live there.

The module is split into a *machine* half and a *shared* half. The machine half is one body per port,
chosen in exactly one place: `src/board/mod.rs` declares `mod x86_64` / `mod aarch64` behind
`#[cfg(target_arch = …)]` and re-exports that body's `start` - the same arrangement
`arch/src/arch/mod.rs` uses. A body holds only what a different machine would answer differently:
its bus or device tree, its UART paths, its milestone order.

The shared half is two modules and the harness:

- `steps.rs` is the shared *work*. Five steps every port's `init` takes - `start_scheduler`,
  `mount_ramfs_root`, `unpack`, `arm_console`, `hand_to_init` - each carrying an ordering contract
  that used to be written twice. It also holds the steps only some boots reach (`bring_up_net`,
  `mount_share`, `mount_disk_root`, `register_exit_hooks`). They live in the *wiring crate*, not the
  library: `arm_console` calls `crate::arch::enable_console_interrupts`, which only the wiring may
  name, and the library's `boot` module was tried first and rejected because `vfs` already names
  `boot` (the command line behind `/proc/cmdline`), so `boot` reaching back for `sched`/`vfs` would
  close a mutual edge (`docs/notes/` §14.9).
- `report.rs` is the shared *reporting* - the boot banner, the boot-stack canary check, `park`, and
  the two things a dying kernel says (`report_stack_use`, `panic_context`). Nothing in a report is a
  machine's to answer differently, so it is one copy of functions of `BootInfo`, the scheduler and a
  `hal` register read.
- `kunit.rs` is the *harness*, the only thing here behind `--features selftest`: `SUITES` is the one
  list of what a self-test boot checks, and `reached(phase, info)` runs the suites whose phase it
  is. The suites sit beside the modules they test - `mm`'s, `sched`'s, `vfs`'s, `syscall`'s two, and
  the boot's own in `report` - which lets one read that module's private state; the neutral milestone
  checks went there (`docs/notes/` §15.19-§15.23). `checks/` no longer exists.

What deliberately does **not** live here: the entry, the descriptor tables, the page tables, the wire
and the device description - those are `mpos-arch` (`docs/notes/` §14.8); the architecture-independent
entry `_start` and the panic handler, which are `src/main.rs`; and the handoff record, the
`mpos-bootinfo` leaf the loader and the kernel share (see [boot](boot.md)).

## Key files

| File | What lives there |
|---|---|
| `src/board/mod.rs` | The one place a machine's body is chosen (`#[cfg]` mod arms + `pub use …::start`); `panic_context`'s re-export; the `compile_error!` that makes a machine with no body fail to link |
| `src/board/x86_64.rs` | The x86_64 body: `start`, the `milestone`/`devices`/`init` callbacks, `init_mm`/`init_vfs`/`init_blk`/`init_net`/`init_virtio`, `check_arch` (M1/M3), and `handoff_to_init` (M8) |
| `src/board/aarch64.rs` | The aarch64 body: `start`, the same three callbacks, virtio-MMIO discovery and its `check_*` family, `MachineWriter`, `init_net`, and the console/device verdicts (`check_dev`, `check_console_rx`, `check_serial_tx`, `check_input_registry`, `check_console_dev`, `check_gpu_dev`, `check_sound_dev`, `check_p9_dev`) |
| `src/board/steps.rs` | The five shared steps and their ordering contracts, plus `bring_up_net`, `mount_share`, `mount_disk_root` and `register_exit_hooks` |
| `src/board/report.rs` | `banner`, `check_boot_stack`, `park`, `report_stack_use`, `panic_context`, and the boot's own suite (`SUITE`: `boot.stack_depth`) |
| `src/board/kunit.rs` | The harness for a test build: `SUITES` and `reached`, which a body calls at each phase it reaches |

## Contracts

- `board::start(handoff: *const Handoff) -> !` (re-exported per machine) - `src/main.rs`'s `_start`
  hands it to `mpos_arch::arch::boot` as *the* entry, and it never returns.
- The three callbacks - `milestone`, `devices`, `init`. `arch::boot` calls the pointers but names
  none of them; `milestone` runs before the heap (console service, console list, banner), `devices`
  runs once the kernel's page tables reach the bus or MMIO windows, and `init` ends inside the pid-1
  handoff.
- The five steps in `steps.rs`, each carrying its ordering contract: the scheduler adopts the boot
  context as task 0 before anything spawns; the tty table exists before the standard file table and
  `/` before any check resolves a path; the unpack lands in that root; the console mux goes up
  *before* the receiver is armed; and the first program the scheduler starts *is* pid 1
  (`hand_to_init` asserts it).
- `register_exit_hooks` - the one exit-hook registry, in the release order the descriptors need
  (locks first, then socket/inotify/evdev/uinput); the two input hooks are gated on the
  `mpos_input_registry` capability, not an architecture.
- `report::panic_context` - called from `src/main.rs`'s panic handler; one definition for both
  machines, over `hal::stack_pointer` and `arch::kernel_sections`.
- `report::park` - blocks through the scheduler rather than halting, so a wake does not wait for the
  next tick.

Who may call this module, and which edges exist, are `docs/ARCHITECTURE.md` §2/§3 (the `Board` row
and the depgraph's allow-list include `ARCH_EXEMPT` for the `arch` reach in `steps::arm_console`).
The boot *sequence* is `docs/ARCHITECTURE.md` §4.

## What userspace sees

`board` owns no `docs/uapi/` rows - it is the wiring, not an ABI surface. What it decides of the
userspace's world is reached through rows another module owns: `/init` becomes pid 1 (so
`/proc/1/cmdline` exists), the aarch64 body builds and mounts `/dev` (`check_dev`, the
`standard_devfs` set the devfs rows describe), and both bodies hang the boot disk at `/mnt` and the
host share at `/mnt/share` (so `/proc/mounts` and `statfs` name them). The `console=` policy the
x86_64 `milestone` installs is `boot::console_list`'s; its gap is docs/uapi/ §5.3 (`console=ttyN`
pinning refused, `/dev/console` has no preferred-console plumbing; `docs/notes/` §7.1c).

## Pins

`src/board/` contains **no `#[cfg(test)] mod tests`** - verified by reading every file under it and
grepping for `cfg(test)`. It is the one module whose truth is only observable on a booted machine,
so its pins are the milestone checks the bodies and suites run:

| Pin | What it would catch |
|---|---|
| `mm`'s suite (`frames`, `large_block`, `heap`) | allocator, frame, buddy-merge and heap contracts |
| `sched`'s suite (`alternating` … `stacks_returned`) | round-robin, preemption, sleep, idle wake and priority (asserted on a run record, not a log) |
| `vfs`'s suite and `syscall`'s gate cases | the dispatch gate, ramfs root, a crossed mount, two tasks on one fd |
| `ipc`'s suite (in `src/syscall/kunit.rs`) | pipes, tty line discipline, signal delivery |
| `report`'s suite (`stack_depth`) | the boot stack's canary once the boot's own work is done; the two production calls in the bodies cover the handoff |
| x86_64's two arm cases (`clock_breaks_registers`, `page_tables_and_spaces`) | clock/`int3`, the register-file ABI agreement, W^X and user address spaces |
| x86_64 `handoff_to_init` | `/init` runs as pid 1, forks, execs a shell |
| aarch64 `check_virtio`/`check_rng`/`check_blk`/`check_fs`/`check_net`/`check_input`/`check_console`/`check_sound`/`check_vsock`/`check_p9` | the MMIO device classes, each failing by name and halting |
| aarch64 `check_dev`/`check_console_rx`/`check_serial_tx`/`check_input_registry`/`check_console_dev`/`check_gpu_dev`/`check_sound_dev`/`check_p9_dev` | the mounted `/dev`, the console RX/TX paths, the registry and the device nodes - the two console ones *test-build-only* now (`#[cfg(feature = "selftest")]`), because the harness was their only caller |

Two caveats, both from the code. First, whether a machine runs its checks is decided by which *image*
was built (`--features selftest`, `src/board/kunit.rs`) rather than by a command-line word: a kernel
from `build`/`run`/`disk` carries neither the suites nor the harness, and the `mpos.selftest` word
the kernel used to parse for that decision is gone (`docs/notes/` §15.19). Second, a boot's verdict is
the sweep's `=== RUN/PASS/FAIL/SUMMARY/END` plus the harness's tally; the `M*`-shaped markers the
checks used to print are gone - nothing had parsed them since §15.6/§15.7 retired that command, and a
marker a reader cannot tell from a mechanism is worse than a sentence (`docs/notes/` §15.24).

No `t-*` program pins this module directly: grepping `user/` for `\bboard\b` finds nothing (the
earlier substring matches are all `keyboard`). The closest adjacent pin - and it pins
`steps::mount_share`, not the module - is `user/tests/src/bin/t-9p.rs`, whose header says it reads the
boot's mount (`/mnt/share/mpos-9p-probe`) that `steps::mount_share` hangs. The handoff's programs
cover the *result* of the wiring on both machines indirectly, which is the shape `AGENTS.md` §3.6
warns about: the milestone checks are what actually exercises `board`.

## Evidence and open work

The reasoning, and where each claim was measured, is in `docs/notes/`: §14.9 (why the five steps are
stated once and live in the wiring crate, not the library), §14.8 (the machine's half moved into
`mpos-arch`), §13.23 (the neutral checks and shared reporting), §13.21 (the aarch64 trap stubs the
checks depend on), §13.14/§13.15 (a check must not damage the machine it checks - the year-36812
lesson), §13.24 (a capability flag is only cheap if everything behind it is gated on the capability),
§13.29 (the 9p probe that was never called), §15.6 (the initramfs flow both bodies share, and the
marker that stopped being a kernel marker), §15.10 (the disk is registered through the driver, not
written out per body), §15.18 (`find_filesystem` for a two- or three-partition disk), §9.1 (what a
port needs) and §7.3 (a machine can carry two disks under two names). The plan and status are
`docs/ROADMAP.md` Phase 13 (the boot-side half is Phase 15) and the Phase 15 block; the boot sequence
is `docs/ARCHITECTURE.md` §4. This section restates none of their numbers on purpose.
