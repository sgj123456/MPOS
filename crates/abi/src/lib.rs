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

// The three bits `clone(2)` cannot carry at all: each asks for something the
// legacy call has no room to describe, so each is a `clone3(2)` flag. They live
// beside `clone`'s word because both handlers decode one vocabulary
// (`syscall::linux::abi::clone3_verdict` reads them with the rest) and the last
// two are above bit 31, which is exactly why `clone`'s 32-bit flag word could
// never have spelled them.
//
// `CLONE_PIDFD` is the bit this kernel serves for real: the child is made and
// the descriptor that names it is delivered in the same call, so a caller never
// has a window in which the pid it was told could have been reused. The other
// two are named refusals rather than silent no-ops - see `clone3_verdict`.
/// Hand the caller a pidfd for the new child, written through the argument
/// struct's `pidfd` field. Measured on Linux 7.2.11
/// (`.workbuddy/probe_clone3*.c`): the field is required (`pidfd = 0` is
/// `EFAULT`, because the write is attempted), and combining the bit with
/// `CLONE_PARENT_SETTID` is `EINVAL`.
pub const CLONE_PIDFD: usize = 0x0000_1000;
/// The child starts with every signal disposition reset to `SIG_DFL`, the mask
/// untouched. This kernel serves it (`CloneRequest::clear_sighand`).
pub const CLONE_CLEAR_SIGHAND: usize = 0x0001_0000_0000;
/// Put the child in a cgroup named by a descriptor in the argument struct.
/// This kernel has no cgroup tree, so no descriptor can name one: the refusal
/// is the `EBADF` Linux answers for an fd that is not a cgroup.
pub const CLONE_INTO_CGROUP: usize = 0x0002_0000_0000;

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
/// `RLIMIT_SIGPENDING` (11 in `asm-generic/resource.h`): how many queued
/// real-time signal records one **user** may hold. It is the row the two
/// `sigqueue` doors are held to — `sigqueue`/`rt_sigqueueinfo` answer `EAGAIN`
/// at it and `kill`/`tgkill` drop the copy and answer 0, which is Linux's
/// measured split (`docs/notes/` §2.3).
pub const RLIMIT_SIGPENDING: usize = 11;
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
/// ceiling, a process count, a queued-signal ceiling, and infinity for the
/// rows nothing budgets. Every other resource reads these until `setrlimit`
/// says otherwise.
///
/// ```
/// use mpos_abi::{Rlimit, RLIM_INFINITY, default_rlimits};
///
/// let t = default_rlimits();
/// // The four budgeted rows sit at `asm-generic/resource.h`'s numbers:
/// // RLIMIT_STACK (3), RLIMIT_NPROC (6), RLIMIT_NOFILE (7) and
/// // RLIMIT_SIGPENDING (11) - so a caller asking for the descriptor ceiling
/// // reads the descriptor ceiling.
/// assert_eq!((t[3].cur, t[3].max), (8 * 1024 * 1024, 8 * 1024 * 1024));
/// assert_eq!((t[6].cur, t[6].max), (4096, 4096));
/// assert_eq!((t[7].cur, t[7].max), (1024, 1_048_576));
/// assert_eq!((t[11].cur, t[11].max), (16, 16));
/// // And the row the descriptor ceiling used to sit on (RLIMIT_RSS, 5)
/// // reads infinity - the two must never swap back.
/// assert_eq!(t[5], Rlimit { cur: RLIM_INFINITY, max: RLIM_INFINITY });
/// ```
///
/// The four rows are `RLIMIT_STACK` (3), `RLIMIT_NPROC` (6), `RLIMIT_NOFILE`
/// (7) and `RLIMIT_SIGPENDING` (11) - the numbers `asm-generic/resource.h`
/// gives, and so the ones a caller's `getrlimit`/`setrlimit`/`prlimit64`
/// speaks. They were written at `5` and `7` (Linux's `RLIMIT_RSS` and
/// `RLIMIT_NPROC` rows), which made a caller asking for the descriptor ceiling
/// read the process count and left the real `RLIMIT_NOFILE` row holding it.
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
    // RLIMIT_MSGQUEUE (12): the bytes one user's POSIX message queues may
    // reserve. Linux's own default is 819200, and it is *not* left at infinity
    // here for the reason §8.0 gives: a queue reserves its whole maximal size at
    // creation (`mq_msgsize * mq_maxmsg`), so an unlimited row would let a
    // program reserve memory the allocator cannot refuse - this row is the bound
    // the family is held to, and `mq_open` answers `EMFILE` at it (measured:
    // with the limit at 0, creation is `EMFILE`).
    t[12] = Rlimit {
        cur: 819_200,
        max: 819_200,
    }; // RLIMIT_MSGQUEUE
    // RLIMIT_SIGPENDING (11): the queued real-time records one user may hold.
    // It is *not* infinity, because the row is a real bound here: the records
    // live in a fixed per-task ring and `RT_QUEUE_LEN` is its size, so this is
    // what the kernel can actually hold. Linux's own default is policy
    // (`max_threads / 2`, ~125k) rather than ABI; a caller that reads this row
    // and queues that many is told the truth. `RT_QUEUE_LEN` cannot be named
    // from this leaf crate, so the number is spelled here and a host test in
    // the kernel (`rlimits_report_the_startup_defaults_and_refuse_unknown_rows`)
    // pins the two together.
    t[11] = Rlimit { cur: 16, max: 16 }; // RLIMIT_SIGPENDING
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
    const SERVED: usize = CLONE_NEWUTS
        | CLONE_NEWNS
        | CLONE_NEWPID
        | CLONE_NEWNET
        | CLONE_NEWUSER
        | CLONE_NEWIPC
        | CLONE_NEWCGROUP;
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
    // `CLONE_NEWTIME` is deliberately **not** in `SERVED`, and the two doors
    // reach the refusal by different routes. Through `clone(2)` a caller cannot
    // name it at all: its bit is `0x80`, entirely inside the `CSIGNAL` byte, so
    // the call reads it as an exit signal and this kernel's own signal
    // validation answers `EINVAL` - measured, `.workbuddy/probe-clone-timens.c`
    // (`clone(SIGCHLD|CLONE_NEWTIME)` = `0x91` is `EINVAL` while
    // `clone(SIGCHLD|CLONE_NEWNET)` succeeds, and `clone(0x80)` alone is
    // `EINVAL` too, which fixes the explanation as the signal number rather
    // than the namespace). Through `clone3(2)` - which carries the exit signal
    // in a field of its own and so *can* spell the bit - the flag arrives and
    // is refused here: Linux creates a task for it (`.workbuddy/probe-clone3-
    // timens.c`), and this kernel has no way to give the child a clock
    // namespace of its own through `clone`, so it is `EOPNOTSUPP` for a
    // privileged caller and `EPERM` for one without `CAP_SYS_ADMIN` - the
    // unserved-kind and capability rules below, in their own order. `unshare`
    // is the door that serves the clocks (§6.3).
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

