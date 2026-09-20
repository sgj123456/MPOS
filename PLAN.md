# MPOS Feature Roadmap

Missing essential features, prioritized by impact on real-world workload compatibility.

---

## Current Status

- **~225 Linux x86_64 syscalls** implemented
- **Uniprocessor** — single core, `critical_section::Mutex<RefCell<T>>`
- **Filesystems**: ramfs, ext2 (write-through), devfs, procfs, sysfs, devpts
- **Networking**: smoltcp TCP/UDP/ICMP/ARP over virtio-net
- **Scheduler**: 4-level priority + round-robin, COW fork, demand-paged stack

---

## Phase 1: Syscall Completion — **DONE**

The POSIX completion set landed: epoll, timerfd, the `*at` relatives and
modern file forms, the identity/capability family, scheduling, memory
advice, and the machine-level calls — every one implemented per its man
page, with flag values, struct layouts and errnos pinned by host-side
tests. What remains of the phase is listed at the end.

### 1.1 epoll (Critical) — implemented in `src/vfs/epoll.rs`

| Syscall | Nr | Status |
|---------|-----|--------|
| `epoll_create` | 213 | Done (size ignored, must be positive) |
| `epoll_create1` | 291 | Done (EPOLL_CLOEXEC) |
| `epoll_ctl` | 233 | Done (ADD/MOD/DEL, EXCLUSIVE rules, ELOOP nesting ≤ 5) |
| `epoll_wait` | 232 | Done (ms timeout; -1 forever, 0 immediate) |
| `epoll_pwait` | 281 | Done (sigmask validated) |
| `epoll_pwait2` | 441 | Done (timespec timeout, sigsetsize == 8) |

Beyond the plan: level and edge trigger (`EPOLLET` with dip tracking),
one-shot (`EPOLLONESHOT` with MOD re-arm), `EPOLLEXCLUSIVE` vocabulary,
`ELOOP` cycle detection for epoll-on-epoll, `EPERM` for regular files,
close-time entry reaping, and readiness from the same `poll_ready` the
poll(2) family consults — so a wait hears about data the instant it
arrives.

### 1.2 timerfd (High) — implemented in `src/vfs/timerfd.rs`

| Syscall | Nr | Status |
|---------|-----|--------|
| `timerfd_create` | 283 | Done (5 clockids, TFD_NONBLOCK/CLOEXEC) |
| `timerfd_settime` | 286 | Done (ABSTIME, CANCEL_ON_SET, old_value) |
| `timerfd_gettime` | 287 | Done (remaining + interval as itimerspec) |

Beyond the plan: an O(1) interval expiry (division, not a loop), a
saturating expiration counter, and the `TFD_TIMER_CANCEL_ON_SET`
registry the `clock_settime` handler walks — the read answers `ECANCELED`
until re-arm.

### 1.3 Other Missing Syscalls — done, 67 of 68

| Syscall | Nr | Status | Notes |
|---------|-----|--------|-------|
| `copy_file_range` | 326 | Done | EXDEV across mounts, in-out offsets, overlap EINVAL |
| `preadv2` / `pwritev2` | 327/328 | Done | RWF_APPEND honoured, -1 = fd cursor, unknown flag EOPNOTSUPP |
| `fallocate` | 285 | Done | mode 0 extends for real; KEEP_SIZE accepted; interior ops EOPNOTSUPP |
| `openat2` | 437 | Done | open_how by size, mode-without-O_CREAT EINVAL, RESOLVE_* EOPNOTSUPP |
| `faccessat2` | 439 | Done | X_OK from stored mode bits, NOFOLLOW honoured |
| `creat`, `utime`, `fchmodat`, `mknodat`, `futimesat`, `readahead`, `splice`, `syncfs`, `syslog` | — | Done | `src/syscall/linux/fs.rs` |
| identity (setreuid…setfsgid, capget/capset), scheduling (sched_setparam…sched_getattr, getpriority/setpriority), memory (msync, mincore, mlock family), machine (personality, vhangup, getcpu, settimeofday, clock_settime, unshare, setns, seccomp, setdomainname), signals (tgkill, rt_sigqueueinfo, rt_sigsuspend, pause) | — | Done | `src/syscall/linux/posix.rs` |
| `accept4`, `sendmmsg`, `recvmmsg` | 288/307/299 | Done | `src/net/socket.rs` |
| `rt_sigpending` | 127 | **Deferred** | needs a pending-mask accessor in `src/sched/runtime.rs` |
| `io_uring_setup/enter/register` | 425-427 | Deferred | per the original phase's own call |


