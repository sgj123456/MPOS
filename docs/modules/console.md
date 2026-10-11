# console

**Home:** `src/console.rs`, `src/logging.rs` · **Owns:** the kernel log ring, the wire(s) it reaches, and the move onto the framebuffer

## What it owns

The console is the kernel's voice, and this module is the one place that decides what that voice is
made of and where it lands. `src/console.rs` wraps the machine's raw sink - reached only through
`hal::Console`, never a port or a register - with interrupt-safe formatting (`print_args`, the body
`println!` expands into), the log ring userland files into and `syslog(2)` serves (`DMESG`,
`log_bytes`, `dmesg_snapshot`, `dmesg_take`, `dmesg_clear`, `dmesg_capacity`), the replay ring that
lets a framebuffer be brought up already showing the boot log (`LOG`, `record`, `takeover`), and the
console-list state that answers which sinks this boot actually has (`set_console_list`,
`serial_is_sink`, `vt_is_sink`, `gpu_mode`, `grids_live`, `vt_live`).

The two rings have different audiences and that is why they are separate. The serial console carries
the *kernel's* voice; the `DMESG` ring carries userland's filed notes, which Linux deliberately keeps
*off* the console - an init that echoes its banner into `/dev/kmsg` *and* prints it to stdout would
otherwise appear on the screen twice, and every boot after this one has such an init. The `LOG` ring
is a third thing again: a bounded copy of what the console has already said, so `takeover` can replay
it and the screen is not born mid-story.

`src/logging.rs` is the layer above `println!`: the `tracing` subscriber that turns an event into one
line, plus the level vocabulary (`rank`, `level_from_cmdline`). It exists because
`tracing-subscriber` needs `std` and, for its formatting layers, an executor, and the kernel has
neither; what a kernel needs from a subscriber is one thing - turn an event into a line - and the
event machinery itself is `no_std` with `default-features = false`. Its `MAX_LEVEL` gate is
`target_os = "none"` alone (not an architecture), because the sink it needs is `console::write_bytes`
and the clock it needs is `hal::ticks`, and both machines have had those since stages 1c and 4; the
gate used to name x86_64 with a premise that had expired, which silently dropped every `tracing` line
on the second machine (`docs/notes/` §13.23).

What deliberately does **not** live here: the line discipline (`vfs::tty`'s `LineDiscipline`), the
settings it obeys (`vfs::termios`'s `Termios`), the multiplexer and its switch negotiation
(`vfs::vcmux`, with the pure table in `vfs::vtmode`), the text grids (`term`), and the DRM card
(`drivers::drm`). The console owns the *wire* and the *sinks*; the tty owns what happens to a byte
before it is written, and the mux owns which of six terminals is on the screen. Where the mux needs
something the console owns - the screen, the render task, the card handover - it asks through this
module's `display_*` seams (`display_notify`, `display_master_held`, `display_handover`,
`display_card_grabbed`), which are no-ops with honest answers on a machine with no framebuffer. That
seam is what let the mux leave the first machine's body and compile for both (`docs/notes/`
§13.16).

The `console=` list is parsed in `src/boot/console_list.rs`, not here, because the parse runs at the
top of the boot task before the heap exists and must be allocation-free; what lands here is the
result, through `set_console_list`. Linux's rule is followed exactly - `console=` may repeat, output
goes to every console it names, the *last* one is preferred, and a value naming a console the machine
does not have is reported and ignored rather than fatal. Only two sinks are recognised, each spelled
the way its machine spells it: the serial wire (`ttyS0` on x86_64, `ttyAMA0` on aarch64) and the
virtual console currently on the framebuffer (`tty0`). `tty1`..`tty6` promise a *pinned* VT, and this
console always writes to the VT that is on screen, so answering those words while doing something
else would be the silent approximation the repository refuses; they are reported as unknown to the
caller and recorded as named gaps instead.

The console's **write policy** is one of the few places this kernel consciously deviates from Linux,
and the code says so. `wire_bytes` writes the *preferred* sink through the parking path
(`serial_bytes` -> `submit_and_park`) so nothing is lost, and writes a peer sink best-effort
(`serial_mirror_now`), handing bytes to the transmit ring and returning, dropping whatever the ring
cannot hold. The point is that a host terminal which cannot keep up must never throttle the console
somebody is actually reading. A best-effort cut is decided by `term::wire::cut` so a dropped write
never lands inside an escape sequence, which would leave the host terminal out of step. Which sink is
preferred follows the command line, not a mirror flag - and that is what replaced the old
`mirror`/`nomirror` pair: `console=ttyS0` alone makes the wire preferred, so every line is lossless;
`console=ttyS0 console=tty0` keeps the wire authoritative until the framebuffer console has actually
taken over, which keeps a boot's earliest lines lossless.