/// A death the kernel's coredump path took - `CLD_DUMPED` (3).
///
/// It is a *third* shape beside `CLD_EXITED` and `CLD_KILLED`, and Linux picks
/// it from the signal alone, not from a core file existing: the two below are
/// the same death, told apart by what the signal's default action is.
pub const CLD_DUMPED: i32 = 3;

/// The rest of the vocabulary, for the readers that name an event class:
/// a task that stopped under its tracer, one a `SIGSTOP`-shaped signal stopped
/// for its parent, and one a `SIGCONT` resumed.
pub const CLD_TRAPPED: i32 = 4;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;

/// Does a death **by this signal** have a core-dumping default action - the
/// half of `CLD_DUMPED`/`WCOREDUMP` that the *signal* decides?
///
/// The other half is the dying process's own `dumpable` flag, so this predicate
/// alone does not answer the question a death record does; it is one input to
/// [`exit_chld`]'s `dumped`, which the death path computes from this *and* the
/// child's `dumpable` (see `.workbuddy/probe_wcore_dumpable.c` beside
/// [`exit_chld`]).
///
/// This is Linux's `sig_kernel_coredump()` set, and the ten signals below are
/// the number measured rather than the man page's summary. Two probes back it
/// (`.workbuddy/probe_signalfd_chld.c`, `.workbuddy/probe_coredump_signals.c`,
/// Linux 7.2.9), and `.workbuddy/probe_coredump_signals.c` walks *every* signal
/// whose default action terminates a task and reads back both the status word's
/// `WCOREDUMP` bit and `waitid`'s `si_code`:
///
///   * `CLD_DUMPED` + `WCOREDUMP` for exactly **`SIGQUIT`, `SIGILL`,
///     `SIGTRAP`, `SIGABRT`, `SIGBUS`, `SIGFPE`, `SIGSEGV`, `SIGXCPU`,
///     `SIGXFSZ`, `SIGSYS`**;
///   * everything else that kills - `SIGHUP`, `SIGINT`, `SIGKILL`, `SIGUSR1`,
///     `SIGUSR2`, `SIGPIPE`, `SIGALRM`, `SIGTERM`, `SIGSTKFLT`, `SIGVTALRM`,
///     `SIGPROF`, `SIGIO`, `SIGPWR` - is `CLD_KILLED` with the bit clear.
///
/// The bit is **not** about a core file being written, which is what this
/// kernel's own comments used to say: the same probe runs the whole set with
/// `RLIMIT_CORE` at 0 (read back as 0) and `SIGSEGV` still answers `CLD_DUMPED`
/// - so it is the signal's default action, and the "no core is written here"
/// reason this tree gave was answering a different question. The stop-class,
/// continue-class and ignore-class signals have no corpse to classify and are
/// deliberately absent from both sets.
pub fn signal_dumps_core(sig: i32) -> bool {
    matches!(
        sig,
        SIGQUIT
            | SIGILL
            | SIGTRAP
            | SIGABRT
            | SIGBUS
            | SIGFPE
            | SIGSEGV
            | SIGXCPU
            | SIGXFSZ
            | SIGSYS
    )
}

