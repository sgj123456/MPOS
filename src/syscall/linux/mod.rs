//! Linux x86_64 syscall number mapping and POSIX handlers.
//!
//! When a program compiled for Linux (`x86_64-unknown-linux-musl`) runs on
//! mpos, it enters through the `syscall` instruction with Linux numbers.
//! This module maps those numbers onto handlers in [`super::proc`]
//! (Linux-compatible memory/process/time calls) and [`fs`] (path-carrying
//! calls in Linux argument shapes).

pub mod nr;

/// The pure ABI half: flag values, struct layouts, argument validation and
/// encodings - host-compiled so the man-page contract is unit-tested, and
/// the only place those facts are spelled.
pub mod abi;

/// The path-carrying calls in Linux argument shapes (`fs::file`), and the
/// device-`ioctl` entry point (`fs::ioctl`) — split along that seam at
/// PLAN.md §11.2 Step E; `fs::mod` re-exports the handlers so dispatch below
/// still names `fs::<handler>` as when it was one file.
#[cfg(target_os = "none")]
pub mod fs;

/// Linux-compatible memory and process handlers (mmap/munmap/mremap/brk,
/// execve/exit/wait, getpid/kill, nanosleep, ...). Kept here at the same
/// level as `fs` so the dispatch can reach both without the deleted custom
/// modules they used to share a parent with.
#[cfg(target_os = "none")]
pub(crate) mod proc;

/// The POSIX completion set's process half: identity, capabilities,
/// scheduling, memory advice, the wall clock's setters, and the signals a
/// process aims at itself. Kept beside `fs` and `proc` rather than inside
/// `proc` because it is the surface *completed* rather than the machinery
/// - the calls are table answers and validations over the scheduler's
/// exported state, and the module's own opening note says what that
/// means for each family.
#[cfg(target_os = "none")]
pub(crate) mod posix;

/// Run a pending signal's handler on the way out of a syscall.
///
/// The register frame is what makes this possible at all - entering a handler
/// means rewriting the task's return context wholesale - so it is handed
/// straight through to the layer that knows the frame's shape. `nr` is the
/// call just completed, which is what `SA_RESTART` needs to decide whether the
/// interrupted call is one Linux runs again (`abi::restartable`).
///
/// Gated on the machine declaring a signal frame (`mpos_signal_frame`): the
/// frame's layout, and the `rt_sigreturn` that unwraps it, are the machine's
/// ABI. A port without one is not delivered a handler at all - the call site in
/// `linux_dispatch` is gated to match - rather than handed a frame the machine
/// would have to guess at (`PLAN.md` §13.2).
#[cfg(mpos_signal_frame)]
pub fn deliver_signal(regs_ptr: *mut (), nr: u64, ret: isize) -> bool {
    proc::deliver_signal(regs_ptr, nr, ret)
}