---

## Phase 2: Signal Delivery (High Priority)

Current model: signals are recorded but never delivered as handlers — every signal kills the process. This breaks programs that install signal handlers (e.g., bash, any program with SIGALRM).

### 2.1 Signal Frame Delivery

**Goal**: When a process has installed a handler via `rt_sigaction`, deliver the signal by jumping to the handler instead of killing.

**Implementation plan**:

1. Build `rt_sigframe` on the user stack matching Linux x86_64 layout:
   ```
   struct rt_sigframe {
       rt_sigreturn: u64,     // trampoline address
       rax, rdx, rcx, rsi,   // saved registers
       rdi, r8..r15,
       rbp, rsp, rip,
       eflags, cs, ss,
       fpstate: [512 bytes],  // FXSAVE area (zeroed, soft-float)
       info: siginfo_t,       // signal info (128 bytes)
       context: ucontext_t,   // saved context with mask
   }
   ```

2. Modify `UserContext` before returning to user:
   - `RIP` -> handler address
   - `RDI` -> signal number
   - `RSP` -> pointer to rt_sigframe (minus 8 for alignment)
   - `RFLAGS` -> clear IF bit (disable interrupts in handler)
   - Push `rt_sigreturn` trampoline address so handler returns cleanly

3. `rt_sigreturn` (nr 15): restore full register file from the signal frame, re-enable interrupts.

**Key files**:
- `src/sched/signal.rs` — add `SignalFrame` struct, frame construction
- `src/sched/runtime.rs` — modify preemption/signal delivery path
- `src/syscall/linux/mod.rs` — verify `rt_sigreturn` dispatches correctly

### 2.2 Signal Mask Enforcement

Currently `rt_sigprocmask` records the mask but never checks it.

**Changes**:
- In the signal delivery path, check `task.blocked & (1 << signum)` before delivering
- Blocked signals stay in the pending set
- `rt_sigreturn` restores the original mask from the signal frame

### 2.3 Signal Queue

Currently `Pending` is a `u32` bitmask — same signal can only be pending once.

**Changes**:
- Replace `u32` pending with `HashMap<u8, VecDeque<siginfo_t>>` per task
- Support `SI_QUEUE`, `SI_TIMER`, `SI_USER` si_code values
- `kill(pid, sig)` sends `SI_USER`; `sigqueue(pid, sig, val)` sends `SI_QUEUE`

---

## Phase 3: Filesystem Enhancements (Medium Priority)

### 3.1 Hard Links

Currently `linkat` returns `EPERM`.

**Changes**:
- Add `link_count()` and `add_link()` / `rm_link()` to `Inode` trait
- ext2: implement `i_links_count` in on-disk inode, decrement on unlink, free when zero
- ramfs: share `Arc<dyn Inode>` across directory entries
- Test: `ln file1 file2 && echo test >> file2 && cat file1` shows same content

### 3.2 File Locking (fcntl F_SETLK)

Currently `F_GETLK`/`F_SETLK`/`F_SETLKW` are permissive stubs.