/// The `(si_code, si_status)` pair an exit code becomes: `CLD_EXITED` with the
/// code itself for a normal exit, and for a death by signal - the code negated -
/// `CLD_DUMPED` when `dumped` says the kernel's coredump path classified it,
/// `CLD_KILLED` otherwise, both carrying the signal number as `si_status`.
/// Linux's three answers for the shapes a corpse has, and the whole reason
/// `si_status` is not simply the exit code.
///
/// **`dumped` is a parameter rather than a fact read from `code`, because it is
/// not a fact about the signal alone.** Two things make it true, and only the
/// first lives here: the signal's default action must dump core
/// ([`signal_dumps_core`]'s set, measured there), **and** the dying process's own
/// `dumpable` flag must be non-zero. `.workbuddy/probe_wcore_dumpable.c`
/// (Linux 7.2.9, `RLIMIT_CORE` 0) measures the second half: the same `SIGSEGV`
/// death is `CLD_DUMPED` at `dumpable` 1 and `CLD_KILLED` at 0 - a
/// `PR_SET_DUMPABLE 0` makes `do_coredump()` bail at its first check, before it
/// can classify anything - and the flag is the *child's*, so a child that sets
/// its own back to 1 reports `CLD_DUMPED` under a non-dumpable parent. (The
/// kernel's own `SUID_DUMP_ROOT` value, 2, is not settable through
/// `PR_SET_DUMPABLE` - Linux answers `EINVAL` - so the gate's only observable
/// pair is 1 versus 0; the predicate reads `!= 0`, which is what `__get_dumpable`
/// is compared against.)
///
/// The death path supplies `dumped`; a `wait4` reader and a `waitid` reader are
/// handed the same value for one death, so they cannot disagree about it.
///
/// ```
/// use mpos_abi::{CLD_DUMPED, CLD_EXITED, CLD_KILLED, exit_chld};
///
/// assert_eq!(exit_chld(7, false), (CLD_EXITED, 7));
/// assert_eq!(exit_chld(-9, false), (CLD_KILLED, 9));
/// // A core-dumping signal, for a dumpable process, is the third shape, not a
/// // fourth: `SIGSEGV` is `CLD_DUMPED` where `SIGKILL` is `CLD_KILLED`.
/// assert_eq!(exit_chld(-11, true), (CLD_DUMPED, 11));
/// // The same signal for a process that turned its dumpable off is not.
/// assert_eq!(exit_chld(-11, false), (CLD_KILLED, 11));
/// // The same number, two meanings: `si_status` is the exit code for an exit
/// // and the signal for a death, so `si_code` is the field to branch on.
/// assert_eq!(exit_chld(9, false).1, exit_chld(-9, false).1);
/// assert_ne!(exit_chld(9, false).0, exit_chld(-9, false).0);
/// ```
pub fn exit_chld(code: isize, dumped: bool) -> (i32, i32) {
    if code < 0 {
        let sig = -code as i32;
        let class = if dumped { CLD_DUMPED } else { CLD_KILLED };
        (class, sig)
    } else {
        (CLD_EXITED, code as i32)
    }
}