/// One Linux syscall, dispatched by number across the whole table.
///
/// # Safety
/// `regs_ptr` must be null or the syscall gate's own frame for the calling
/// task. Handlers reach it through `hal::user_ip`/`hal::set_exec_return` and
/// through the machine's own frame code, and it is dereferenced there without
/// further checks.
#[cfg(target_os = "none")]
pub unsafe fn dispatch(nr: u64, args: [usize; 6], from_user: bool, regs_ptr: *mut ()) -> isize {
    // SECCOMP_MODE_STRICT is on for this thread. Only the four calls a
    // condemned process still needs survive: read, write, exit and the
    // signal-return trampoline. Anything else is fatal - the filtered call
    // never reaches the handlers below and this thread gets SIGKILL, matching
    // the `do_exit(SIGKILL)` Linux runs on a strict-mode violation.
    if crate::sched::current_is_seccomp_strict()
        && !matches!(nr, nr::READ | nr::WRITE | nr::EXIT | nr::RT_SIGRETURN)
    {
        crate::sched::send_signal(crate::sched::current(), crate::sched::signal::SIGKILL);
        return err::ENOSYS;
    }
    // And a `SECCOMP_MODE_FILTER` program decides *before* the table does, for
    // the same reason strict mode does: the filtered call must not run at all.
    // The whole check sits behind one relaxed load of a machine-wide flag, so a
    // machine that never installs a filter - every boot in this image - pays
    // nothing for the feature.
    //
    // Every arm below was measured against this developer's Linux rather than
    // read off the man page, because three of them are not what the man page
    // sounds like; §8.2 holds the numbers and the two drafts that were wrong.
    //
    // Deferred off aarch64 with the rest of the `seccomp(2)` handler: no filter
    // can be installed there until that syscall is ported. STRICT mode above
    // still runs on both machines; only BPF filtering waits.
    #[cfg(not(target_arch = "aarch64"))]
    if crate::seccomp::any_installed() {
        let mut data_args = [0u64; 6];
        for (slot, arg) in data_args.iter_mut().zip(args) {
            *slot = arg as u64;
        }
        // The instruction pointer comes from the gate through `hal`, which names
        // no machine. The filter carries it in `seccomp_data`, so a filter that
        // checks which of its own call sites is running reads the value the
        // kernel recorded rather than a guess.
        // SAFETY: `regs_ptr` is the gate's own frame for this task.
        let ip = unsafe { crate::hal::user_ip(regs_ptr) };
        let data = crate::seccomp::data_bytes(nr as i32, ip, data_args);
        if let Some(answer) = crate::sched::run_seccomp_filters(&data) {
            use crate::sched::signal::SIGSYS;
            use crate::seccomp as sc;
            match sc::action(answer) {
                // Allow, and the log action that also allows: this kernel has
                // one loglevel and no per-filter audit sink, so a filter asking
                // for its decision to be recorded gets the decision. Measured:
                // `LOG` lets the call through exactly as `ALLOW` does.
                sc::RET_ALLOW | sc::RET_LOG => {}
                // `ERRNO`: the call does not happen, and the errno is the data
                // half verbatim - including the case that surprises, a data
                // half of 0, which answers a *successful* 0 rather than an
                // error. Measured: `-data` for both.
                sc::RET_ERRNO => return -(sc::action_data(answer) as isize),
                // `TRAP`: `SIGSYS`, raised instead of the call. With no handler
                // - the default action, and every case a real program has - the
                // process dies of it. With one, Linux's handler returns into
                // the caller's own registers, so the caller reads back whatever
                // it had in `rax`, which on x86_64 is the number it just asked
                // for: `getppid` measured as answering 110. The return below is
                // that observation rather than a choice, and ENOSYS here would
                // have been the plausible-looking wrong answer.
                sc::RET_TRAP => {
                    crate::sched::send_signal(crate::sched::current(), SIGSYS);
                    return nr as isize;
                }
                // `TRACE`: a tracer that never asked for
                // `PTRACE_O_TRACESECCOMP` lets the call through, and that
                // option is refused by `PTRACE_SETOPTIONS` here, so every
                // traced caller on this machine is that kind of tracer. Nobody
                // tracing: the call fails with `ENOSYS`, which is the measured
                // answer and the one that is *not* what "trace" sounds like.
                sc::RET_TRACE => {
                    if !crate::sched::has_tracer(crate::sched::current()) {
                        return err::ENOSYS;
                    }
                }
                // `USER_NOTIF` hands the call to a userspace supervisor, and
                // this kernel has none to hand it to. Measured: Linux with no
                // listener fails the call with `ENOSYS`, so refusing it is not
                // a fallback but the answer for the machine it is running on.
                sc::RET_USER_NOTIF => return err::ENOSYS,
                // The kills, and any value not recognised above: all of them
                // end the task with **`SIGSYS`**, which is what Linux's
                // `RET_KILL_THREAD` and `RET_KILL_PROCESS` actually deliver -
                // `SIGKILL` was this kernel's answer before it was measured,
                // and would have been the wrong signal to hang the change on.
                // An unrecognised action falling here is deliberate too: the
                // man page says Linux treats those as `KILL_PROCESS`, and a
                // filter returning an invented action was seen to kill the
                // process with SIGSYS. Allowing instead would have been the
                // one direction a sandbox cannot survive being wrong in.
                _ => {
                    crate::sched::send_signal(crate::sched::current(), SIGSYS);
                    return err::ENOSYS;
                }
            }
        }
    }
    let ret = match nr {
        // --- file I/O: Linux argument shapes ---
        nr::READ => fs::handle_read(args, from_user),
        nr::WRITE => fs::handle_write(args, from_user),
        nr::OPEN => fs::open(args, &cwd()),
        nr::OPENAT => fs::openat(args, &cwd()),
        nr::CLOSE => fs::handle_close(args),
        nr::FSTAT => fs::fstat(args),
        // The legacy stat pair. musl reaches for these before the `*at` forms,
        // and busybox asks about a command's file before it will exec it - so
        // without them every external command answers ENOSYS and nothing in
        // /bin can be run.
        nr::STAT | nr::LSTAT => fs::stat_legacy(nr, args, &cwd()),
        nr::NEWFSTATAT => fs::newfstatat(args, &cwd()),
        nr::STATX => fs::statx(args, &cwd()),
        nr::LSEEK => fs::handle_lseek(args),
        nr::GETDENTS64 => fs::handle_getdents64(args, from_user),
        nr::DUP => fs::handle_dup(args),
        nr::DUP2 => fs::handle_dup2(args),
        nr::DUP3 => fs::handle_dup3(args),
        nr::PIPE => fs::handle_pipe([args[0], 0, 0, 0, 0, 0], from_user),
        nr::PIPE2 => fs::handle_pipe(args, from_user),
        nr::UNLINKAT => fs::unlinkat(args, &cwd()),
        // The legacy path forms, man(2) conventions musl/glibc call: each is
        // its *at equivalent at the caller's cwd.
        nr::UNLINK => fs::unlinkat([abi::AT_FDCWD as usize, args[0], 0, 0, 0, 0], &cwd()),
        nr::MKDIR => fs::mkdirat([abi::AT_FDCWD as usize, args[0], args[1], 0, 0, 0], &cwd()),
        nr::RMDIR => fs::rmdir(args, &cwd()),
        nr::RENAME => fs::renameat2(
            [
                abi::AT_FDCWD as usize,
                args[0],
                abi::AT_FDCWD as usize,
                args[1],
                0,
                0,
            ],
            &cwd(),
        ),
        // The *at spelling carries its own dirfds: (olddirfd, oldpath,
        // newdirfd, newpath). apk renames its downloaded index into the
        // cache with this form, dirfds and all.
        nr::RENAMEAT => fs::renameat2([args[0], args[1], args[2], args[3], 0, 0], &cwd()),
        nr::MKDIRAT => fs::mkdirat(args, &cwd()),
        nr::RENAMEAT2 => fs::renameat2(args, &cwd()),
        nr::GETCWD => fs::handle_getcwd(args, from_user),
        nr::CHDIR => fs::chdir(args),
        nr::FCHDIR => fs::handle_fchdir(args),
        nr::FCNTL => fs::fcntl(args),
        nr::FLOCK => fs::flock(args),
        nr::TRUNCATE => fs::truncate(args, &cwd()),
        nr::FTRUNCATE => fs::ftruncate(args),
        nr::STATFS => fs::statfs(args, &cwd()),
        nr::FSTATFS => fs::fstatfs(args),
        nr::IOCTL => fs::handle_ioctl(args, from_user),
        nr::WRITEV => fs::handle_writev(args, from_user),
        nr::READV => fs::handle_readv(args, from_user),
        nr::ACCESS => fs::access(args, &cwd()),
        nr::FACCESSAT => fs::faccessat(args, &cwd()),
        nr::READLINK => fs::readlink(args, &cwd()),
        nr::READLINKAT => fs::readlinkat(args, &cwd()),
        // `symlinkat` moves the linkpath to args[2] and puts a dirfd in
        // args[1] - the plain `symlink` layout would read the dirfd number as
        // the link's name, which is how apk's library links all ended up as
        // one junk name in `/` with EEXIST for every one after the first.
        nr::SYMLINK => fs::symlink(args, &cwd()),
        nr::SYMLINKAT => fs::symlinkat(args, &cwd()),

        // --- sockets: AF_INET over the smoltcp stack ---
        #[cfg(mpos_net)]
        nr::SOCKET => crate::net::socket::handle_socket(args, from_user),
        #[cfg(mpos_net)]
        nr::CONNECT => crate::net::socket::handle_connect(args, from_user),
        #[cfg(mpos_net)]
        nr::ACCEPT => crate::net::socket::handle_accept(args, from_user),
        #[cfg(mpos_net)]
        nr::SENDTO => crate::net::socket::handle_sendto(args, from_user),
        #[cfg(mpos_net)]
        nr::RECVFROM => crate::net::socket::handle_recvfrom(args, from_user),
        #[cfg(mpos_net)]
        nr::SENDMSG => crate::net::socket::handle_sendmsg(args, from_user),
        #[cfg(mpos_net)]
        nr::RECVMSG => crate::net::socket::handle_recvmsg(args, from_user),
        #[cfg(mpos_net)]
        nr::SHUTDOWN => crate::net::socket::handle_shutdown(args, from_user),
        #[cfg(mpos_net)]
        nr::BIND => crate::net::socket::handle_bind(args, from_user),
        #[cfg(mpos_net)]
        nr::LISTEN => crate::net::socket::handle_listen(args, from_user),
        #[cfg(mpos_net)]
        nr::GETSOCKNAME => crate::net::socket::handle_getsockname(args, from_user),
        #[cfg(mpos_net)]
        nr::GETPEERNAME => crate::net::socket::handle_getpeername(args, from_user),
        #[cfg(mpos_net)]
        nr::SOCKETPAIR => crate::net::socket::handle_socketpair(args, from_user),
        #[cfg(mpos_net)]
        nr::SETSOCKOPT => crate::net::socket::handle_setsockopt(args, from_user),
        #[cfg(mpos_net)]
        nr::GETSOCKOPT => crate::net::socket::handle_getsockopt(args, from_user),
        // The multiple-message pair and the flagged accept: `sendmsg` and
        // `recvmsg` over an array of records, and `accept` with the two
        // descriptor flags the modern spelling takes.
        #[cfg(mpos_net)]
        nr::ACCEPT4 => crate::net::socket::handle_accept4(args, from_user),
        #[cfg(mpos_net)]
        nr::SENDMMSG => crate::net::socket::handle_sendmmsg(args, from_user),
        #[cfg(mpos_net)]
        nr::RECVMMSG => crate::net::socket::handle_recvmmsg(args, from_user),

        // --- process ---
        // `execve` runs on both machines: the handler is arch-neutral up to its
        // return-frame patch, which goes through `hal` (`set_exec_return`), and
        // the ptrace stop after it, which is a no-op where no tracer is attached.
        nr::EXECVE => proc::handle_execve(args, &cwd(), regs_ptr),
        nr::EXIT => proc::handle_exit(args),
        nr::EXIT_GROUP => proc::handle_exit_group(args),
        // `wait4(pid, *status, options, *rusage)` keeps its Linux argument
        // order: the handler reads the status pointer from slot 1 and the
        // options from slot 2.
        nr::WAIT4 => proc::handle_wait(args, from_user),
        nr::GETPID => proc::handle_getpid(),
        nr::GETTID => proc::handle_gettid(),
        nr::GETPPID => proc::handle_getppid(),
        nr::SCHED_YIELD => proc::handle_yield(),
        nr::NANOSLEEP => proc::handle_nanosleep(args, from_user),
        nr::CLOCK_NANOSLEEP => proc::handle_clock_nanosleep(args, from_user),
        nr::KILL => proc::handle_kill(args),
        nr::TKILL => proc::handle_kill(args),
        // `ptrace` and its bulk-memory relatives: one task observing another.
        // They are in `proc` because everything they need - the task table, the
        // register frames, the stops - is the process machinery's. All of them
        // run on both machines. The bulk pair and the signal wait never
        // consulted a register frame (`rt_sigtimedwait` needs only the pending
        // set and the generic `siginfo_t`, `process_vm_*` reaches the target's
        // memory through the scheduler), and `ptrace` itself no longer names one
        // either: the request words are one protocol, and which *slot* a role
        // lives in is `abi::uregs`'s per-column answer
        // (`RESULT`/`IP`/`STACK`). Un-gating it is what makes `strace` work on
        // the second machine - its very first act is `PTRACE_TRACEME`, which
        // used to answer `ENOSYS` here.
        nr::PTRACE => proc::handle_ptrace(args, from_user),
        nr::PROCESS_VM_READV => proc::handle_process_vm_readv(args, from_user),
        nr::PROCESS_VM_WRITEV => proc::handle_process_vm_writev(args, from_user),
        nr::RT_SIGTIMEDWAIT => proc::handle_rt_sigtimedwait(args, from_user),
        // `reboot` runs on both machines, and its gate used to be the stale
        // kind: it was written when the stop words ended at the x86_64
        // machine's `isa-debug-exit` port, and that port is a `hal` capability
        // now (`hal::exit_qemu`, which a machine without the flag answers by
        // halting). Nothing in the handler names a register or a port, so the
        // second machine serves it as it is - and the marker that said
        // otherwise was visible on its very first real userland: busybox
        // `init` calls `reboot(RB_DISABLE_CAD)` a second into its startup, and
        // on aarch64 that answered `mpos: syscall 142 -> ENOSYS` on every
        // boot. `t-reboot-libc` pins the policy words on both machines.
        nr::REBOOT => proc::handle_reboot(args),
        // The hostname is kept, not just accepted: `uname -n`, `hostname`
        // and `/proc/sys/kernel/hostname` all read it, and an init's
        // sysinit line changes it on every boot.
        nr::SETHOSTNAME => proc::handle_sethostname(args),
        nr::SYNC => 0, // every write already reached its storage: ramfs and ext2 write through
        nr::UMASK => abi::handle_umask(args),
        nr::SENDFILE => proc::handle_sendfile(args, from_user),
        nr::SETPGID => proc::handle_setpgid(args),
        nr::GETPGID => proc::handle_getpgid(args),
        // `getpgrp()` takes no argument, so there is nothing to read out of
        // the register file and the caller's own group is the whole answer.
        nr::GETPGRP => proc::handle_getpgid([0; 6]),
        // `clone` runs on both machines: the handler is arch-neutral up to the
        // child frame, which `hal::user_context` builds per-arch. This is the real
        // aarch64 `fork` path - aarch64 has no `fork` syscall number, `fork()` is
        // `clone(SIGCHLD)` - so a shell can spawn a child here. The post-`clone`
        // tracer announcement is a no-op where no tracer is attached.
        nr::CLONE => proc::handle_clone(args, regs_ptr),
        // `fork`/`vfork` have no aarch64 number at all (the table marks them
        // unreachable rather than absent), so the arm compiles there and can
        // never be matched; the handler itself is arch-neutral now.
        nr::FORK | nr::VFORK => proc::handle_fork(nr, args, regs_ptr),

        // --- memory ---
        nr::BRK => proc::handle_brk(args),
        nr::MMAP => proc::handle_mmap(args),
        nr::MUNMAP => proc::handle_munmap(args),
        nr::MPROTECT => proc::handle_mprotect(args),
        nr::MREMAP => proc::handle_mremap(args),
        nr::MADVISE => proc::handle_madvise(args),

        // --- stubs ---
        // Two spellings of one wait, with timeouts that are *not* the same
        // shape: `poll` takes a millisecond count, `ppoll` a `timespec *`.
        // Sharing a handler between them reads a pointer as a count, and the
        // wait then outlives the machine.
        nr::POLL => fs::poll(args),
        nr::PPOLL => fs::ppoll(args, from_user),
        nr::SELECT => fs::select(args),
        nr::UTIMENSAT => fs::utimensat(args, &cwd()),
        // `utimes(path, tv)` names no dirfd and carries a timeval struct -
        // which is moot, since timestamps are not kept. It lands on the same
        // path-led answer utimensat gives. (x86_64 has no `utime` number:
        // musl routes that one through utimensat itself.)
        nr::UTIMES => fs::utimensat([abi::AT_FDCWD as usize, args[0], args[1], 0, 0, 0], &cwd()),
        nr::SIGALTSTACK => sigaltstack(args),
        nr::GETITIMER => proc::handle_getitimer(args),
        nr::SETITIMER => proc::handle_setitimer(args),
        nr::ALARM => proc::handle_alarm(args),
        // The POSIX interval timer family. `timer_create` is what a libc
        // reaches for when a program asks for a *callback* on a timeout
        // (`SIGEV_THREAD`), which musl spells as a helper thread plus a
        // `SIGEV_THREAD_ID` timer carrying `SIGRTMIN` - the shape
        // `proc::handle_timer_create` answers.
        nr::TIMER_CREATE => proc::handle_timer_create(args, from_user),
        nr::TIMER_SETTIME => proc::handle_timer_settime(args, from_user),
        nr::TIMER_GETTIME => proc::handle_timer_gettime(args, from_user),
        nr::TIMER_GETOVERRUN => proc::handle_timer_getoverrun(args),
        nr::TIMER_DELETE => proc::handle_timer_delete(args),
        nr::SYSINFO => proc::handle_sysinfo(args),
        nr::CLOCK_GETTIME => proc::handle_clock_gettime(args),
        nr::GETTIMEOFDAY => proc::handle_gettimeofday(args),
        nr::UNAME => proc::handle_uname(args, true),
        nr::GETUID => posix::handle_getuid(args),
        nr::GETEUID => posix::handle_geteuid(args),
        nr::GETGID => posix::handle_getgid(args),
        nr::GETEGID => posix::handle_getegid(args),
        // setuid/setgid: root may set any id - a privilege dance like
        // sudo's depends on exactly that - and everyone else only an id
        // they already hold, which is the man page's own rule.
        nr::SETUID => posix::handle_setuid(args),
        nr::SETGID => posix::handle_setgid(args),
        nr::SETSID => proc::handle_setsid(args),
        nr::GETSID => proc::handle_getsid(args),
        nr::SETGROUPS => posix::handle_setgroups(args, from_user),
        nr::GETGROUPS => posix::handle_getgroups(args, from_user),
        nr::INOTIFY_INIT1 => fs::handle_inotify_init1(args),
        nr::INOTIFY_ADD_WATCH => fs::handle_inotify_add_watch(args, &cwd()),
        nr::INOTIFY_RM_WATCH => fs::handle_inotify_rm_watch(args),
        nr::RT_SIGACTION => sigaction(args),
        nr::RT_SIGPROCMASK => rt_sigprocmask(args),
        #[cfg(mpos_signal_frame)]
        nr::RT_SIGRETURN => proc::handle_rt_sigreturn(regs_ptr),
        #[cfg(not(target_arch = "aarch64"))]
        nr::ARCH_PRCTL => handle_arch_prctl(args),
        nr::PRCTL => abi::handle_prctl(args),
        nr::GETRANDOM => abi::handle_getrandom(args, true),
        nr::PRLIMIT64 => abi::handle_prlimit64(args, true),
        nr::FUTEX => proc::handle_futex(args),
        nr::SET_TID_ADDRESS => proc::handle_set_tid_address(args),
        nr::SCHED_SETAFFINITY => abi::handle_sched_setaffinity(args),
        nr::SCHED_GETAFFINITY => abi::handle_sched_getaffinity(args),
        nr::SET_ROBUST_LIST => abi::handle_set_robust_list(args),
        // `RSEQ` deliberately has no arm: it falls through to the `_ => ENOSYS`
        // below. The kernel does not carry the per-thread registration that
        // maintaining `cpu_id`/`node_id`/`mm_cid` (and honouring
        // `RSEQ_FLAG_UNREGISTER`) requires, so the honest answer is "not
        // available" - which is exactly the fallback a libc that probes `rseq`
        // and finds none is written to take. A former arm returned `0` for a
        // well-formed registration without writing any field the ABI contract
        // puts in the caller's block; §4.2 forbids that, and the caller it was
        // written for (glibc) registers at length 33/alignment 64 on a current
        // kernel, a shape that arm rejected anyway, so it honoured neither.
        // Doing `rseq` for real is a named gap. See `PLAN.md` A6.
        nr::CLEANUP_AFTER_KTHREAD => 0,

        // --- the calls an ordinary libc reaches for ---
        nr::PREAD64 => fs::handle_pread64(args, from_user),
        nr::PWRITE64 => fs::handle_pwrite64(args, from_user),
        nr::PREADV => fs::handle_preadv(args, from_user),
        nr::PWRITEV => fs::handle_pwritev(args, from_user),
        // The positioned-vectored pair with the advice words. `offset` of
        // -1 is the cursor-carrying spelling - the position the descriptor
        // itself keeps - and a flag the kernel does not know is the one
        // refusal, spelled EOPNOTSUPP the way the man page spells it.
        nr::PREADV2 => fs::handle_preadv2(args, from_user),
        nr::PWRITEV2 => fs::handle_pwritev2(args, from_user),
        // `fallocate`'s default operation is real: the zero-filled
        // extension a truncate makes is the allocated range the man page
        // promises. The interior-reshaping operations are refused with
        // the errno a filesystem without the feature gets.
        nr::FALLOCATE => fs::handle_fallocate(args),
        // The kernel-made copy: both ends regular files, a mount crossing
        // answered EXDEV like `link(2)`'s, and the offsets in-out.
        nr::COPY_FILE_RANGE => fs::handle_copy_file_range(args, from_user),
        nr::FSYNC | nr::FDATASYNC => fs::handle_fsync(args),
        nr::CHMOD => fs::handle_chmod(args, &cwd()),
        nr::FCHMOD => fs::handle_fchmod(args),
        nr::CHOWN => fs::handle_chown(args, &cwd()),
        nr::FCHOWNAT => fs::handle_fchownat(args, &cwd()),
        nr::LCHOWN => fs::handle_lchown(args, &cwd()),
        nr::FCHOWN => fs::handle_fchown(args),
        // The legacy `link` names two paths; `linkat` names two dirfds and
        // the paths under them, and is the shape the handler wants.
        nr::LINK => fs::handle_linkat(
            [
                abi::AT_FDCWD as usize,
                args[0],
                abi::AT_FDCWD as usize,
                args[1],
                0,
                0,
            ],
            &cwd(),
        ),
        nr::LINKAT => fs::handle_linkat(args, &cwd()),
        nr::MKNOD => fs::handle_mknod(args, &cwd()),
        // mount and umount2 are how userland builds the directory tree a
        // system needs - /proc, /sys, /dev, /run - which is init's job, not
        // the kernel's boot code's.
        nr::MOUNT => fs::handle_mount(args, &cwd()),
        nr::UMOUNT2 => fs::handle_umount2(args, &cwd()),
        nr::PIVOT_ROOT => fs::handle_pivot_root(args, &cwd()),
        nr::CHROOT => fs::handle_chroot(args, &cwd()),
        nr::SIGNALFD => fs::handle_signalfd(args, 0, from_user),
        nr::SIGNALFD4 => fs::handle_signalfd(args, args[3], from_user),
        nr::EVENTFD => fs::handle_eventfd(args),
        #[cfg(target_os = "none")]
        nr::MEMFD_CREATE => fs::handle_memfd_create(args, from_user),
        // --- the POSIX completion set ---
        //
        // The descriptor-backed timer and the interest-list multiplexer:
        // the pair of subsystems a poll loop is built on, and the two this
        // table was missing. The timer answers through `read` the way the
        // signalfd above answers through one; the epoll instance is an
        // anonymous inode whose interest list the ctl builds and the wait
        // walks.
        nr::TIMERFD_CREATE => fs::handle_timerfd_create(args),
        nr::TIMERFD_SETTIME => fs::handle_timerfd_settime(args, from_user),
        nr::TIMERFD_GETTIME => fs::handle_timerfd_gettime(args, from_user),
        nr::EPOLL_CREATE => fs::handle_epoll_create(args),
        nr::EPOLL_CREATE1 => fs::handle_epoll_create1(args),
        nr::EPOLL_CTL => fs::handle_epoll_ctl(args, from_user),
        nr::EPOLL_WAIT => fs::handle_epoll_wait(args, from_user),
        nr::EPOLL_PWAIT => fs::handle_epoll_pwait(args, from_user),
        nr::EPOLL_PWAIT2 => fs::handle_epoll_pwait2(args, from_user),
        // The *at and modern spellings of the file calls: the access check
        // with flags, the extensible open, the permission bits and the
        // node-maker relative to a dirfd, and the legacy `creat`/`utime`
        // numbers a libc still dials directly.
        nr::OPENAT2 => fs::handle_openat2(args, &cwd(), from_user),
        nr::FACCESSAT2 => fs::handle_faccessat2(args, &cwd()),
        nr::FCHMODAT => fs::handle_fchmodat(args, &cwd()),
        nr::FCHMODAT2 => fs::handle_fchmodat2(args, &cwd()),
        nr::MKNODAT => fs::handle_mknodat(args, &cwd()),
        nr::FUTIMESAT => fs::handle_futimesat(args, &cwd(), from_user),
        nr::CREAT => fs::handle_creat(args, &cwd()),
        nr::UTIME => fs::handle_utime(args, &cwd(), from_user),
        // The pipe-and-file mover, the eager read, the one-descriptor
        // flush, and the kernel log's own door - four calls whose whole
        // contract here is a rule each and the descriptor checks.
        nr::SPLICE => fs::handle_splice(args, from_user),
        nr::READAHEAD => fs::handle_readahead(args),
        nr::SYNCFS => fs::handle_syncfs(args),
        nr::SYSLOG => fs::handle_syslog(args, from_user),
        // The process half: identity, capabilities, scheduling, memory
        // advice, the wall clock's setters, and the self-aimed signals.
        // `posix` holds the whole family; the module's own opening note
        // spells what each subfamily answers.
        nr::SETREUID => posix::handle_setreuid(args),
        nr::SETREGID => posix::handle_setregid(args),
        nr::SETRESUID => posix::handle_setresuid(args),
        nr::GETRESUID => posix::handle_getresuid(args, from_user),
        nr::SETRESGID => posix::handle_setresgid(args),
        nr::GETRESGID => posix::handle_getresgid(args, from_user),
        nr::SETFSUID => posix::handle_setfsuid(args),
        nr::SETFSGID => posix::handle_setfsgid(args),
        nr::CAPGET => posix::handle_capget(args, from_user),
        nr::CAPSET => posix::handle_capset(args, from_user),
        nr::SCHED_SETPARAM => posix::handle_sched_setparam(args, from_user),
        nr::SCHED_GETPARAM => posix::handle_sched_getparam(args, from_user),
        nr::SCHED_SETSCHEDULER => posix::handle_sched_setscheduler(args, from_user),
        nr::SCHED_GETSCHEDULER => posix::handle_sched_getscheduler(args),
        nr::SCHED_GET_PRIORITY_MAX => posix::handle_sched_get_priority_max(args),
        nr::SCHED_GET_PRIORITY_MIN => posix::handle_sched_get_priority_min(args),
        nr::SCHED_RR_GET_INTERVAL => posix::handle_sched_rr_get_interval(args, from_user),
        nr::SCHED_SETATTR => posix::handle_sched_setattr(args, from_user),
        nr::SCHED_GETATTR => posix::handle_sched_getattr(args, from_user),
        nr::GETPRIORITY => posix::handle_getpriority(args),
        nr::SETPRIORITY => posix::handle_setpriority(args),
        nr::MSYNC => posix::handle_msync(args),
        nr::MINCORE => posix::handle_mincore(args, from_user),
        nr::MLOCK => posix::handle_mlock(args),
        nr::MUNLOCK => posix::handle_munlock(args),
        nr::MLOCKALL => posix::handle_mlockall(args),
        nr::MUNLOCKALL => posix::handle_munlockall(args),
        nr::MLOCK2 => posix::handle_mlock2(args),
        nr::PERSONALITY => posix::handle_personality(args),
        nr::VHANGUP => posix::handle_vhangup(args),
        nr::GETCPU => posix::handle_getcpu(args, from_user),
        nr::SETTIMEOFDAY => posix::handle_settimeofday(args, from_user),
        nr::CLOCK_SETTIME => posix::handle_clock_settime(args, from_user),
        nr::UNSHARE => posix::handle_unshare(args),
        nr::SETNS => posix::handle_setns(args),
        nr::SECCOMP => posix::handle_seccomp(args, from_user),
        nr::SETDOMAINNAME => posix::handle_setdomainname(args),
        nr::TGKILL => posix::handle_tgkill(args),
        nr::RT_SIGPENDING => posix::handle_rt_sigpending(args, from_user),
        nr::RT_SIGQUEUEINFO => posix::handle_rt_sigqueueinfo(args, from_user),
        nr::RT_SIGSUSPEND => posix::handle_rt_sigsuspend(args, from_user),
        nr::PAUSE => posix::handle_pause(),
        nr::FADVISE64 => fs::handle_fadvise64(args),
        nr::PSELECT6 => fs::pselect6(args),
        nr::TIME => proc::handle_time(args),
        nr::TIMES => proc::handle_times(args),
        nr::GETRUSAGE => proc::handle_getrusage(args),
        nr::CLOCK_GETRES => proc::handle_clock_getres(args),
        nr::WAITID => proc::handle_waitid(args),
        nr::GETRLIMIT => abi::handle_getrlimit(args, from_user),
        nr::SETRLIMIT => abi::handle_setrlimit(args, from_user),
        nr::MEMBARRIER => abi::handle_membarrier(args),
        nr::GET_ROBUST_LIST => abi::handle_get_robust_list(args),

        // --- extended attributes: validated, then `EOPNOTSUPP` ---
        // No filesystem here stores xattrs; the handlers check the arguments and
        // the target and answer that the operation is not supported, so a
        // probing caller learns "not supported on this filesystem" instead of
        // the fall-through "no such syscall". The `l`-forms act on the symlink
        // itself, the `f`-forms on a descriptor's file, and the number is passed
        // through because `setxattr` carries a value and a flags word the other
        // two do not.
        nr::GETXATTR | nr::SETXATTR | nr::REMOVEXATTR => fs::xattr_path(nr, args, &cwd(), true),
        nr::LGETXATTR | nr::LSETXATTR | nr::LREMOVEXATTR => fs::xattr_path(nr, args, &cwd(), false),
        nr::FGETXATTR | nr::FSETXATTR | nr::FREMOVEXATTR => fs::xattr_fd(nr, args),
        nr::LISTXATTR => fs::xattr_list_path(args, &cwd(), true),
        nr::LLISTXATTR => fs::xattr_list_path(args, &cwd(), false),
        nr::FLISTXATTR => fs::xattr_list_fd(args),

        _ => {
            // A call with no handler is worth saying out loud - once. A libc
            // probes for optional calls by number and treats ENOSYS as "not
            // available", which is a legitimate answer; what is not
            // legitimate is never finding out that something is missing.
            super::err::ENOSYS
        }
    };
    if ret == super::err::ENOSYS {
        // `ioctl` is the one number where the *request*, not the number, is the
        // information. `handle_ioctl` has a catch-all `ENOTTY`, so an `ENOSYS`
        // reaching here never means "there is no ioctl call" - only a device
        // refusing one request it was routed to (evdev's force-feedback and
        // key-repeat probes, a missing DRM callback). The generic note keyed by
        // number would print `syscall 16 -> ENOSYS`, which a reader takes as
        // "the ioctl call is missing" - the opposite of what happened. Name the
        // request, exactly as the `EOPNOTSUPP` arm below does.
        if nr == nr::IOCTL {
            note_enosys_ioctl(args[0] as i32, args[1] as u32);
        } else {
            note_enosys(nr);
        }
    }
    if ret == super::err::EOPNOTSUPP {
        // The one case where the number is not the information: `ioctl` is a
        // thousand requests behind one number, and the request word is what
        // says which device was asked for what. The fd-bearing calls
        // (`write`, `fallocate`, `llseek`, the xattr `f`-forms ...) name a
        // descriptor rather than a number too, so a refused `write` is only
        // diagnosable by which file it was aimed at - print that.
        if nr == nr::IOCTL {
            note_eopnotsupp_ioctl(args[0] as i32, args[1] as u32);
        } else {
            note_eopnotsupp(nr, args[0] as i32);
        }
    }
    ret
}

