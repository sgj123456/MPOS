#![no_std]

//! The arch-neutral Linux ABI facts: the `clone(2)` flag bits, the `ptrace(2)`
//! protocol constants, a process's default umask, and the resource-limit model.
//!
//! Why a leaf and not the syscall layer. `sched` reads every one of these on a
//! path that has nothing to do with dispatching a syscall - it builds a task's
//! initial umask, budgets its rlimits, decodes a `clone` flag word to decide
//! what a child shares, and decides from these `PTRACE_*` bits what a traced
//! task does at a stop. Naming `crate::syscall` for those values is what made
//! `sched` unbuildable without the x86_64 ABI layer (`NOTES.md` §11.2, the
//! `sched -> syscall` row); the ptrace protocol was the last reference to
//! leave, and moving it here took that row to 0. The values themselves are
//! shared across architectures - these `CLONE_*` bits, `PTRACE_*` words and
//! `RLIMIT_*` rows carry the same number on x86_64 and aarch64, because both
//! use the generic definitions - so they belong below the machine-specific ABI,
//! in a crate that a scheduler on any machine may name. What deliberately stays
//! in `syscall` is the part that is not arch-neutral: the syscall *numbers*
//! (`nr.rs`'s non-`CLONE` table), the architecture's register-file shape (the
//! x86_64 `user_regs_struct`, which is why `PTRACE_GETREGS` stays there too),
//! and the per-arch `MMAP_BASE`, which follows each architecture's
//! address-space layout and is decided with the aarch64 process-running stage
//! (`NOTES.md` §9.1's stage 6/7).
//!
//! This is the single source for each fact. `syscall::linux::nr` and
//! `syscall::linux::abi` re-export these rather than redefining them, so the
//! `unshare`/`clone` handlers and the `sched` core read one table, not two.

/// The mode bits a fresh process masks creation with: `0o022`, the `umask(2)`
/// default every shell and init inherits. `sched` seeds a task's umask with this
/// and the syscall layer reads it back for `umask(2)`'s first answer.
pub const DEFAULT_UMASK: u32 = 0o022;

/// The magic `release` word that turns `uname(2)` into §10.5's disk-read
/// counter.
///
/// A *private* ABI word, and saying so is the whole disclosure: Linux's
/// `kernel_osrelease` is a string no caller compares against a number, so a
/// value of `0x6D70_6F73_0000_0001` ("mpos" + version 1) in that field cannot
/// collide with anything a real kernel answers - and a program that parses
/// `uname -r` sees an odd-but-plausible release string tail rather than an
/// error. This is the same door `t-diskbench` reads its numbers through because
/// there is no other door to put them behind: `/proc/partitions` is column-
/// parsed by tools (`fdisk` opens it before anything else), so appending a
/// sixth column would break the readers that exist, and this kernel has no
/// sysfs attribute store a `/sys/class/block/*/stat` file could live in. The
/// counter lives where a measurement can reach it, and the fact that it is
/// private is written down here, in the UAPI table, and beside the handler.
///
/// The payload grammar is one row per registered disk, rendered live at call
/// time: `version` = `"#1 "` followed by `<name>:<reads>:<writes>,` for every
/// disk in registration order. The reader-side spelling of both halves — the
/// word and the rows — is `user/tests/src/bin/t-diskbench.rs`.
pub const DISKSTAT_OSRELEASE_MAGIC: u64 = 0x6D70_6F73_0000_0001;

// `clone(2)` flag bits. Each one names a part of the caller that the child
// **shares** instead of copying; a zero flag word is therefore plain `fork`.
//
// The eight namespace flags below put "give me a namespace of my own" in the
// same word as "share this part of me with the child." They live here rather
// than beside either handler because two calls read them - `unshare(2)` and
// `clone(2)` - and `sched` decodes them on the fork path; a table copied into
// each consumer would be several tables. `CLONE_NEWTIME` is the one worth
// staring at: Linux puts it at `0x80`, inside the low byte the exit signal
// (`CSIGNAL`, 0..=0x7f) lives in, so "a `CLONE_NEW*` is a bit in the high half"
// is a rule the header does not follow and a reader must not assume. Both of
// those were spelled wrong here once (`CLONE_NEWNS` as `CLONE_THREAD`,
// `CLONE_NEWTIME` as `CLONE_SETTLS`), which is why these values are pinned by a
// test and a comment rather than trusted.
pub const CLONE_VM: usize = 0x0000_0100;
pub const CLONE_FS: usize = 0x0000_0200;
pub const CLONE_FILES: usize = 0x0000_0400;
pub const CLONE_SIGHAND: usize = 0x0000_0800;
pub const CLONE_VFORK: usize = 0x0000_4000;
pub const CLONE_PARENT: usize = 0x0000_8000;
pub const CLONE_THREAD: usize = 0x0001_0000;
pub const CLONE_SYSVSEM: usize = 0x0004_0000;
pub const CLONE_SETTLS: usize = 0x0008_0000;
pub const CLONE_PARENT_SETTID: usize = 0x0010_0000;
pub const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;
/// Historical no-op. Linux removed its meaning years ago but still accepts
/// the bit, and musl's `pthread_create` puts it in every thread's flag word -
/// refusing it refuses every pthread in the system.
pub const CLONE_DETACHED: usize = 0x0040_0000;
/// Historical no-op, same treatment: accepted so a caller from a kernel that
/// still sends it is not refused for a bit that does nothing.
pub const CLONE_UNTRACED: usize = 0x0080_0000;
pub const CLONE_CHILD_SETTID: usize = 0x0100_0000;