**Changes**:
- Add `locks: Vec<FileLock>` to `File` or per-inode lock table
- `F_SETLK`: record lock (F_RDLCK or F_WRLCK) for (pid, fd, offset, len)
- `F_GETLK`: scan for conflicts, return `F_UNLCK` if none
- `F_SETLKW`: block until lock available
- `read`/`write`: check for conflicting locks (F_WRLCK blocks reads, F_RDLCK blocks writes)
- `close`: release all locks for the closing fd

### 3.3 File Timestamps

Currently `utimensat` is accepted but timestamps are not stored.

**Changes**:
- Add `atime`, `mtime`, `ctime` fields to `Inode` trait (or a `Metadata` struct)
- ext2: store in `i_atime`, `i_mtime`, `i_ctime` on disk
- ramfs: store in `RamInode` behind mutex
- `stat` returns real timestamps
- `open`/`write`/`read`/`mkdir`/`unlink` update mtime/ctime

### 3.4 Permission and Ownership Model

Currently `chmod`/`chown` are accepted but values are not stored.

**Changes**:
- Add `uid`, `gid`, `mode` to `Inode` trait
- ext2: persist in on-disk inode (`i_uid`, `i_gid`, `i_mode`)
- ramfs: store in-memory
- `open()`: check permissions against current process uid/gid (simplified: root bypasses all)
- `stat` returns stored values
- Support `S_IRUSR`/`S_IWUSR`/`S_IXUSR`/`S_IRGRP`/`S_IWGRP`/`S_IXGRP`/`S_IROTH`/`S_IWOTH`/`S_IXOTH`

### 3.5 /proc Enhancements

Current procfs has: `meminfo`, `mounts`, `self`, `cmdline`, `status`, `maps`, `fd`.

**Missing entries**:

| File | Description |
|------|-------------|
| `/proc/<pid>/fd/<n>` | Symlink to opened file |
| `/proc/<pid>/limits` | Resource limits |
| `/proc/<pid>/stat` | One-line process status |
| `/proc/<pid>/statm` | Memory usage in pages |
| `/proc/cpuinfo` | CPU info ("mpos" / QEMU CPU) |
| `/proc/version` | Kernel version string |
| `/proc/uptime` | Uptime in seconds |
| `/proc/loadavg` | Load average (0,0,0 on single core) |
| `/proc/filesystems` | Supported filesystem types |
| `/proc/diskstats` | Block device stats |
| `/proc/net/tcp` | TCP connections |
| `/proc/net/udp` | UDP sockets |

---

## Phase 4: Networking Enhancements (Medium Priority)

### 4.1 AF_UNIX Domain Sockets

Currently only AF_INET. AF_UNIX is essential for local IPC (dbus, X11, Docker socket, snap/flatpak).

**Implementation plan**:
- New file: `src/net/unix.rs`
- `UnixSocket`: backed by a `sockaddr_un` path in the filesystem
- `socketpair()` already has pipe-like semantics — reuse `PipeState` or create `UnixSocketState`
- `bind()`: create a filesystem node at the socket path
- `connect()`: look up path, establish bidirectional pipe-like channel
- `accept()`: accept incoming connection from the path
- `sendmsg`/`recvmsg` with `SCM_RIGHTS` for fd passing (stretch goal)

### 4.2 ICMP Improvements

Current: auto echo reply only. No user-facing ICMP socket.

**Changes**:
- `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` creates raw ICMP socket
- `sendto`/`recvfrom` on raw ICMP socket
- Enable `ping` command to work

### 4.3 TCP Socket Options

| Option | Current | Needed |
|--------|---------|--------|
| `SO_REUSEADDR` | Not supported | Implement — avoids `EADDRINUSE` on server restart |
| `SO_KEEPALIVE` | Not supported | Implement — TCP keepalive probes |
| `TCP_NODELAY` | Not supported | Implement — disable Nagle algorithm |
| `SO_RCVBUF`/`SO_SNDBUF` | Not supported | Return reasonable defaults |
| `SO_ERROR` | Not supported | Get/clear socket error |
| `SO_LINGER` | Not supported | Accept and honor or stub gracefully |

