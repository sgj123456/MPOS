# Modules

One document per module, mirroring the source tree. This is the **documentation
structure** for the project: when a module changes, its document here changes with
it, and a new module arrives with its document (the same rule `AGENTS.md` §3
already states for `docs/ARCHITECTURE.md`'s edges and `docs/uapi/`'s rows).

The three docs above this directory answer different questions, and a module page
does not repeat them:

| Question | Where it is answered |
|---|---|
| What is this project, and how do I build it? | `README.md` |
| What is planned, and where does it stand? | `docs/ROADMAP.md` |
| *Why* is the code like this — the measurement, the lesson? | `docs/notes/` (via `docs/notes/index.md`) |
| What are the modules and what may call what? | `docs/ARCHITECTURE.md` §2/§3 |
| What does userspace see, and is it served? | `docs/uapi/` |
| **What does one module own, and how is it structured?** | the page in this directory |

## The template

Every page here keeps the same shape, so a reader can jump between them:

```markdown
# <module>

**Home:** `src/<module>/` · **Owns:** <the one concept this module is the home of>

## What it owns
The concept, in prose. What belongs here and what deliberately does not.

## Key files
| File | What lives there |

## Contracts
The traits / entry points other modules call, and the invariant each one keeps.
The *edges* (who may call this) are `docs/ARCHITECTURE.md` §2/§3 — link, do not copy.

## What userspace sees
The rows this module owns in `docs/uapi/`, when it has any.

## Pins
The host tests and `t-*` programs that would fail if this module's contract broke.

## Evidence and open work
Pointers into `docs/notes/` (§N, routed by `docs/notes/index.md`) and `docs/ROADMAP.md` for the
reasoning and the remaining work. No measurements are restated here — that is what the notes are for.
```

## The pages

| Module | Home | Owns |
|---|---|---|
| [board](board.md) | `src/board/` | this machine's boot wiring, devices and milestone checks; the steps every port's `init` runs |
| [boot](boot.md) | `src/boot/` | the handoff record and the command line the loader leaves behind |
| [console](console.md) | `src/console.rs`, `src/logging.rs` | the kernel log ring, the wire(s) it reaches, and the move onto the framebuffer |
| [drivers](drivers.md) | `src/drivers/` | device discovery (PCI/MMIO), the virtio transports and their devices, and the `/dev` node set |
| [mm](mm.md) | `src/mm/` | physical frames, the slab heap, mapping plans, the ELF loader |
| [net](net.md) | `src/net/` | smoltcp over the device seam, the socket registry, AF_INET/AF_UNIX/AF_PACKET/netlink/vsock |
| [sched](sched.md) | `src/sched/` | tasks: policy, runtime, signals, futex, exit |
| [sync](sync.md) | `src/sync/` | the locking doctrine (`critical_section`, never `spin`) |
| [syscall](syscall.md) | `src/syscall/` | the ABI gate: numbers, dispatch, argument validation, errno shapes |
| [vfs](vfs.md) | `src/vfs/` | inodes, mounts and path resolution; the filesystems and the pseudo-filesystems |
| [misc](misc.md) | `src/{rand,uevent,uaccess,kernel_services}.rs` | the kernel's odd corners: entropy, kobject events, user-memory access, the service table |
| [arch](arch.md) | `arch/` (crate `mpos-arch`) | the machine layer: `hal/`, the two ports, and the loader's `early/` half |
| [loader](loader.md) | `boot/` (crate `mpos-boot`) | the UEFI firmware phase that becomes the kernel |
| [leaf-crates](leaf-crates.md) | `crates/` | the downward-only host-testable leaves |
| [xtask](xtask.md) | `xtask/` | the build/run/test front-end |
| [user](user.md) | `user/` | the programs and the two test suites that run on the machine |
| [fuzz](fuzz.md) | `fuzz/` | the libFuzzer targets over the kernel's parsers |