pub const CLONE_NEWTIME: usize = 0x0000_0080;
pub const CLONE_NEWNS: usize = 0x0002_0000;
pub const CLONE_NEWCGROUP: usize = 0x0200_0000;
pub const CLONE_NEWUTS: usize = 0x0400_0000;
pub const CLONE_NEWIPC: usize = 0x0800_0000;
pub const CLONE_NEWUSER: usize = 0x1000_0000;
pub const CLONE_NEWPID: usize = 0x2000_0000;
pub const CLONE_NEWNET: usize = 0x4000_0000;

/// All eight namespace bits at once - the mask that says "these bits are
/// namespace requests, not sharing requests", which is the distinction both
/// callers have to make before they can answer anything.
pub const CLONE_NAMESPACES: usize = CLONE_NEWTIME
    | CLONE_NEWNS
    | CLONE_NEWCGROUP
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWNET;

// ---- the `ptrace(2)` protocol --------------------------------------------
//
// What a tracer and a tracee say to each other, as opposed to what they say it
// about. These values come from Linux's `linux/ptrace.h`, which is
// architecture-independent, so they carry the same number on x86_64 and aarch64
// - the same reason the `CLONE_*` bits above are here. Both sides read them:
// `sched`'s stop block decides with them what a tracee does, and `syscall`'s
// ptrace handler decodes them, and neither should have to name the other to
// agree on a bit.
//
// What deliberately does *not* live here is anything shaped by the register
// file. `PTRACE_GETREGS` and its three siblings exist only on the x86_64 column
// and fill the x86_64 `user_regs_struct`, so those request numbers stay in
// `syscall::linux::abi`, where the architecture's shape belongs.

/// `PTRACE_O_*`: what the tracer asked to be told about.
///
/// `TRACESYSGOOD` is the one that matters most in practice, and for a
/// cosmetic-looking reason that is not cosmetic: without it a syscall stop is
/// reported as a plain `SIGTRAP`, which a debugger cannot tell from a
/// breakpoint or a real `SIGTRAP` arriving. With it the reported signal is
/// `SIGTRAP | 0x80`, which cannot be anything else.
pub const PTRACE_O_TRACESYSGOOD: usize = 0x0000_0001;
pub const PTRACE_O_TRACEFORK: usize = 0x0000_0002;
pub const PTRACE_O_TRACEVFORK: usize = 0x0000_0004;
pub const PTRACE_O_TRACECLONE: usize = 0x0000_0008;
pub const PTRACE_O_TRACEEXEC: usize = 0x0000_0010;
pub const PTRACE_O_TRACEVFORKDONE: usize = 0x0000_0020;
pub const PTRACE_O_TRACEEXIT: usize = 0x0000_0040;
/// Ask to be told when a `SECCOMP_RET_TRACE` filter selects the running
/// syscall: the tracer gets a `PTRACE_EVENT_SECCOMP` stop before the call is
/// made. Without it a `TRACE` decision has no tracer to hand the call to and
/// the call fails `ENOSYS` (measured - a tracer present but silent is no
/// better than none).
pub const PTRACE_O_TRACESECCOMP: usize = 0x0000_0080;
pub const PTRACE_O_EXITKILL: usize = 0x0010_0000;

/// The bit `TRACESYSGOOD` sets in the reported stop signal.
pub const SYSCALL_TRAP: u8 = 0x80;

/// `PTRACE_EVENT_*`: the value in bits 23:16 of a `SIGTRAP` stop's status
/// word. Zero means "an ordinary stop", which is every syscall boundary and
/// every signal delivery.
pub const PTRACE_EVENT_NONE: u32 = 0;

/// The stop a tracee makes to announce a child it has just created.
///
/// Which of the three it is comes from the flags the child was asked for, not
/// from the call: `fork`, `vfork` and a thread-creating `clone` are one syscall
/// with different flags, and a tracer that asked about threads but not about
/// processes needs to be told which one it got. At this stop
/// `PTRACE_GETEVENTMSG` answers the **new child's pid**.
pub const PTRACE_EVENT_FORK: u32 = 1;
pub const PTRACE_EVENT_VFORK: u32 = 2;
pub const PTRACE_EVENT_CLONE: u32 = 3;

