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
/// `CAP_SYS_ADMIN` checked there too); this kernel serves the two kinds whose
/// copies are wired into the child's construction — the mount table (the child
/// starts in a copy; `t-ns-mnt`) and the names (Linux's `copy_utsname` on
/// exactly this flag; `t-clone` pins it) — and refuses every other kind whole.
/// A caller asking for PID isolation gets the same answer from here as
/// `unshare` gives it: nothing to hand over yet.
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
pub fn clone_namespace_verdict(flags: usize, privileged: bool) -> Result<(), isize> {
    const SERVED: usize = CLONE_NEWUTS | CLONE_NEWNS;
    let asked = flags & CLONE_NAMESPACES;
    if asked != 0 && !privileged {
        return Err(mpos_err::EPERM);
    }
    if asked & !SERVED != 0 {
        return Err(mpos_err::EOPNOTSUPP);
    }
    Ok(())
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
    fn the_clone_namespace_verdict_serves_two_kinds_and_refuses_whole() {
        // The rule the scheduler asks before it creates anything: the two kinds
        // whose copies are wired into a child's construction are served, every
        // other kind is refused *whole* (`EOPNOTSUPP` - "present, cannot do
        // that") rather than half-served with a child that shares what its flags
        // said it got its own of, and a caller without `CAP_SYS_ADMIN` is
        // refused `EPERM` first, for *any* namespace word (Linux's order:
        // privilege, then what can be served).
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
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWPID, true),
            Err(mpos_err::EOPNOTSUPP)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWNET, true),
            Err(mpos_err::EOPNOTSUPP)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUSER, true),
            Err(mpos_err::EOPNOTSUPP)
        );
        // Served *and* unserved in one word: nothing is half-done, so the whole
        // request is refused and the served kind is not quietly handed over.
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWUTS | CLONE_NEWNET, true),
            Err(mpos_err::EOPNOTSUPP)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWNS, false),
            Err(mpos_err::EPERM)
        );
        assert_eq!(
            clone_namespace_verdict(CLONE_NEWPID, false),
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