// ---------------------------------------------------------------------------
// CPU time, in the unit a caller reads it in
// ---------------------------------------------------------------------------

/// `USER_HZ`: the clock-tick rate the Linux userspace ABI speaks, whatever the
/// kernel's own timer runs at. musl hardcodes `100` and hands it to
/// `sysconf(_SC_CLK_TCK)`, and both machines this kernel serves use the generic
/// `100`, so a caller converting a tick count to seconds divides by exactly
/// this - `times(2)`, a `struct rusage` and a `SIGCHLD` record's
/// `si_utime`/`si_stime` are one unit.
pub const USER_HZ: u64 = 100;

/// Convert this kernel's internal tick count into the `USER_HZ` ticks a caller
/// reads: `ticks * USER_HZ / tick_hz`, **truncated**, one division.
///
/// Linux's own conversion is `nsec_to_clock_t` - `x * USER_HZ / NSEC_PER_SEC`,
/// truncated - and this is the same division one unit up: the machine's timer
/// runs at `tick_hz` (the PIT's `250`, the generic timer's own rate) rather than
/// at nanoseconds, and the answer a caller must get back is the same one. Every
/// field that reports CPU time goes through here, so a `times(2)` total and a
/// `SIGCHLD` record's `si_utime` cannot come to different units.
///
/// The `u128` intermediate is deliberate rather than decorative: a long uptime's
/// tick count times `USER_HZ` would wrap a `u64` for no reason, and the cost is
/// a single wide multiply. `tick_hz` is a caller-supplied machine constant
/// rather than a value read here, because this crate is a leaf and names no
/// machine (`AGENTS.md` §3's downward-only rule) - the kernel passes
/// `crate::hal::TICK_HZ`, which is never zero.
///
/// ```
/// use mpos_abi::{USER_HZ, clock_ticks_to_user_hz};
///
/// // 250 Hz internal ticks: 250 ticks is one second, 100 USER_HZ ticks.
/// assert_eq!(clock_ticks_to_user_hz(250, 250), USER_HZ);
/// // The division truncates, as Linux's does - not rounds.
/// assert_eq!(clock_ticks_to_user_hz(2, 250), 0);
/// assert_eq!(clock_ticks_to_user_hz(3, 250), 1);
/// assert_eq!(clock_ticks_to_user_hz(0, 250), 0);
/// ```
pub fn clock_ticks_to_user_hz(ticks: u64, tick_hz: u64) -> u64 {
    (ticks as u128 * USER_HZ as u128 / tick_hz as u128) as u64
}