#[cfg(target_os = "none")]
use super::err;
#[cfg(target_os = "none")]
use crate::sched::current_cwd as cwd;

#[cfg(target_os = "none")]
const ARCH_SET_GS: usize = 0x1001;
#[cfg(target_os = "none")]
const ARCH_SET_FS: usize = 0x1002;
#[cfg(target_os = "none")]
const ARCH_GET_FS: usize = 0x1003;
#[cfg(target_os = "none")]
const ARCH_GET_GS: usize = 0x1004;
#[cfg(target_os = "none")]
const MSR_GS_BASE: u32 = 0xC000_0101;

#[cfg(target_os = "none")]
/// Record that `nr` answered ENOSYS, printing it the first time only.
///
/// Two ways to get here, and the log does not distinguish them because a
/// caller cannot: a number with no handler at all, and a handler that
/// refuses what it was asked - `clone(CLONE_THREAD)`, `mmap` with a shared
/// anonymous mapping. Both are a program being told this kernel will not do
/// that, which is worth saying once and not once per call: a libc in a
/// startup loop asks for the same thing dozens of times.
fn note_enosys(nr: u64) {
    if abi::mark_missing(nr) {
        crate::print!("mpos: syscall {nr} -> ENOSYS\n");
    }
}

#[cfg(target_os = "none")]
/// Record that a call was asked for something this kernel will not do, naming
/// the number once. "Not supported" is a legitimate answer a libc probes for -
/// `O_TMPFILE` on a filesystem without it, a `SEEK_HOLE` no driver answers -
/// and printing it on every occurrence turned one such call into a line per
/// attempt on the console. `fd` is the call's first argument, which for the
/// descriptor-taking syscalls names the file that refused - `write(1)` alone
/// only says *a* write was refused, not whose - so it is printed beside it.
/// Path-taking calls name nothing useful in `fd`, and there it is noise.
fn note_eopnotsupp(nr: u64, fd: i32) {
    if abi::mark_unsupported(nr) {
        crate::print!("mpos: syscall {nr} (fd {fd}) -> EOPNOTSUPP\n");
    }
}