The **move onto the framebuffer** is a single atomic flip. `BACKEND` is `Serial` for the whole boot
and is flipped once by `takeover`, when the render task has a screen and the framebuffer console is
one of the consoles; `set_gpu` refuses the flip when `vt_is_sink()` is false, which is the old
headless veto restated in the terms the command line actually uses. `takeover` replays the `LOG` ring
onto the freshly made grid *before* switching, so there is no window between "the GPU is the console"
and "there is a screen to paint". The write path distinguishes two separate questions that look like
one on a normal boot: `gpu_mode()` is "the framebuffer is the console the kernel writes to", while
`grids_live()` is "one screen exists and the render task can paint it". They are the same answer on
every boot but a compositor's, and the split exists because on a `gui` boot the kernel log stays on
the wire (the scanout belongs to the compositor) and yet the text VTs still have to be *somewhere* -
a text VT the compositor has released is one somebody has to be able to read. `vt_live()` is what a
subsystem that must decide whether an event belongs to a shell (the keyboard is the one caller) is
told, and it is deliberately not `vt_is_sink()`.

`force_serial` is on the panic path: it flips the backend back to the wire *and* makes the wire a
sink even on a boot whose command line never named it, because a report that reached nobody is not a
report.

## Key files

| File | What lives there |
|---|---|
| `src/console.rs` | the whole console service: `println!`'s formatting, the `DMESG` and `LOG` rings, the console-list state, the backend flip, the wire write policy, and the `display_*` seams the mux asks for |
| `src/logging.rs` | the `no_std` `tracing` subscriber, `rank`, `level_from_cmdline`, and the `MAX_LEVEL` gate |
| `src/boot/console_list.rs` | `ConsoleSink`/`ConsoleList`, the allocation-free `console=` parse, and the Linux rule it follows |
| `src/vfs/tty.rs` | the line discipline and the `Tty` node; the console owns the wire, this owns what a byte means before it is written. `Tty::write_at`, `receive_byte` and `kernel_draws` are where the two meet |
| `src/vfs/vcmux.rs` | the console multiplexer: one wire, six terminals, the `^A` switch and the `VT_PROCESS` handover. It owns the *display* questions and asks the console for them |
| `src/vfs/termios.rs` | the `struct termios` and `termios2` wire layouts the `TCGETS`/`TCSETS` family exchanges |
| `src/board/steps.rs` | `arm_console`: the one order in which the mux goes up and the receiver is armed (`vfs::vcmux::init` then `arch::enable_console_interrupts`) |
| `src/board/x86_64.rs` | where the x86_64 boot calls `console::init()` and `console::set_console_list(ConsoleList::from_cmdline(...))` |
| `arch/src/hal/mod.rs` | the traits and the forwards: `Console`, `SerialRing`, `ConsoleInput`, and `console_init`/`console_write_bytes`/`console_interrupt_driven`/`serial_tx_*` |
| `arch/src/hal/tx_ring.rs` | the machine-neutral transmit queue both machines drain, parameterised by a `tx_ring::Transmitter` |
| `arch/src/arch/x86_64/serial.rs`, `arch/src/arch/aarch64/pl011.rs` | the two machines' wires; each implements `hal::Console` and `hal::SerialRing` |

Note: comments in `src/console.rs` still name `drivers::serial` as the module that keeps no lock; the
wire now lives in `mpos-arch` (`arch/src/arch/*/serial.rs` / `pl011.rs`) behind `hal::Console`, so
that name is stale (an observation from reading the tree, not something the code states).

## Contracts

The **machine seam** is `hal::Console` (baseline: `init`, `write_bytes`, `interrupt_driven`) plus the
optional `hal::SerialRing` (`tx_space`, `tx_submit`, `tx_wait_now`, `tx_stop_waiting`) and
`hal::ConsoleInput` (`poll_input`, `inject_input`), each gated on the `mpos_*` capability flag the
build declares per target rather than on an architecture name. The console's own entry points are
`init`, `print_args`, `write_bytes`, `vc_output`, `takeover`, `force_serial`, `set_console_list` and
the query functions above; the log ring adds `log_bytes`/`dmesg_*`; logging adds `init`,
`level_from_cmdline` and `rank`.

The invariants each entry point keeps:

