# syscall

**Home:** `src/syscall/` · **Owns:** the ABI gate — the syscall numbers, the dispatch table that maps them onto handlers, the validation of every user pointer that crosses the boundary, and the errno shape each refusal takes

## What it owns

This is the one layer that speaks Linux's calling convention to a binary the
kernel did not compile. The register shape is Linux's: number in `rax`,
arguments in `rdi, rsi, rdx, r10, r8, r9`, result in `rax`, errors as negative
errno values — and `rcx`/`r11` are deliberately absent from the argument list
because the `syscall` instruction destroys them (`src/syscall/mod.rs`'s header).
`ARG_COUNT` is 6, and every handler takes `[usize; ARG_COUNT]`.

The module is split along the line `no_std` draws everywhere: the *number table*
and the *pure ABI half* compile and are tested on the host, while the dispatch
and the handlers need a console and a scheduler under them and exist only on the
machine (`src/syscall/mod.rs`'s header). The errno table itself lives at the
crate root as `crate::err` and is re-exported here, so a layer *below* `syscall`
can refuse in Linux's language without reaching up into the dispatch table. The
user-buffer validation primitives (`buffer_ok`, `MAX_IO`, `err_of`) live at
`crate::uaccess` for the same reason and are re-exported here for the handlers
that already named them.

Three rules give the module its character, and all three are stated in
`AGENTS.md` §3. First, **answer truthfully or refuse**: a call this kernel does
not implement falls through to the `_ => ENOSYS` catch-all in
`src/syscall/linux/mod.rs`, and that arm is deliberately crowded — returning
success for something not implemented is the single worst thing this codebase can
do. Second, a missing call is worth *saying out loud once*: dispatch calls
`note_enosys(nr)` (and `note_enosys_ioctl` for the one number where the request,
not the number, is the information) so a reader of the wire can tell an honest
refusal from a silently dropped call. Third, the man-page contract is a fact and
therefore belongs where the host can test it — flag values, struct layouts and
argument shaping live in `linux::abi`, and a mistyped bit or offset reads as a red
host test rather than a mystery on the machine.

The third piece the module owns is the panic trail (`src/syscall/mod.rs`'s
`trail` module, machine-only). A panic names where the damage was *noticed*,
which in a kernel is routinely a whole syscall after the write that caused it, so
dispatch records the last few calls — number and the first two arguments — in a
small relaxed-store ring that the panic handler dumps.

## Key files

| File | What lives there |
|---|---|
| `src/syscall/mod.rs` | `linux_dispatch` (the machine entry: trail, `check_itimer`, ptrace stops, the signal-delivery point on the way out), `ARG_COUNT`, `MMAP_BASE`/`DEFAULT_UMASK` re-exports, `cstr_arg`/`argv_arg`/`MAX_PATH`/`MAX_ARG_STR`, the `err`/`uaccess` re-exports, and the machine-only `trail` ring |
| `linux/mod.rs` | `dispatch(nr, args, from_user, regs_ptr)`: the seccomp `STRICT`/`FILTER` pre-checks, the whole `match nr` table of handler arms, the `_ => ENOSYS` catch-all, and the ENOSYS/EOPNOTSUPP console notes |
| `linux/nr.rs` | The number module: `include!`s one arch's table (below), re-exports the `CLONE_*` flags from `mpos-abi`, and carries the host test that pins every x86_64 number |
| `linux/nr_x86_64.rs` | The x86_64 numbers the `include!` compiles on the host and on that target |
| `linux/nr_aarch64.rs` | The aarch64 (`asm-generic`) numbers — a different set as well as different values; a name arm64 has no number for gets an unreachable sentinel |
| `linux/abi.rs` | The pure ABI half: flag words, `struct stat`/`statx`/`statfs` writers, `user_regs`/`fpregs` shapes, the `AT_*`/`F_*` vocabularies, and the handlers whose whole behaviour is table answers (`handle_prctl`, `handle_umask`, `handle_getrandom`, the rlimit and affinity calls); host-tested |
| `linux/fs/mod.rs` | The seam between the two file halves (NOTES §11.2 Step E): `sync_nonblocking` and the re-exports, so dispatch still names `fs::<handler>` |
| `linux/fs/file.rs` | The path-carrying calls in Linux argument shapes (the `*at` family, `read`/`write`/`readv`/`writev`, `fcntl`, `poll`, `epoll`, `futex`, the xattr entry points, dup/pipe/statfs) |
| `linux/fs/ioctl.rs` | The device-`ioctl` entry point: `handle_ioctl` decoding the request word by its `_IOC_TYPE` magic and routing to the console/DRM/evdev/uinput/fb/rtc/netlink/interface families |
| `linux/proc.rs` | The handlers that need a running machine: memory (`mmap`/`munmap`/`mremap`/`mprotect`/`brk`), time (`nanosleep`, `clock_gettime`, `setitimer`), identity, `execve`, `clone`/`wait4`, `futex`, and signal delivery (`deliver_signal`) |
| `linux/posix.rs` | The POSIX completion set as table answers and validations: identity and capability changes, scheduling policy, memory advice, the wall-clock setters, `pause`/`rt_sigsuspend` |

## Contracts

`linux_dispatch` is the machine's single entry into the ABI — the syscall/trap
gate reaches it through the kernel-services hook table
(`crate::kernel_services`'s `dispatch_syscall`, `docs/notes/` §14.2), and the boot
milestone checks call it directly — and `dispatch` is the table underneath it;
both are
`unsafe` because `regs_ptr` must be the gate's own frame for the calling task.
`deliver_signal` is the one unusual return path — called on the way out, it
rewrites the task's register frame so the return lands in a signal handler
instead of the interrupted instruction, which is why signal delivery happens
*after* the call's result is written. The host-compiled half's contract is that
the handlers call the items in `linux::abi` rather than restating them, so a host
test is about the code that runs and not about a copy of it.

**Where the numbers live, and why the test suite `#[path]`-includes `nr.rs`.**
`linux/nr.rs` holds no numbers of its own: it `include!`s `nr_x86_64.rs` or
`nr_aarch64.rs` by `#[cfg(target_arch)]`, because the two machines share neither
a numbering nor a set (arm64 drops the legacy `open`/`stat`/`poll`/`select`/`fork`
calls entirely). Both files spell the same `pub const` *names*, so the dispatch
arms and every caller keep addressing `nr::NAME` unchanged and only the values
differ by machine. The conformance harness `user/abitest/lib/lin.rs` declares
`#[path = "../../../src/syscall/linux/nr.rs"] pub mod nr;` — it does not list
numbers of its own. The reason is that the numbers *are* the ABI: a wrong
constant silently calls the wrong handler, and nothing below trusts a check. By
including the kernel's own file instead of copying it, kernel and tests cannot
disagree about a number without one of them failing to build.

**Adding a syscall is a recipe, and the recipe is `AGENTS.md` §4** — it is the
authoritative sequence and is deliberately not duplicated here. In outline the
recipe is: the number and `pub const` beside the other numbers (in the arch table
`nr.rs` includes), the dispatch arm in `linux/mod.rs`, the handler in
`fs/file.rs` (path/file/device calls) or `fs/ioctl.rs` (the device-`ioctl` entry)
or `proc.rs` (memory/process/time) or `posix.rs` (identity/capabilities/
scheduling/clocks), validation with the handler and pure facts in the
host-compiled `abi.rs`, a conformance program with its `[[bin]]` entry, and an
update to `docs/ROADMAP.md`/`docs/uapi/`/`docs/notes/` in the same commit. A
call that is not implemented has no arm at all: it falls to `_ => ENOSYS`.

## What userspace sees

Everything. The syscall inventory — the numbers, which are served, which are
deliberately `ENOSYS` with their prerequisite named, and which are served with a
named sub-gap — belongs to docs/uapi/ §1 (with the raw number table in
`linux/nr.rs`/`nr_x86_64.rs`/`nr_aarch64.rs`) and is not restated here. The
device-`ioctl` families this layer routes are docs/uapi/ §2; the struct
layouts it writes are §4; the flag vocabularies are §3. The module's own console
notes (`note_enosys`, `note_eopnotsupp`) are the diagnostic face of the refusal
policy.

## Pins

**Host.** `src/syscall/linux/nr.rs` carries a `#[cfg(test)] mod tests` whose
`syscall_numbers_match_the_x86_64_table` pins every number against the
`syscall(2)` man page's x86_64 column, one assertion per name. `linux/abi.rs` has
a large `#[cfg(test)] mod tests` over the flag verdicts, struct offsets and
argument shaping. `src/syscall/mod.rs` has
`errors_match_the_errno_values_they_name`, pinning the errno constants (the
`nr.rs`/`abi.rs` modules are also pulled into the gate by
`cargo test -p mpos --lib --target x86_64-unknown-linux-gnu`).

**Guest.** On the `std` side (`user/tests/src/bin/`): `t-enosys` (walks the
deliberately-refused numbers and asserts `ENOSYS`), `t-seccomp`, and the ABI-shape
programs `t-fstat`, `t-newfstatat`, `t-statx`, `t-statx-agree`, `t-stat-buffer`,
`t-stat-consistency`, `t-open-flags`, `t-openat`, `t-openat2`, `t-fcntl`,
`t-pipe2`, `t-signal-frame`, `t-signal-delivery`, `t-signalfd`,
`t-rt-signal-queue`, `t-clone`, `t-execve`, `t-futex`. On the bare-metal side
(`user/tests/src/bin/`): `t-ioctl` (the device-`ioctl` entry and its
families) and `t-netlink` (a request word routed through `fs/ioctl.rs` to the net
half) — both std-side since the `no_std` suite finished migrating (§15.11's last
batch deleted `user/abitest`), and `tests.toml` decides which of these the
default sweep runs.

## Evidence and open work

`docs/notes/` is where the reasoning and measurements live: Phase 1 §1 and §1.3
(the syscall-completion pass and the calls left refused), §8.2 (seccomp, whose
pre-checks sit at the top of `dispatch`), §11.2 Step E (the `fs.rs` split into
`file.rs` + `ioctl.rs`), §12.2 (accepted calls whose full contract is not
implemented), and §15.11 (the migration of the `no_std` suite onto the `std`
side, a batch at a time). `docs/ROADMAP.md`'s "P1 Syscall completion" and
"Remaining ABI support" track what is left, and docs/uapi/ §5 is the working
list of served / honestly refused / wrong shape.