#[cfg(target_os = "none")]
/// The `ioctl` case of [`note_eopnotsupp`], naming the request instead of the
/// call. "syscall 16" says a device refused something; the request word carries
/// the device's type letter and the request number, which is the whole
/// difference between a framebuffer mode request and an interface rename.
///
/// Bounded rather than once-per-number because the interesting part is the
/// request, and a program that probes in a loop would otherwise print one line
/// per attempt - the same reason the report above exists at all.
fn note_eopnotsupp_ioctl(fd: i32, request: u32) {
    static REFUSED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    if REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 8 {
        crate::print!("mpos: ioctl({fd}) request {request:#x} -> EOPNOTSUPP\n");
    }
}

#[cfg(target_os = "none")]
/// The `ENOSYS` case of [`note_eopnotsupp_ioctl`] - a device answering "I cannot
/// do that request" with `ENOSYS` rather than `EOPNOTSUPP` (evdev's
/// force-feedback and key-repeat probes are the ones a boot actually hits).
///
/// Its own counter, not a shared one, for the reason [`note_enosys`] and
/// [`note_eopnotsupp`] keep separate tables: one descriptor can refuse two
/// different requests with the two errnos, and a single budget spent by
/// whichever probe came first would silence the other. Bounded rather than
/// once-per-number because the request word is the useful part and a prober in
/// a loop would otherwise print a line per attempt.
fn note_enosys_ioctl(fd: i32, request: u32) {
    static REFUSED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    if REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 8 {
        crate::print!("mpos: ioctl({fd}) request {request:#x} -> ENOSYS\n");
    }
}