- The backend and the three sink flags are plain atomics, never a lock, because `write_bytes` is
  reached from the serial interrupt (the same reason the machine's serial driver keeps no lock). That
  is what makes them safe to read on the IRQ path.
- `wire_bytes` writes the preferred sink losslessly and peers best-effort, with `term::wire::cut`
  deciding where a partial write may end.
- `takeover` replays the `LOG` ring before it flips the backend, and `set_gpu` refuses the flip
  unless the framebuffer console is one of this boot's consoles.
- `force_serial` both flips the backend and makes the wire a sink, so a panic report is read.
- `vc_output` is a *virtual console's* own bytes (the shell on `/dev/ttyN`), distinct from
  `write_bytes` (the kernel console): a flood on a background console updates that console's grid and
  is simply not on screen until you switch to it.

The **console list** contract is `ConsoleList::from_cmdline` (pure, so a host test can assert what a
line yields), `DEFAULT`, `has_serial`, `has_vt`, `prefers_serial`. The edges - who may call this
module and whom it may call - are `docs/ARCHITECTURE.md` §2/§3; the concurrency rules it relies on
are §7.

## What userspace sees

The console's userspace surface is shared with the tty and is owned jointly with `src/vfs/tty.rs`.
`docs/uapi/` carries the rows: the `0x54xx` termios/`TIOC*` family (owner `src/vfs/tty.rs`,
`src/vfs/pty.rs`), and the `'V'` (VT) and `'K'` (KD) families, served by `Tty::vt_ioctl` and
`Tty::kd_ioctl` with their state in `src/vfs/vcmux.rs`. The console tty is published under the
machine's own serial name, and the kernel log is reachable through `syslog(2)` (pinned by `t-syslog`)
even though `docs/uapi/` has no dedicated `syslog` row - a gap in the inventory rather than in the
kernel (inference: a grep of `docs/uapi/` finds no `syslog`/`klogctl` row).

Two console gaps are named rather than approximated, in `docs/uapi/`: `console=ttyN` pinning is
refused, and `/dev/console` has no preferred-console plumbing (`docs/notes/` §7.1c). The serial
transmit wait is fatal-signal-only, like `wait4` (docs/uapi/ §1.2).

## Pins

| Pin | Kind | What it would fail on |
|---|---|---|
| `src/boot/console_list.rs` tests | host | `a_line_with_no_console_word_keeps_the_default_list`, `a_repeated_parameter_names_every_console_and_the_last_is_preferred`, `one_word_picks_one_console`, `a_console_the_machine_does_not_have_is_reported_and_dropped`, `an_aarch64_serial_console_names_the_wire_and_is_preferred`, `an_aarch64_extra_port_is_reported_unknown_like_its_x86_twin` |
| `src/logging.rs` tests | host | `levels_compare_by_verbosity`, `the_command_line_turns_logging_up_and_down`, `the_message_is_kept_apart_from_the_fields` |
| `src/vfs/termios.rs` tests | host | `a_stored_termios_reads_back_as_itself`, `a_termios2_carries_its_speeds_at_the_end_of_forty_four_bytes`, `the_console_default_is_a_line_at_a_time_and_echoed` |
| `src/vfs/tty.rs` tests | host | the `LineDiscipline` suite, and above it `the_report_and_the_screen_never_disagree_about_the_cursor`, `a_query_to_a_terminal_the_kernel_does_not_draw_travels_unanswered`, `a_query_to_a_screen_the_kernel_draws_is_answered_and_kept_off_the_wire`, `a_console_reports_its_screens_window_and_others_do_not` |
| `t-tty-query` | `user/tests` | which questions the console answers and which it forwards, per who draws the screen |
| `t-tty-vtime` | `user/tests` | the `VMIN`/`VTIME` blocking read table (the source of `docs/notes/` §12.4a) |
| `t-tty-nonblock` | `user/tests` | the same table with `O_NONBLOCK` |
| `t-tty-eintr` | `user/tests` | a trapped signal ending a blocked read |
| `t-vt` | `user/tests` | the `'V'`/`'K'` family on both machines, and the mux switch visible on the wire |
| `t-pty` | `user/tests` | the pty pair and its terminal ioctls (the other wire a `Tty` speaks to) |
| `t-syslog` | `user/tests` | the `DMESG` ring's `syslog(2)` answers |
| `t-fbdev`, `t-seat`, `t-drm` | `user/abitest` integration | the framebuffer, the seat manager's `VT_*`/`KD_*` handover, and the DRM card the console hands the scanout to |

`src/console.rs` and `src/vfs/vcmux.rs` have no `#[cfg(test)]` module of their own (grep), so their
logic is pinned through the callers above - the console list, the termios layouts, the discipline,
and the machine tests. `t-aa-ptyprobe` in `user/tests` describes itself as a temporary diagnostic
("Deleted before the batch is committed"), so it is not counted as a pin.

## Evidence and open work

The reasoning behind the framebuffer takeover, the mux port to both machines and the logging gate is
in `docs/notes/` §13.16 and §13.23; the framebuffer console's own landing is §13.26; the `t-tty-vtime`
freeze is §12.4a, and its read-path sibling note is the same section. The console-list design (why a
list, not a "headless" flag) is the module header of `src/boot/console_list.rs`, and the `console=`
gaps are `docs/uapi/`'s "Console gaps" bullet, whose reasoning is `docs/notes/` §7.1c. `docs/ARCHITECTURE.md` §2/§3 own the
edges and §7 the concurrency rules, and `docs/ROADMAP.md` owns what is left: the `/dev/console`
preferred-console plumbing and `console=ttyN` pinning.
