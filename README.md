# MPOS

<div align="center">

![Rust 2024](https://img.shields.io/badge/Rust-2024-E8453C?style=for-the-badge&logo=rust&logoColor=white)
![Target x86_64](https://img.shields.io/badge/Target-x86__64-000000?style=for-the-badge&logo=amd&logoColor=white)
![Limine v9.6.7](https://img.shields.io/badge/Limine-v9.6.7-4A90D9?style=for-the-badge&logo=gnu&logoColor=white)
![License MIT](https://img.shields.io/badge/License-MIT-blue?style=for-the-badge)

**A Milestone-Verified Operating System Kernel**

A minimal, milestone-verified operating system kernel written in Rust, booted via the Limine protocol. Every subsystem is proven correct before the next one is built—from the frame allocator to a fully interactive shell.

[Quick Start](#-quick-start) · [Architecture](#-architecture) · [Milestones](#-milestones) · [Building](#-building) · [Testing](#-testing) · [中文](README.zh.md)

</div>

---

## 🧩 Overview

**MPOS** is a teaching and research operating system kernel built around a **milestone verification** methodology. Rather than building the kernel incrementally, MPOS proves each layer correct via assertions and host-side unit tests before building the next one.

The kernel is organized as a **Rust library** (`src/lib.rs`) that can be unit-tested on the host (`cargo test --lib --target x86_64-unknown-linux-gnu`), while a thin binary entry point (`src/main.rs`) walks through the M0–M14 milestone verification pipeline on QEMU.

### Core Features

| Feature | Description |
|---------|-------------|
| 🧪 **Milestone Verification** | Each subsystem (allocator, scheduler, VFS, shell) is proven correct before the next is built. Serial logs carry markers (`M0_OK` through `M13_OK`) asserted by the test harness. |
| 🔬 **Host-Testable Split** | All code not touching machine state lives in the library crate and runs on the host via `cargo test`. Machine-dependent code is gated behind `#[cfg(target_os = "none")]`. |
| 📐 **Write-Through ext2** | A complete read/write ext2 implementation with write-through semantics—no journal, no dirty page tracking, crash-visible. |
| 🏗️ **Priority Scheduler** | Strict priority across four bands (LOW, NORMAL, HIGH, CRITICAL), round-robin within a band, deadline-based sleep queue. |
| 🐚 **Interactive Shell** | A userspace shell with pipes, output redirection, environment variables, background jobs, and a scriptable `init`. |

---

## 🏛️ Architecture

```
mpos/
├── src/
│   ├── main.rs              # Binary entry point, milestone verification M0–M14
│   ├── lib.rs               # Library root (host-testable)
│   ├── arch/                # Architecture-dependent code
│   │   └── x86_64/          # GDT, IDT, paging, serial, timer, syscall gate
│   ├── mm/                  # Memory management
│   │   ├── pframe.rs        # Buddy frame allocator
│   │   ├── heap.rs          # Slab kernel heap
│   │   ├── elf.rs           # ELF64 loader
│   │   └── vm.rs            # Mapping plans, KernelSections
│   ├── sched/               # Scheduler
│   │   ├── policy.rs        # Priority bands, round-robin, sleep queue
│   │   ├── runtime.rs       # Task table, context switch, signals
│   │   └── signal.rs        # Signal numbers (SIGINT, SIGPIPE, SIGTERM, SIGKILL)
│   ├── vfs/                 # Virtual file system
│   │   ├── ramfs.rs         # In-memory file system
│   │   ├── devfs.rs         # Device file system (/dev/tty, /dev/null, etc.)
│   │   ├── ext2.rs          # Read/write ext2 implementation
│   │   ├── pipe.rs          # Pipe implementation
│   │   └── tty.rs           # TTY line discipline (canonical mode)
│   ├── syscall/             # Syscall gate (17 calls)
│   ├── blk/                 # Block device layer (virtio-blk)
│   ├── boot/                # Boot protocol translation (Limine)
│   └── shell/               # Shell syntax parser (no_std, zero-allocation)
├── user/
│   ├── lib/sys.rs           # Slim libc (syscall wrappers)
│   └── bin/                 # User programs
│       ├── init.rs          # PID 1: restarts shell, reaps orphans
│       ├── sh.rs            # Shell with pipes and redirection
│       ├── hello.rs         # Ring 3 proof of concept
│       ├── ls.rs, cat.rs, echo.rs, sleep.rs, true.rs, false.rs
├── scripts/
│   ├── mkiso.sh             # Build kernel, user programs, ISO, and disk image
│   ├── run.sh               # Launch in QEMU
│   └── test.sh              # Full CI pipeline
├── arch/x86_64/linker.ld    # Position-independent linker script
└── limine.conf              # Bootloader configuration
```

### Design Principles

- **Library/Binary Split**: `src/lib.rs` targets `x86_64-unknown-linux-gnu` (host testing); machine code is gated behind `target_os = "none"`.
- **Shared Syscall Table**: User programs include `src/syscall/nr.rs` via `#[path]`—kernel and userspace cannot disagree on numbering.
- **Zero-Allocation Shell Parser**: The shell syntax parser uses no heap allocation, runs on the host, and is fully unit-tested.
- **Buddy Allocator with Intrusive Free List**: Metadata is carved from the managed memory; O(1) coalescing via doubly-linked lists embedded in free blocks.

---

## 🎯 Milestones

Each milestone proves the current subsystem correct before the next is built.

| Milestone | Subsystem | What Is Verified |
|:---------:|-----------|------------------|
| **M0** | Allocator | Global heap allocator works end-to-end (Vec, String) |
| **M1** | Interrupts | Timer ticks at the correct frequency; breakpoint exception is catchable |
| **M2** | Frames & Heap | Buddy allocator hands out non-overlapping blocks; slab heap satisfies all layouts; both agree on memory usage |
| **M3** | Paging | Kernel runs on its own page tables; whole image W^X verified; processes get independent address spaces sharing the kernel half |
| **M4** | Scheduler | Round-robin, preemption, sleep, idle, priority-based core stealing all work |
| **M6** | Syscalls | `int 0x80` gate dispatches correctly; fd checks work; exit through the gate ends the task |
| **M7** | VFS | ramfs root, mounted sub-filesystems, multi-task file I/O through the fd table |
| **M8** | Initramfs & devfs | tar unpacking, `/dev/tty`, `/dev/console`, `/dev/null`, `/dev/zero` |
| **M9** | Processes | ELF loaded into a new address space; enter Ring 3; exit code round-trip verified |
| **M10** | Block Device & ext2 | virtio-blk detection; ext2 mount, read host-seeded files, write via /mnt, cold-mount verification |
| **M11** | Pipes, TTY, Signals | Pipe byte transfer and EOF; TTY line editing, echo, Ctrl-D; SIGTERM, SIGKILL, kill(0) probe |
| **M12** | Shell & Core Utils | Shell, echo, cat, ls, true, false, sleep—all running in Ring 3 |
| **M13** | Init Handoff | `/bin/init` becomes PID 1; shell becomes interactive via UART interrupts |
| **M14** | Exit Status | Shell `$?` carries the exit code of the previous command |

---

## 🚀 Quick Start

### Prerequisites

- **Rust nightly** (with `rust-src`, `rustfmt`, `clippy`)
- **QEMU** (x86_64 or aarch64)
- **mkisofs** / **xorriso** (for creating the ISO)
- **mke2fs** / **e2fsprogs** (for creating the ext2 disk image)

```bash
# Install Rust nightly and required components
rustup toolchain install nightly
rustup component add rust-src rustfmt clippy --toolchain nightly

# Add bare-metal targets
rustup target add x86_64-unknown-none aarch64-unknown-none --toolchain nightly
```

### Quick Start

```bash
# 1. Fetch the Limine bootloader (once only)
./scripts/fetch-limine.sh

# 2. Build the kernel, user programs, and bootable ISO
./scripts/mkiso.sh

# 3. Launch in QEMU
./scripts/run.sh
```

The kernel will boot, run the M0–M13 milestone verification, and then drop into an interactive shell.

---

## 🔨 Building

### Kernel Only

```bash
cargo build --target x86_64-unknown-none
```

### Full Image (ISO + Disk)

```bash
./scripts/mkiso.sh
```

This script compiles the kernel, compiles each user program with `rustc` (standalone, static, `ET_EXEC`), assembles the initramfs tar, creates a bootable ISO supporting dual BIOS+UEFI El Torito boot, and formats an 8 MiB ext2 disk image.

### Cross-Architecture

```bash
ARCH=aarch64 ./scripts/run.sh
```

aarch64 support is under development—the kernel ELF currently builds, but the boot flow is not yet complete.

---

## 🧪 Testing

### Host-Side Unit Tests

The library crate targets the host, enabling fast iteration:

```bash
cargo test --lib --target x86_64-unknown-linux-gnu
```

This tests the scheduler policy, shell parser, VFS path resolution, and other architecture-independent code.

### Full CI Pipeline

```bash
./scripts/test.sh
```

Runs the full pipeline:

1. `cargo fmt --check`
2. `cargo clippy` (kernel and host targets)
3. `cargo test --lib` (host)
4. Build via `mkiso.sh`
5. Launch in QEMU and capture the serial log
6. Assert milestone markers (`M0_OK` through `M13_OK`)
7. Check for `KERNEL PANIC` or `INIT_EXITED`

---

## 📡 Syscalls

MPOS exposes 17 syscalls through the `int 0x80` gate:

| # | Call | Description |
|:-:|------|-------------|
| 0 | `EXIT` | Terminate the process |
| 1 | `READ` | Read from a file descriptor |
| 2 | `WRITE` | Write to a file descriptor |
| 3 | `YIELD` | Yield the CPU to the scheduler |
| 4 | `GETPID` | Get the current process ID |
| 5 | `SLEEP` | Sleep for N milliseconds |
| 6 | `PIPE` | Create a pipe |
| 7 | `KILL` | Send a signal to a process |
| 8 | `OPEN` | Open a file |
| 9 | `CLOSE` | Close a file descriptor |
| 10 | `SPAWN` | Load and execute a program |
| 11 | `WAIT` | Wait for a child process |
| 12 | `LISTDIR` | List directory entries |
| 13 | `DUP2` | Duplicate a file descriptor |
| 14 | `CREATE` | Create a file or directory |
| 15 | `UNLINK` | Remove a file system entry |
| 16 | `CHDIR` | Change the working directory |

---

## 🧰 User Programs

| Program | Description |
|---------|-------------|
| `init` | PID 1 — restarts `/bin/sh` with exponential backoff, reaps orphaned processes |
| `sh` | Shell with pipes, redirection, variables, background jobs, and scripting |
| `hello` | Ring 3 proof of concept (prints, yields, sleeps, exits with code 42) |
| `ls` | Directory listing |
| `cat` | File concatenation |
| `echo` | Argument echo |
| `sleep` | Sleep for N seconds |
| `true` / `false` | Exit 0 / exit 1 |

All user programs are `#![no_std]`, compiled standalone with `rustc`, sharing the slim libc in `user/lib/sys.rs`.

---

## 📜 License

This project is licensed under the [MIT License](LICENSE).

---

<div align="center">
<sub>Built with 🦀 Rust—proven correct, one milestone at a time.</sub>
</div>