/// Record that a *futex operation* answered ENOSYS, printing it the first time
/// only.
///
/// The call itself is implemented and the operation is not, which is the one
/// refusal `note_enosys` cannot describe: it is keyed by syscall number, and
/// every one of these arrives as futex. Naming the operation is the whole
/// difference between a log that says "something about futex is missing" and one
/// that says which of the twenty-odd things the command word can mean is - and
/// the answer decides whether the gap is a caller falling back or a caller
/// losing the guarantee it asked for.
#[cfg(target_os = "none")]
pub(super) fn note_missing_futex_op(op: usize) {
    if abi::mark_missing_futex_op(op) {
        crate::print!("mpos: futex op {op:#x} -> ENOSYS\n");
    }
}

/// The 32-byte `struct sigaction` of the Linux x86_64 ABI, in musl's
/// `sigsetsize = 8` shape: handler, flags, restorer, then one 8-byte mask.
/// `handler` is a user address, or `SIG_DFL` (0) / `SIG_IGN` (1).
#[cfg(target_os = "none")]
#[repr(C)]
#[derive(Clone, Copy)]
struct UserSigAction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

/// `rt_sigaction(sig, act, old, sigsetsize)`.
///
/// Either pointer may be null, and each is used on its own: `act` records a
/// disposition, `old` reports the one in force. A query has to *write* its
/// answer - answering 0 and leaving the struct alone hands the caller the
/// bytes it happened to have there, which for a fresh process means sentinels,
/// not `SIG_DFL`.
///
/// The disposition is recorded per process and it *is* acted on: a caught
/// signal builds an `rt_sigframe` and runs the handler on the task's return
/// path (`signal::build_frame`), and a signal nobody catches or ignores takes
/// its `default_action`. This used to read "no handler is ever called, because
/// no user frame is built for one" - that stopped being true when signal
/// delivery landed, and the handler-running path is what `t-signal-delivery`
/// and `t-tty-eintr` now pin. `SA_RESTART` is honoured too, in the frame rather
/// than here: a disposition carrying it makes an interrupted call one Linux
/// rewinds (`abi::restartable` decides *which* calls) resume on the `syscall`
/// instruction with its own arguments still in place, so a caller that sets the
/// flag never sees `EINTR`. The stored `old` disposition is read back verbatim.
#[cfg(target_os = "none")]
fn sigaction(args: [usize; 6]) -> isize {
    let (sig, act, old) = (args[0], args[1], args[2]);
    // Linux's refusal set: signal 0 is not a signal, numbers above `_NSIG` (64)
    // are not signals, and SIGKILL/SIGSTOP cannot be given a disposition by
    // anyone. The real-time range `32..=64` is nameable and installable here, as
    // on Linux (`catchable` now spans it).
    if !crate::sched::signal::catchable(sig) {
        return err::EINVAL;
    }
    let id = crate::sched::current();
    if act != 0 {
        if !super::buffer_ok(act, core::mem::size_of::<UserSigAction>(), false, true) {
            return err::EFAULT;
        }
        let incoming = unsafe { core::ptr::read(act as *const UserSigAction) };
        let action = crate::sched::signal::Action {
            handler: incoming.handler,
            flags: incoming.flags,
            restorer: incoming.restorer,
            mask: incoming.mask,
        };
        if !crate::sched::set_signal_action(id, sig as u8, action) {
            return err::ESRCH;
        }
    }
    if old != 0 {
        if !super::buffer_ok(old, core::mem::size_of::<UserSigAction>(), true, true) {
            return err::EFAULT;
        }
        let Some(previous) = crate::sched::signal_action(id, sig as u8) else {
            return err::ESRCH;
        };
        let outgoing = UserSigAction {
            handler: previous.handler,
            flags: previous.flags,
            restorer: previous.restorer,
            mask: previous.mask,
        };
        unsafe { core::ptr::write(old as *mut UserSigAction, outgoing) };
    }
    0
}