### 4.4 Netlink Write Operations

Currently read-only. Need write support for network configuration.

**Changes**:
- `RTM_NEWADDR`: configure IP address (for `ip addr add`)
- `RTM_DELADDR`: remove IP address
- `RTM_NEWROUTE`: add route
- `RTM_DELROUTE`: remove route
- `RTM_NEWLINK`: bring interface up/down
- `RTM_SETLINK`: change MTU, name

---

## Phase 5: Memory Management Enhancements (Medium Priority)

### 5.1 Shared Anonymous mmap

Currently `MAP_SHARED | MAP_ANONYMOUS` returns `ENOSYS`.

**Implementation plan**:
- Create a global anonymous `Inode` type shared between processes
- `mmap(MAP_SHARED | MAP_ANONYMOUS)`: allocate shared inode, map pages
- `fork()`: increment frame refcounts (already COW-aware)
- `munmap()`: decrement refcount, free only when zero

### 5.2 madvise Real Implementation

Currently returns 0 (no-op). Implement at least:

| Advice | Behavior |
|--------|----------|
| `MADV_DONTNEED` | Immediately free pages; next access re-faults |
| `MADV_SEQUENTIAL` | Hint: sequential access pattern, enable read-ahead |
| `MADV_RANDOM` | Hint: random access, disable read-ahead |
| `MADV_WILLNEED` | Hint: will need soon, trigger read-ahead |
| `MADV_FREE` | Lazy-free: mark pages reclaimable but don't free yet |

### 5.3 Process_vm_readv / Process_vm_writev

| Syscall | Nr | Description |
|---------|-----|-------------|
| `process_vm_readv` | 310 | Read from another process's memory |
| `process_vm_writev` | 311 | Write to another process's memory |

Needed by debuggers and checkpoint/restore tools.

---

## Phase 6: Process Management Enhancements (Low-Medium Priority)

### 6.1 ptrace (Debugger Support)

| Syscall | Nr | Description |
|---------|-----|-------------|
| `ptrace` | 101 | Process trace |

**Required for**: gdb, strace, ltrace

**Implementation plan**:
- `PTRACE_TRACEME`: child sets `traced = true`, sends `SIGSTOP` to self
- `PTRACE_ATTACH`/`PTRACE_SEIZE`: attach to target process
- `PTRACE_PEEKDATA`/`PTRACE_POKEDATA`: read/write target memory
- `PTRACE_GETREGS`/`PTRACE_SETREGS`: read/write target registers
- `PTRACE_SINGLESTEP`: set TF (trap flag) in RFLAGS, deliver `SIGTRAP` on next instruction
- `PTRACE_CONT`: resume execution, optionally delivering a signal
- On any stop, target enters `TASK_TRACED` state and parent is notified via `waitid`

### 6.2 Process Resource Limits (Real)

Currently `prlimit64` returns hardcoded values, `setrlimit` is ignored.

**Changes**:
- Add `rlimits: Rlimits` to `Task` struct (64-bit values for each `RLIMIT_*`)
- `prlimit64(pid, res, new_lim, old_lim)`: read/write per-process limits
- `fork()`: inherit limits from parent
- `execve()`: keep limits (no reset)
- Enforce `RLIMIT_NOFILE` on fd allocation
- Enforce `RLIMIT_STACK` on stack growth

### 6.3 Namespace Support (Minimal)

| Syscall | Nr | Description |
|---------|-----|-------------|
| `setns` | 308 | Enter namespace |
| `unshare` | 272 | Unshare namespace |

**Minimal scope**: `CLONE_NEWNS` (mount namespace only) for basic container isolation.

### 6.4 Additional Process Syscalls