/// The exec event: raised on the far side of an `execve` that succeeded.
/// `PTRACE_GETEVENTMSG` answers the tid the task had *before* it exec'd at this
/// stop, which is the question a tracer that reassembles a thread group after
/// the leader exec'd is asking.
pub const PTRACE_EVENT_EXEC: u32 = 4;

/// The seccomp event: raised on entry to a syscall a `SECCOMP_RET_TRACE` filter
/// selected, before the call is made. `PTRACE_GETEVENTMSG` answers the action's
/// **data half** here (measured, `.workbuddy/probe-eventmsg-harness2.py`: a
/// `RET_TRACE|0x1234` on `gettid` reported `0x1234`), which is the number a
/// supervisor uses to route the call.
pub const PTRACE_EVENT_SECCOMP: u32 = 7;

/// The events this kernel accepts a name for and does not produce.
///
/// `VFORK_DONE` and `EXIT` are each a separate stop a tracer can ask to be told
/// about with the matching `PTRACE_O_*`, and mpos raises neither: an `EXIT` stop
/// would be a second stop at the very end of a task's life, in the middle of the
/// release path a terminating task is already inside, and `VFORK_DONE` needs a
/// parent that is suspended for the child's benefit - which mpos's `clone`
/// deliberately is not.
///
/// They are named rather than absent because `PTRACE_SETOPTIONS` refuses an
/// option it cannot name, and a tracer that died of `EINVAL` because the kernel
/// had never heard of `PTRACE_O_TRACEEXIT` would be worse served than one that
/// asked and was quietly not told. The allowance lives in one module so that the
/// set of "named but never produced" is one line to read.
pub mod unwatched_events {
    pub const VFORK_DONE: u32 = 5;
    pub const EXIT: u32 = 6;
}

/// How many resource limits a process carries. The rows are indexed by the
/// `RLIMIT_*` numbers, which are the same on x86_64 and aarch64 (asm-generic):
/// `3` stack, `6` process count, `7` descriptor table. The two rows between them
/// are `4` core file and `5` resident set. A table that put the descriptor
/// ceiling at `5` - which this one did until S6's batch 10 read the numbering
/// off a maintained table instead of off the kernel - answered
/// `getrlimit(RLIMIT_NOFILE)` with the process-count budget and `RLIMIT_RSS`
/// with the descriptor one.
pub const RLIM_NLIMITS: usize = 16;
/// `RLIM_INFINITY`: what an unlimited limit reads as.
pub const RLIM_INFINITY: u64 = u64::MAX;

/// One resource limit pair: the soft ceiling a process is held to and the hard
/// ceiling it may raise the soft one to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rlimit {
    pub cur: u64,
    pub max: u64,
}

/// The limits a fresh process starts with: an 8 MiB stack, a descriptor
/// ceiling, a process count, and infinity for the rows nothing budgets. Every
/// other resource reads these until `setrlimit` says otherwise.
///
/// ```
/// use mpos_abi::{Rlimit, RLIM_INFINITY, default_rlimits};
///
/// let t = default_rlimits();
/// // The three budgeted rows sit at `asm-generic/resource.h`'s numbers:
/// // RLIMIT_STACK (3), RLIMIT_NPROC (6), RLIMIT_NOFILE (7) - so a caller
/// // asking for the descriptor ceiling reads the descriptor ceiling.
/// assert_eq!((t[3].cur, t[3].max), (8 * 1024 * 1024, 8 * 1024 * 1024));
/// assert_eq!((t[6].cur, t[6].max), (4096, 4096));
/// assert_eq!((t[7].cur, t[7].max), (1024, 1_048_576));
/// // And the row the descriptor ceiling used to sit on (RLIMIT_RSS, 5)
/// // reads infinity - the two must never swap back.
/// assert_eq!(t[5], Rlimit { cur: RLIM_INFINITY, max: RLIM_INFINITY });
/// ```
///
/// The three rows are `RLIMIT_STACK` (3), `RLIMIT_NPROC` (6) and
/// `RLIMIT_NOFILE` (7) - the numbers `asm-generic/resource.h` gives, and so the
/// ones a caller's `getrlimit`/`setrlimit`/`prlimit64` speaks. They were written
/// at `5` and `7` (Linux's `RLIMIT_RSS` and `RLIMIT_NPROC` rows), which made a
/// caller asking for the descriptor ceiling read the process count and left the
/// real `RLIMIT_NOFILE` row holding it.
pub fn default_rlimits() -> [Rlimit; RLIM_NLIMITS] {
    let mut t = [Rlimit {
        cur: RLIM_INFINITY,
        max: RLIM_INFINITY,
    }; RLIM_NLIMITS];
    t[3] = Rlimit {
        cur: 8 * 1024 * 1024,
        max: 8 * 1024 * 1024,
    }; // RLIMIT_STACK
    t[6] = Rlimit {
        cur: 4096,
        max: 4096,
    }; // RLIMIT_NPROC
    t[7] = Rlimit {
        cur: 1024,
        max: 1_048_576,
    }; // RLIMIT_NOFILE
    t
}