/// `SS_DISABLE`, the `ss_flags` bit that says "there is no alternate stack".
#[cfg(target_os = "none")]
const SS_DISABLE: i32 = 2;

/// `rt_sigprocmask(how, set, oldset, sigsetsize)`.
///
/// mpos keeps no per-process mask - every signal is delivered the moment it
/// arrives - so a change is validated and then forgotten, and every query
/// reports an empty mask. The validation is still real: Linux refuses an
/// unknown `how`, a `sigsetsize` other than the ABI's 8, a `set` that cannot
/// be read, and an `oldset` that cannot be written. A query must *write* its
/// answer - a fresh process owes its caller zeros, not the sentinels it
/// happened to have there.
#[cfg(target_os = "none")]
fn rt_sigprocmask(args: [usize; 6]) -> isize {
    use crate::sched::signal::{SIGKILL, SIGSTOP, bit};

    const SIG_BLOCK: usize = 0;
    const SIG_UNBLOCK: usize = 1;
    const SIG_SETMASK: usize = 2;
    let (how, set, old, sigsetsize) = (args[0], args[1], args[2], args[3]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    if how > SIG_SETMASK {
        return err::EINVAL;
    }

    let mut requested = 0u64;
    if set != 0 {
        if !super::buffer_ok(set, 8, false, true) {
            return err::EFAULT;
        }
        requested = unsafe { (set as *const u64).read_unaligned() };
    }
    // The wire's `sigset_t` holds signal *n* in bit *n-1*; the kernel's mask
    // arithmetic names signal *n* by bit *n* (`signal::Pending`), and this
    // boundary is where the two spellings are told apart. Converting here is
    // what makes a libc's `sigaddset(SIGTERM)` actually block `SIGTERM` rather
    // than its neighbour — the off-by-one both sides could otherwise live
    // with happily, because no test inside the kernel speaks the wire.
    let requested = crate::sched::signal::from_wire(requested);

    // The query half is answered first and from the mask as it stands, which
    // is what makes the common `rt_sigprocmask(0, NULL, &old, 8)` a pure read.
    let current = crate::sched::current_blocked();
    if old != 0 {
        if !super::buffer_ok(old, 8, true, true) {
            return err::EFAULT;
        }
        unsafe { (old as *mut u64).write_unaligned(crate::sched::signal::to_wire(current)) };
    }

    if set != 0 {
        let next = match how {
            SIG_BLOCK => current | requested,
            SIG_UNBLOCK => current & !requested,
            _ => requested,
        };
        // Two signals cannot be blocked by anyone: they are how the kernel
        // stops a process that has stopped listening, and a mask that could
        // hold them would make a process unstoppable.
        let reserved = bit(SIGKILL) | bit(SIGSTOP);
        crate::sched::set_current_blocked(next & !reserved);
    }
    0
}

/// The 24-byte `stack_t` of the Linux x86_64 ABI.
#[cfg(target_os = "none")]
#[repr(C)]
#[derive(Clone, Copy)]
struct UserStackT {
    ss_sp: usize,
    ss_flags: i32,
    pad: i32,
    ss_size: usize,
}

/// `sigaltstack(ss, old)`.
///
/// As with `rt_sigaction`, a query has to *write* its answer: the caller seeds
/// the out-struct, and a fresh process must read back `SS_DISABLE` with no
/// stack rather than the sentinels it happened to have there.
///
/// An install is recorded per process. Nothing switches to the stack yet - a
/// signal still kills rather than calling a handler - but a stack the caller
/// asked for is a fact about the process, and Linux reports it back.
#[cfg(target_os = "none")]
fn sigaltstack(args: [usize; 6]) -> isize {
    let (ss, old) = (args[0], args[1]);
    let id = crate::sched::current();
    if ss != 0 {
        if !super::buffer_ok(ss, core::mem::size_of::<UserStackT>(), false, true) {
            return err::EFAULT;
        }
        let incoming = unsafe { core::ptr::read(ss as *const UserStackT) };
        // Linux refuses a flag word with bits it does not know before it
        // looks at anything else.
        if incoming.ss_flags & !SS_DISABLE != 0 {
            return err::EINVAL;
        }
        if incoming.ss_flags & SS_DISABLE != 0 {
            if !crate::sched::set_signal_alt_stack(id, None) {
                return err::ESRCH;
            }
        } else {
            // Linux answers ENOMEM, not EINVAL, for a stack too small for a
            // signal frame, and the same for an unset base.
            if incoming.ss_sp == 0 || incoming.ss_size < crate::sched::signal::MINSIGSTKSZ {
                return err::ENOMEM;
            }
            let stack = crate::sched::signal::AltStack {
                sp: incoming.ss_sp,
                size: incoming.ss_size,
            };
            if !crate::sched::set_signal_alt_stack(id, Some(stack)) {
                return err::ESRCH;
            }
        }
    }
    if old != 0 {
        if !super::buffer_ok(old, core::mem::size_of::<UserStackT>(), true, true) {
            return err::EFAULT;
        }
        let outgoing = match crate::sched::signal_alt_stack(id) {
            Some(stack) => UserStackT {
                ss_sp: stack.sp,
                ss_flags: 0,
                pad: 0,
                ss_size: stack.size,
            },
            None => UserStackT {
                ss_sp: 0,
                ss_flags: SS_DISABLE,
                pad: 0,
                ss_size: 0,
            },
        };
        unsafe { core::ptr::write(old as *mut UserStackT, outgoing) };
    }
    0
}

/// `arch_prctl(code, addr)`.
///
/// The SET forms install a base; the GET forms write it into `*(u64 *)addr` and
/// answer 0, which is the shape the man page and every libc wrapper agree on - a
/// base returned in `rax` instead would leave the caller reading whatever its
/// buffer already held.
///
/// The two registers are handled differently on purpose. `FS` is the one musl
/// actually uses for thread-local storage, and this kernel keeps it per *task*:
/// `prepare` reinstalls it on every dispatch, so a bare register write would be
/// undone by the next context switch. The call therefore goes through the
/// scheduler's copy. `GS` is not used by any program that runs here yet, and it
/// has no per-task home, so it stays a direct register access.
///
/// Deferred off aarch64: the `GS` half is raw `wrmsr`/`rdmsr` on an x86 model
/// specific register, and `arch_prctl(2)` with its `ARCH_*` request words is an
/// x86_64 syscall that has no aarch64 number at all. Its dispatch arm is gated
/// to match; aarch64 reaches its thread pointer through `hal::Cpu`'s TLS
/// capability instead, not a syscall.
#[cfg(all(target_os = "none", not(target_arch = "aarch64")))]
fn handle_arch_prctl(args: [usize; 6]) -> isize {
    let (cmd, addr) = (args[0], args[1]);
    match cmd {
        ARCH_SET_FS => {
            crate::sched::set_current_tls(addr);
            0
        }
        ARCH_GET_FS => {
            if !super::buffer_ok(addr, 8, true, true) {
                return err::EFAULT;
            }
            unsafe { (addr as *mut u64).write_unaligned(crate::sched::current_tls() as u64) };
            0
        }
        ARCH_SET_GS => {
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") MSR_GS_BASE,
                    in("eax") addr as u32,
                    in("edx") (addr >> 32) as u32,
                );
            }
            0
        }
        ARCH_GET_GS => {
            let (mut lo, mut hi): (u32, u32);
            unsafe {
                core::arch::asm!(
                    "rdmsr",
                    in("ecx") MSR_GS_BASE,
                    out("eax") lo,
                    out("edx") hi,
                );
            }
            if !super::buffer_ok(addr, 8, true, true) {
                return err::EFAULT;
            }
            let val = ((hi as u64) << 32) | (lo as u64);
            unsafe { (addr as *mut u64).write_unaligned(val) };
            0
        }
        _ => err::EINVAL,
    }
}