| Syscall | Nr | Description |
|---------|-----|-------------|
| `getpgrp` | 111 | Implemented, verify correctness |
| `setreuid` | 113 | Set real/effective UID |
| `setregid` | 114 | Set real/effective GID |
| `getresuid` | 118 | Get real/effective/saved UID |
| `getresgid` | 119 | Get real/effective/saved GID |
| `capget` | 125 | Get capability state |
| `capset` | 126 | Set capability state |

---

## Phase 7: Device Drivers (Low Priority)

### 7.1 virtio-gpu

**Why**: Graphical output, desktop environment.

- VirtIO GPU driver (2D mode first, 3D stretch)
- Framebuffer mapped to userspace via mmap
- `ioctl` DRM interface (`DRM_IOCTL_MODE_GETRESOLUTION`, `DRM_IOCTL_MODE_SETCRTC`)
- `/dev/dri/card0` via devfs

### 7.2 virtio-input

**Why**: Keyboard/mouse without serial.

- VirtIO input driver (evdev protocol)
- Event queue: `struct input_event { time, type, code, value }`
- `/dev/input/event0` via devfs
- Keyboard scancode -> keycode mapping

### 7.3 AHCI/SATA (stretch)

**Why**: Real hardware support beyond virtio.

- AHCI HBA driver
- ATA IDENTIFY, READ SECTORS, WRITE SECTORS
- GPT partition detection (already have partition scanner)

---

## Phase 8: Security Hardening (Low Priority)

### 8.1 ASLR (Address Space Layout Randomization)

**Changes**:
- Randomize mmap base address per process (currently fixed)
- Randomize stack base address
- PIE support already exists (`ET_DYN` ELF) — just randomize load address
- Use `rand::next_u64()` for randomization seed

### 8.2 seccomp

| Syscall | Nr | Description |
|---------|-----|-------------|
| `seccomp` | 317 | Secure computing mode |

- `SECCOMP_MODE_STRICT`: block execve, socket, mount
- `SECCOMP_MODE_FILTER`: BPF program filter (future)

### 8.3 User Stack Canary

**Changes**:
- Inject random canary at process start
- `__stack_chk_fail` trampoline that sends `SIGABRT`
- Compiler flag: `-fstack-protector-strong` for user programs

---

## Phase 9: Architecture Expansion (Far Future)

### 9.1 aarch64 Complete Boot

Current status: kernel ELF compiles but boot flow incomplete.

**Needed**:
- AArch64 exception vector table (`VBAR_EL1`)
- GICv2/v3 interrupt controller driver
- ARM virtual memory (4KB pages, 3/4-level tables via `TTBR0_EL1`/`TTBR1_EL1`)
- PSCI for power management (reboot, shutdown)
- Timer via `CNTP_EL0` (ARM generic timer)
- UART via PL011 or SBSA

### 9.2 RISC-V Support

- SBI interface for boot/runtime services
- PLIC interrupt controller
- Sv39/Sv48 page tables
- `mtime`/`mtimecmp` timer
- UART via SIFIVE_UART or NS16550

---

## Priority Summary

| Phase | Effort | Impact | Blocks |
|-------|--------|--------|--------|
| **P1: epoll + timerfd** | ~2 weeks | Critical | Most network services |
| **P2: Signal delivery** | ~2 weeks | Critical | Shell, bash, most programs |
| **P3: FS enhancements** | ~2 weeks | High | Hard links, timestamps, locking |
| **P4: Networking** | ~2 weeks | High | AF_UNIX, ping, TCP options |
| **P5: Memory** | ~1 week | Medium | Shared memory, madvise |
| **P6: Process** | ~2 weeks | Medium | Debugging, rlimits |
| **P7: Drivers** | ~2 weeks | Low | GUI, input |
| **P8: Security** | ~1 week | Low | ASLR, seccomp |
| **P9: Architectures** | ~4 weeks | Low | aarch64, RISC-V |

**Recommended order**: P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7 -> P8 -> P9

The first two phases (epoll/timerfd + signal delivery) are the highest impact — they unlock the majority of real-world Linux userspace programs.