/// `setrlimit`'s one rule, returned as the errno a refusal carries (or the
/// accepted pair): the soft limit may not exceed the hard, and only root may
/// lift the hard ceiling above where it stands.
///
/// ```
/// use mpos_abi::{Rlimit, rlimit_admit};
///
/// let standing = Rlimit { cur: 100, max: 200 };
/// // A soft above the *new* hard is refused whatever else the pair says.
/// assert_eq!(
///     rlimit_admit(standing, Rlimit { cur: 300, max: 200 }, true),
///     Err(mpos_err::EINVAL)
/// );
/// // Lowering within the standing hard is accepted for anyone.
/// assert_eq!(
///     rlimit_admit(standing, Rlimit { cur: 50, max: 60 }, false),
///     Ok(Rlimit { cur: 50, max: 60 })
/// );
/// // Raising the hard ceiling above where it stands is root's alone.
/// assert_eq!(
///     rlimit_admit(standing, Rlimit { cur: 10, max: 300 }, false),
///     Err(mpos_err::EPERM)
/// );
/// assert_eq!(
///     rlimit_admit(standing, Rlimit { cur: 10, max: 300 }, true),
///     Ok(Rlimit { cur: 10, max: 300 })
/// );
/// ```
pub fn rlimit_admit(old: Rlimit, new: Rlimit, is_root: bool) -> Result<Rlimit, isize> {
    if new.cur > new.max {
        return Err(mpos_err::EINVAL);
    }
    if !is_root && new.max > old.max {
        return Err(mpos_err::EPERM);
    }
    Ok(new)
}