/// The signal numbers [`signal_dumps_core`] names, taken from the Linux
/// `signal.h` table. Named here rather than in the predicate so a reader can
/// see the set and a test can hold it to that set.
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGABRT: i32 = 6;
pub const SIGBUS: i32 = 7;
pub const SIGFPE: i32 = 8;
pub const SIGSEGV: i32 = 11;
pub const SIGXCPU: i32 = 24;
pub const SIGXFSZ: i32 = 25;
pub const SIGSYS: i32 = 31;

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
        // The three `clone3(2)` bits, measured on Linux 7.2.11 by calling
        // `clone3` with each alone (`.workbuddy/probe_clone3d.c`).
        assert_eq!(CLONE_PIDFD, 0x0000_1000);
        assert_eq!(CLONE_CLEAR_SIGHAND, 0x0001_0000_0000);
        assert_eq!(CLONE_INTO_CGROUP, 0x0002_0000_0000);
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
        // IPC is served too now: `clone(CLONE_NEWIPC)` moves the *child* into a
        // fresh namespace and leaves the parent's alone (measured,
        // `.workbuddy/probe_clone_ipc.c`, and `t-ns-ipc` pins it), so the word
        // that used to be the unserved witness here is a legal pair.
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUTS | CLONE_NEWIPC, true),
            Ok(())
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUSER | CLONE_NEWIPC, false),
            Ok(()),
            "an unprivileged word carrying CLONE_NEWUSER lifts the gate for IPC too"
        );
        // The mixing story ended where the ladder did: CGROUP was the last
        // unserved witness these pins had, and it landed with the tree it
        // views (2026-10-25) - the pair that used to be refused whole is a
        // legal word now, and the unprivileged USER pair rides the same
        // lift it always did.
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWIPC | CLONE_NEWCGROUP, true),
            Ok(())
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUSER | CLONE_NEWCGROUP, false),
            Ok(()),
            "an unprivileged word carrying CLONE_NEWUSER lifts the gate for CGROUP too"
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
    fn default_rlimits_budgets_stack_nofile_nproc_and_sigpending() {
        let t = default_rlimits();
        assert_eq!(t.len(), RLIM_NLIMITS);
        // The four rows that are finite are the ones a static musl binary needs
        // to start, plus the queued-signal bound; everything else reads
        // infinity. Which four they are is the point:
        // `asm-generic/resource.h`'s numbers, so that a caller asking for
        // RLIMIT_NOFILE gets the descriptor ceiling.
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
        // `RLIMIT_SIGPENDING` (11) is finite on purpose: it is where the queued
        // real-time records are actually held, so a caller that reads it and
        // queues that many is told the truth. The kernel's host test
        // `rlimits_report_the_startup_defaults_and_refuse_unknown_rows` pins
        // the number to `sched::signal::RT_QUEUE_LEN`, which this leaf cannot
        // name.
        assert_eq!(t[RLIMIT_SIGPENDING], Rlimit { cur: 16, max: 16 });
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
        assert_eq!(exit_chld(7, false), (CLD_EXITED, 7));
        assert_eq!(exit_chld(-9, false), (CLD_KILLED, 9));
        // The same number, two meanings: `si_status` is the exit code for an
        // exit and the *signal* for a death, so a caller that reads the status
        // alone cannot tell "exited 9" from "killed by 9" - which is the whole
        // reason `si_code` is the field to branch on.
        assert_eq!(exit_chld(9, false).1, exit_chld(-9, false).1);
        assert_ne!(exit_chld(9, false).0, exit_chld(-9, false).0);
        assert_eq!((CLD_EXITED, CLD_KILLED), (1, 2));
    }

    #[test]
    fn a_core_dumping_signal_is_cld_dumped_not_cld_killed() {
        // The exact set, measured with `.workbuddy/probe_coredump_signals.c` on
        // a current host (Linux 7.2.9) - every signal whose default action ends
        // a task, walked one at a time through both `wait4`'s `WCOREDUMP` bit
        // and `waitid`'s `si_code`, RLIMIT_CORE at 0 throughout. The table below
        // is the whole standard set, so a signal wrongly added to (or dropped
        // from) `signal_dumps_core` fails here rather than in the sweep.
        let core = [
            SIGQUIT, SIGILL, SIGTRAP, SIGABRT, SIGBUS, SIGFPE, SIGSEGV, SIGXCPU, SIGXFSZ, SIGSYS,
        ];
        for sig in 1i32..=31 {
            let dumps = core.contains(&sig);
            assert_eq!(signal_dumps_core(sig), dumps, "signal {sig}");
            // The signal half alone classifies the death *when the process is
            // dumpable*; the dumpable half is the scheduler's to add, and is
            // pinned by `t-wait4`'s `PR_SET_DUMPABLE 0` case.
            assert_eq!(
                exit_chld(-(sig as isize), dumps),
                (if dumps { CLD_DUMPED } else { CLD_KILLED }, sig),
                "signal {sig}'s death record"
            );
        }
        // The dumpable gate: the *same* signal is `CLD_KILLED`, not
        // `CLD_DUMPED`, when the death path says the process was not dumpable.
        assert_eq!(exit_chld(-SIGSEGV as isize, false), (CLD_KILLED, SIGSEGV));
        assert_eq!(exit_chld(-SIGSEGV as isize, true), (CLD_DUMPED, SIGSEGV));
        // A real-time signal has no dump class of its own.
        assert!(!signal_dumps_core(34));
        // An exit never becomes a dump, whatever its code - and `dumped` is
        // ignored for one.
        assert_eq!(exit_chld(3, false), (CLD_EXITED, 3));
        assert_eq!(exit_chld(3, true), (CLD_EXITED, 3));
        assert_ne!(exit_chld(3, false).0, CLD_DUMPED);
        assert_eq!(CLD_DUMPED, 3);
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

    #[test]
    fn a_tick_count_converts_to_the_unit_a_caller_reads() {
        // The one conversion every CPU-time field goes through, held to Linux's
        // `nsec_to_clock_t` shape one unit up: the machine's timer rate in,
        // `USER_HZ` out, truncated. `100` is what musl hardcodes and what both
        // machines' `sysconf(_SC_CLK_TCK)` answers, so a caller dividing by it
        // must get the same unit back.
        assert_eq!(USER_HZ, 100);
        assert_eq!(clock_ticks_to_user_hz(250, 250), 100); // 250 Hz: 1 s = 100
        assert_eq!(clock_ticks_to_user_hz(500, 250), 200);
        assert_eq!(clock_ticks_to_user_hz(0, 250), 0);
        // Truncation, not rounding: 3/250 s is 1.2 USER_HZ ticks -> 1, and
        // 2/250 s is 0.8 -> 0. A `round` here is the bug this pins.
        assert_eq!(clock_ticks_to_user_hz(3, 250), 1);
        assert_eq!(clock_ticks_to_user_hz(2, 250), 0);
        // The conversion is the *machine's* rate, not a fixed multiply: a
        // 1000 Hz timer answers the same seconds from a different count.
        assert_eq!(clock_ticks_to_user_hz(1000, 1000), 100);
        assert_eq!(clock_ticks_to_user_hz(1, 1000), 0);
        // No wrap on a long uptime: a tick count whose `* USER_HZ` overflows a
        // `u64` still converts, which is the `u128` intermediate's whole job.
        assert_eq!(
            clock_ticks_to_user_hz(u64::MAX, 250),
            (u64::MAX as u128 * 100 / 250) as u64
        );
    }
}