/// What a `clone`'s flag word asks *of the new task's namespaces*, decided
/// before anything is created. Linux creates the child inside copied
/// namespaces (`create_new_namespaces`, one flag per kind, behind
/// `CAP_SYS_ADMIN` checked there too); this kernel serves the kinds whose
/// copies are wired into the child's construction — the names (`copy_utsname`;
/// `t-clone`), the mount table (the child starts in a copy; `t-ns-mnt`), a pid
/// namespace to be the child's own init, a fresh network stack, and a fresh
/// **user** namespace the child is *born* in (`t-ns-user`'s child-side claims)
/// — and refuses every other kind whole. A caller asking for IPC or cgroup
/// isolation gets `EOPNOTSUPP`: nothing to hand over yet.
///
/// The capability gate is two rules, and the split is measured. A word
/// **without** `CLONE_NEWUSER` asks for its kinds against the caller's own
/// namespace, so an unprivileged caller is `EPERM` (`.workbuddy/probe-userns10.c`:
/// `CLONE_NEWUTS` and `CLONE_NEWNS` alone are, and so is every other kind
/// alone). A word **carrying** `CLONE_NEWUSER` asks against the fresh namespace
/// the child is already in — and making a user namespace *is* the grant, so
/// that namespace's holder has the full set and every kind beside it is
/// created: measured, `CLONE_NEWUSER` beside each of `NEWUTS`, `NEWNS`,
/// `NEWPID`, `NEWNET`, `NEWIPC` and `NEWCGROUP` all succeed unprivileged. So
/// `CLONE_NEWUSER` does not merely pass the gate, it **lifts** it for the rest
/// of the word.
///
/// The unserved-kind refusal is `EOPNOTSUPP`, not `EINVAL`: the bit is a real
/// namespace flag (an unknown bit is the caller's own validation job, upstream
/// of this), and what is missing is this kernel's ability, which is precisely
/// what `EOPNOTSUPP` means (`AGENTS.md` §3.2).
///
/// This is the leaf's rather than the syscall layer's because its reader is the
/// scheduler — `sched::clone` answers it before creating anything, and a
/// `sched → syscall` edge is exactly the V-* class Phase 11 keeps at zero. The
/// namespace bits it consults are defined above, so the rule has one home.
///
/// ```
/// use mpos_abi::{CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWUSER, CLONE_THREAD,
///                clone_namespace_verdict};
///
/// // A served kind needs CAP_SYS_ADMIN against the caller's own namespace.
/// assert_eq!(clone_namespace_verdict(CLONE_NEWNS, true), Ok(()));
/// assert_eq!(clone_namespace_verdict(CLONE_NEWNS, false), Err(mpos_err::EPERM));
/// // Making a user namespace *is* the grant, so it passes unprivileged and
/// // lifts the gate for every other served kind in the same word.
/// assert_eq!(clone_namespace_verdict(CLONE_NEWUSER, false), Ok(()));
/// assert_eq!(
///     clone_namespace_verdict(CLONE_NEWUSER | CLONE_NEWNS, false),
///     Ok(())
/// );
/// // A pid namespace beside a thread is the request being impossible, not a
/// // missing capability - EINVAL even for root.
/// assert_eq!(
///     clone_namespace_verdict(CLONE_THREAD | CLONE_NEWPID, true),
///     Err(mpos_err::EINVAL)
/// );
/// ```
pub fn clone_namespace_verdict(flags: usize, privileged: bool) -> Result<(), isize> {
    const SERVED: usize = CLONE_NEWUTS | CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWUSER;
    let asked = flags & CLONE_NAMESPACES;
    // The pairs Linux itself calls *invalid*, from `clone(2)`'s ERRORS list:
    // "CLONE_NEWPID and one (or both) of CLONE_THREAD or CLONE_PARENT were
    // specified" is `EINVAL`. A pid namespace's whole content is the number the
    // child is given, and a thread is not given one - it belongs to its
    // process - so the two halves of such a word contradict each other. Decided
    // here, before the privilege question: this is the request being
    // impossible, not the caller lacking a capability. (The rule is *documented*
    // rather than measured here: creating a pid namespace needs `CAP_SYS_ADMIN`,
    // which this sandbox does not have - `.workbuddy/probe_pidns.c` gets `EPERM`
    // - so a maintainer with the capability can upgrade this to an observation.)
    if asked & CLONE_NEWPID != 0 && flags & (CLONE_THREAD | CLONE_PARENT) != 0 {
        return Err(mpos_err::EINVAL);
    }
    // `CLONE_NEWUSER` is exempt from the capability gate for the same measured
    // reason it is exempt in `unshare`: making a user namespace needs no
    // capability, because it *is* the grant. And because the child is *born*
    // in that fresh namespace, holding the full set there, the *other* kinds in
    // the same word are checked against the new namespace too: measured,
    // `clone(CLONE_NEWUSER|CLONE_NEWUTS)` succeeds unprivileged while
    // `clone(CLONE_NEWUTS)` alone is `EPERM` (`.workbuddy/probe-userns10.c`).
    // So the gate applies only to a word that does *not* carry `CLONE_NEWUSER`.
    let gated = asked & !CLONE_NEWUSER;
    if asked & CLONE_NEWUSER == 0 && gated != 0 && !privileged {
        return Err(mpos_err::EPERM);
    }
    // `CLONE_NEWTIME` is deliberately **not** ruled on here, and the reason is
    // measured: its bit is `0x80`, entirely inside the `CSIGNAL` byte, so
    // `clone(2)` reads it as an exit signal and a caller cannot name it through
    // this call at all. `.workbuddy/probe-clone-timens.c` (host 7.2) shows
    // `clone(SIGCHLD|CLONE_NEWTIME)` = `0x91` answering `EINVAL` while
    // `clone(SIGCHLD|CLONE_NEWNET)` succeeds - and `clone(0x80)` *alone* answers
    // `EINVAL` too, which is what fixes the explanation as the signal number
    // rather than the namespace. `.workbuddy/probe-clone3-timens.c` agrees from
    // the other side: `clone3`, which carries the exit signal in its own field,
    // *creates* a task for `CLONE_NEWTIME` - so the flag is not refused as a
    // request, it is simply unexpressible here. This kernel reaches the same
    // `EINVAL` by the same route, from its own signal validation, so nothing is
    // owed to a caller: `unshare` is the one door, and it is the door Linux
    // documents (§6.3).
    // A *thread* with a namespace of its own is something Linux serves - its
    // `create_new_namespaces` runs for a thread too - and something this kernel
    // cannot: `sched::spawn_thread` copies the creating process's namespaces by
    // reference, so until now the flag was accepted by this verdict and then
    // dropped by the thread path. That is the shape §4's rule 4 is about (a
    // caller that asked for isolation, got none, and was told nothing), so it
    // is refused by name: `EOPNOTSUPP` - "present, cannot do that" - and *not*
    // `EINVAL`, because Linux does not call the request invalid.
    if flags & CLONE_THREAD != 0 && asked & SERVED != 0 {
        return Err(mpos_err::EOPNOTSUPP);
    }
    if asked & !SERVED != 0 {
        return Err(mpos_err::EOPNOTSUPP);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// A child's death, as the `CLD_*` vocabulary spells it
// ---------------------------------------------------------------------------

/// `si_code` for a child that exited normally, and one a signal ended.
///
/// The two halves of the `_sigchld` union `siginfo_t` carries - which is why
/// they live here rather than in either reader: a child's death is a *fact*
/// about the exit code, not about the door it is read through. The scheduler
/// queues it as the `SIGCHLD` record when the parent wants the signal
/// (`src/sched/runtime/`), and the syscall layer writes it into the
/// `siginfo_t` a collected `waitid(2)` hands over; both ask this.
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;

/// The rest of the vocabulary, for the readers that name an event class:
/// a task that stopped under its tracer, one a `SIGSTOP`-shaped signal stopped
/// for its parent, and one a `SIGCONT` resumed.
pub const CLD_TRAPPED: i32 = 4;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;

/// The `(si_code, si_status)` pair an exit code becomes: `CLD_EXITED` with the
/// code itself for a normal exit, `CLD_KILLED` with the signal number for a
/// death by signal - Linux's two answers for the two shapes a corpse has, and
/// the whole reason `si_status` is not simply the exit code.
///
/// **`CLD_DUMPED` (3) is the one code this cannot answer, and the measurement
/// says its reason is not the one this used to give.** A signalled death whose
/// default action dumps core is `CLD_DUMPED` on Linux - and it is `CLD_DUMPED`
/// even with the core limit at zero, which is what `.workbuddy/probe_signalfd_chld.c`
/// attached to this change measured on a current host (Linux 7.2.9,
/// `RLIMIT_CORE` set to 0 and read back as 0: a `SIGSEGV` death answered
/// `CLD_DUMPED` with `si_status` 11, and `wait4`'s status word carried
/// `WCOREDUMP`). So the bit follows the *signal*, not a file that was written;
/// "no core is written here" - the reason this kernel's comments used to give -
/// is not what Linux's answer is about. What is left is a named gap rather than
/// a silently different number, because reproducing it means modelling which
/// signals dump at all (`sig_kernel_coredump`'s set) and saying so in the
/// `wait4` status word and the `waitid` record together.
///
/// ```
/// use mpos_abi::{CLD_EXITED, CLD_KILLED, exit_chld};
///
/// assert_eq!(exit_chld(7), (CLD_EXITED, 7));
/// assert_eq!(exit_chld(-9), (CLD_KILLED, 9));
/// // The same number, two meanings: `si_status` is the exit code for an exit
/// // and the signal for a death, so `si_code` is the field to branch on.
/// assert_eq!(exit_chld(9).1, exit_chld(-9).1);
/// assert_ne!(exit_chld(9).0, exit_chld(-9).0);
/// ```
pub fn exit_chld(code: isize) -> (i32, i32) {
    if code < 0 {
        (CLD_KILLED, -code as i32)
    } else {
        (CLD_EXITED, code as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_flag_bits_are_the_asm_generic_values() {
        // The point of these tests is the two mistakes the header records:
        // `CLONE_NEWNS` was once spelled as `CLONE_THREAD` and `CLONE_NEWTIME`
        // as `CLONE_SETTLS`. Both are high-half vs low-half confusions, and the
        // numbers here are what Linux's `include/uapi/linux/sched.h` says,
        // measured against a running kernel's `clone` behaviour rather than the
        // man page (which does not print the values).
        assert_eq!(CLONE_VM, 0x0000_0100);
        assert_eq!(CLONE_FS, 0x0000_0200);
        assert_eq!(CLONE_FILES, 0x0000_0400);
        assert_eq!(CLONE_SIGHAND, 0x0000_0800);
        assert_eq!(CLONE_VFORK, 0x0000_4000);
        assert_eq!(CLONE_PARENT, 0x0000_8000);
        assert_eq!(CLONE_THREAD, 0x0001_0000);
        assert_eq!(CLONE_SYSVSEM, 0x0004_0000);
        assert_eq!(CLONE_SETTLS, 0x0008_0000);
        assert_eq!(CLONE_PARENT_SETTID, 0x0010_0000);
        assert_eq!(CLONE_CHILD_CLEARTID, 0x0020_0000);
        assert_eq!(CLONE_DETACHED, 0x0040_0000);
        assert_eq!(CLONE_UNTRACED, 0x0080_0000);
        assert_eq!(CLONE_CHILD_SETTID, 0x0100_0000);
    }

    #[test]
    fn namespace_bits_are_the_high_half_except_newtime() {
        // `CLONE_NEWTIME` sits at 0x80, inside the `CSIGNAL` byte - the one that
        // breaks the "namespaces are the high half" intuition, spelled wrong
        // once and pinned so.
        assert_eq!(CLONE_NEWTIME, 0x0000_0080);
        assert_eq!(CLONE_NEWNS, 0x0002_0000);
        assert_eq!(CLONE_NEWCGROUP, 0x0200_0000);
        assert_eq!(CLONE_NEWUTS, 0x0400_0000);
        assert_eq!(CLONE_NEWIPC, 0x0800_0000);
        assert_eq!(CLONE_NEWUSER, 0x1000_0000);
        assert_eq!(CLONE_NEWPID, 0x2000_0000);
        assert_eq!(CLONE_NEWNET, 0x4000_0000);
        assert_eq!(
            CLONE_NAMESPACES,
            CLONE_NEWTIME
                | CLONE_NEWNS
                | CLONE_NEWCGROUP
                | CLONE_NEWUTS
                | CLONE_NEWIPC
                | CLONE_NEWUSER
                | CLONE_NEWPID
                | CLONE_NEWNET
        );
    }

    #[test]
    fn the_clone_namespace_verdict_serves_its_kinds_and_refuses_whole() {
        // The rule the scheduler asks before it creates anything: the kinds whose
        // copies are wired into a child's construction are served, every other
        // kind is refused *whole* (`EOPNOTSUPP` - "present, cannot do that")
        // rather than half-served with a child that shares what its flags said it
        // got its own of, and a caller without `CAP_SYS_ADMIN` is refused `EPERM`
        // first, for any namespace word that does *not* carry `CLONE_NEWUSER`
        // (Linux's order: privilege, then what can be served).
        //
        // This pin exists because its absence was measured: the verdict landed
        // with no test of its own *and* behind a syscall-layer mask that read
        // every namespace bit as unknown, so it was code no word could reach and
        // nothing could notice (`t-clone`'s served/refused claims ran red).
        assert_eq!(clone_namespace_verdict(CLONE_NEWUTS, true), Ok(()));
        assert_eq!(clone_namespace_verdict(CLONE_NEWNS, true), Ok(()));
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUTS | CLONE_NEWNS, true),
            Ok(())
        );
        // A word asking for no namespace has nothing to decide, whatever the
        // caller holds - the byte `fork` is built from is not a namespace.
        assert_eq!(clone_namespace_verdict(0, true), Ok(()));
        assert_eq!(clone_namespace_verdict(0, false), Ok(()));
        // PID and NET are served (`sched::clone` gives the child a fresh
        // namespace when the flag is set), so the refused case moved to the
        // kinds this kernel still has no copy for - the drift a hardcoded served
        // set always has, recorded the same way in `t-caps` and `t-posix`.
        // Their unprivileged case is the lift loop below, with every other kind.
        assert_eq!(clone_namespace_verdict(CLONE_NEWPID, true), Ok(()));
        assert_eq!(clone_namespace_verdict(CLONE_NEWNET, true), Ok(()));
        // And the one word whose halves contradict each other: a thread cannot
        // be the init of a namespace its own process is not in, so this is
        // `EINVAL` - the request, not the privilege - even for a caller that
        // holds `CAP_SYS_ADMIN`.
        assert_eq!(
            clone_namespace_verdict(CLONE_THREAD | CLONE_NEWPID, true),
            Err(mpos_err::EINVAL)
        );
        // The other half of the sentence `clone(2)` writes: `CLONE_PARENT` with
        // a pid namespace is the same impossible request, and a thread-shaped
        // word has to carry `SIGHAND|VM` to get past the flag validation at all,
        // so the words below are the ones a caller would actually send.
        assert_eq!(
            clone_namespace_verdict(CLONE_PARENT | CLONE_NEWPID, true),
            Err(mpos_err::EINVAL)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_THREAD | CLONE_SIGHAND | CLONE_VM | CLONE_NEWPID, true),
            Err(mpos_err::EINVAL)
        );
        // A *thread* with a namespace of its own: Linux serves it, this kernel
        // cannot (`spawn_thread` copies the creating process's namespaces), so
        // the word is refused by name instead of being accepted and dropped -
        // which is what used to happen, silently.
        for kind in [CLONE_NEWNS, CLONE_NEWUTS, CLONE_NEWUSER] {
            assert_eq!(
                clone_namespace_verdict(CLONE_THREAD | CLONE_SIGHAND | CLONE_VM | kind, true),
                Err(mpos_err::EOPNOTSUPP),
                "{kind:#x} was accepted for a thread and then ignored"
            );
        }
        // USER is served now: `sched::clone` gives the child a fresh namespace to
        // be born in, and it needs no capability - the namespace *is* the grant -
        // so an unprivileged caller gets it too (`t-ns-user`'s child-side claims
        // pin the child's view on both machines).
        assert_eq!(clone_namespace_verdict(CLONE_NEWUSER, true), Ok(()));
        assert_eq!(clone_namespace_verdict(CLONE_NEWUSER, false), Ok(()));
        // The measured split the doc comment above describes: a word carrying
        // `CLONE_NEWUSER` has its other kinds checked against the fresh
        // namespace, so they are served to an unprivileged caller; the same kind
        // *alone* is gated on the caller's own `CAP_SYS_ADMIN`.
        // `.workbuddy/probe-userns10.c` is the observation.
        for kind in [CLONE_NEWUTS, CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWNET] {
            assert_eq!(
                clone_namespace_verdict(CLONE_NEWUSER | kind, false),
                Ok(()),
                "CLONE_NEWUSER should lift the gate for {kind:#x}"
            );
            assert_eq!(
                clone_namespace_verdict(kind, false),
                Err(mpos_err::EPERM),
                "{kind:#x} alone should still be gated"
            );
        }
        // Served *and* unserved in one word: nothing is half-done, so the whole
        // request is refused and the served kind is not quietly handed over.
        // `CLONE_NEWUSER` lifts the *gate*, not the served set: IPC beside it is
        // still `EOPNOTSUPP`.
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUTS | CLONE_NEWIPC, true),
            Err(mpos_err::EOPNOTSUPP)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUSER | CLONE_NEWIPC, false),
            Err(mpos_err::EOPNOTSUPP)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWNS, false),
            Err(mpos_err::EPERM)
        );
    }

    #[test]
    fn ptrace_protocol_words_are_the_generic_values() {
        // Same reason as the clone bits above: the numbers are what a tracer's
        // `switch` is written against, and a wrong one is a stop read as the
        // wrong kind. They are here rather than in the x86_64 column because
        // `linux/ptrace.h` is architecture-independent.
        assert_eq!(PTRACE_O_TRACESYSGOOD, 1);
        assert_eq!(PTRACE_O_TRACEEXEC, 0x10);
        assert_eq!(PTRACE_O_EXITKILL, 0x0010_0000);
        assert_eq!(PTRACE_EVENT_NONE, 0);
        assert_eq!(PTRACE_EVENT_FORK, 1);
        assert_eq!(PTRACE_EVENT_VFORK, 2);
        assert_eq!(PTRACE_EVENT_CLONE, 3);
        assert_eq!(PTRACE_EVENT_EXEC, 4);
        assert_eq!(unwatched_events::VFORK_DONE, 5);
        assert_eq!(unwatched_events::EXIT, 6);
        assert_eq!(SYSCALL_TRAP, 0x80);
    }

    #[test]
    fn default_rlimits_budgets_stack_nofile_and_nproc() {
        let t = default_rlimits();
        assert_eq!(t.len(), RLIM_NLIMITS);
        // The three rows that are finite are the ones a static musl binary
        // needs to start; everything else reads infinity. Which three they are
        // is the point: `asm-generic/resource.h`'s numbers, so that a caller
        // asking for RLIMIT_NOFILE gets the descriptor ceiling.
        assert_eq!(
            t[3],
            Rlimit {
                cur: 8 * 1024 * 1024,
                max: 8 * 1024 * 1024
            }
        );
        assert_eq!(
            t[6],
            Rlimit {
                cur: 4096,
                max: 4096
            }
        );
        assert_eq!(
            t[7],
            Rlimit {
                cur: 1024,
                max: 1_048_576
            }
        );
        // `RLIMIT_RSS` (5) is the row the descriptor ceiling used to sit on, and
        // nothing budgets it - the assertion that keeps the two from swapping
        // back.
        assert_eq!(
            t[5],
            Rlimit {
                cur: RLIM_INFINITY,
                max: RLIM_INFINITY
            }
        );
        assert_eq!(
            t[0],
            Rlimit {
                cur: RLIM_INFINITY,
                max: RLIM_INFINITY
            }
        );
    }

    #[test]
    fn a_childs_death_is_the_cld_pair_linux_reports() {
        // Measured with `.workbuddy/probe_signalfd_chld.c` on a current host:
        // a child that exits 7 is `CLD_EXITED` with `si_status` 7 through the
        // signalfd, through a handler's `siginfo_t` and through `sigtimedwait`
        // alike, and a child killed by `SIGKILL` is `CLD_KILLED` with
        // `si_status` 9.
        assert_eq!(exit_chld(7), (CLD_EXITED, 7));
        assert_eq!(exit_chld(-9), (CLD_KILLED, 9));
        // The same number, two meanings: `si_status` is the exit code for an
        // exit and the *signal* for a death, so a caller that reads the status
        // alone cannot tell "exited 9" from "killed by 9" - which is the whole
        // reason `si_code` is the field to branch on.
        assert_eq!(exit_chld(9).1, exit_chld(-9).1);
        assert_ne!(exit_chld(9).0, exit_chld(-9).0);
        assert_eq!((CLD_EXITED, CLD_KILLED), (1, 2));
    }

    #[test]
    fn setrlimit_admits_by_the_soft_hard_and_root_rule() {
        let hard_infinity = Rlimit {
            cur: 100,
            max: RLIM_INFINITY,
        };
        // A soft above the hard is refused, whatever the caller's privilege.
        assert_eq!(
            rlimit_admit(hard_infinity, Rlimit { cur: 200, max: 100 }, true),
            Err(mpos_err::EINVAL)
        );
        // Lowering both within the standing hard is accepted.
        assert_eq!(
            rlimit_admit(hard_infinity, Rlimit { cur: 50, max: 60 }, false),
            Ok(Rlimit { cur: 50, max: 60 })
        );
        // Raising the hard ceiling past where it stands is root's alone.
        let low = Rlimit { cur: 10, max: 20 };
        assert_eq!(
            rlimit_admit(low, Rlimit { cur: 10, max: 30 }, false),
            Err(mpos_err::EPERM)
        );
        assert_eq!(
            rlimit_admit(low, Rlimit { cur: 10, max: 30 }, true),
            Ok(Rlimit { cur: 10, max: 30 })
        );
    }
}
