#![no_std]

//! The arch-neutral Linux ABI facts: the `clone(2)` flag bits, a process's
//! default umask, and the resource-limit model.
//!
//! Why a leaf and not the syscall layer. `sched` reads every one of these on a
//! path that has nothing to do with dispatching a syscall - it builds a task's
//! initial umask, budgets its rlimits, and decodes a `clone` flag word to decide
//! what a child shares. Naming `crate::syscall` for those values is what made
//! `sched` unbuildable without the x86_64 ABI layer (`PLAN.md` §11.2, the
//! `sched -> syscall` row). The values themselves are shared across
//! architectures - these `CLONE_*` bits and `RLIMIT_*` rows carry the same number
//! on x86_64 and aarch64, because both use the asm-generic definitions - so they
//! belong below the machine-specific ABI, in a crate that a scheduler on any
//! machine may name. What deliberately stays in `syscall` is the part that is not
//! arch-neutral: the syscall *numbers* (`nr.rs`'s non-`CLONE` table), the
//! x86_64 `user_regs_struct` field count, and the per-arch `MMAP_BASE`, which
//! follows each architecture's address-space layout and is decided with the
//! aarch64 process-running stage (`PLAN.md` §9.1's stage 6/7).
//!
//! This is the single source for each fact. `syscall::linux::nr` and
//! `syscall::linux::abi` re-export these rather than redefining them, so the
//! `unshare`/`clone` handlers and the `sched` core read one table, not two.

/// The mode bits a fresh process masks creation with: `0o022`, the `umask(2)`
/// default every shell and init inherits. `sched` seeds a task's umask with this
/// and the syscall layer reads it back for `umask(2)`'s first answer.
pub const DEFAULT_UMASK: u32 = 0o022;

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

/// How many resource limits a process carries. The rows are indexed by the
/// `RLIMIT_*` numbers, which are the same on x86_64 and aarch64 (asm-generic):
/// `3` stack, `5` descriptor table, `7` process count.
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
pub fn default_rlimits() -> [Rlimit; RLIM_NLIMITS] {
    let mut t = [Rlimit {
        cur: RLIM_INFINITY,
        max: RLIM_INFINITY,
    }; RLIM_NLIMITS];
    t[3] = Rlimit {
        cur: 8 * 1024 * 1024,
        max: 8 * 1024 * 1024,
    }; // RLIMIT_STACK
    t[5] = Rlimit {
        cur: 1024,
        max: 1_048_576,
    }; // RLIMIT_NOFILE
    t[7] = Rlimit {
        cur: 4096,
        max: 4096,
    }; // RLIMIT_NPROC
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
    fn default_rlimits_budgets_stack_nofile_and_nproc() {
        let t = default_rlimits();
        assert_eq!(t.len(), RLIM_NLIMITS);
        // The three rows that are finite are the ones a static musl binary
        // needs to start; everything else reads infinity.
        assert_eq!(
            t[3],
            Rlimit {
                cur: 8 * 1024 * 1024,
                max: 8 * 1024 * 1024
            }
        );
        assert_eq!(
            t[5],
            Rlimit {
                cur: 1024,
                max: 1_048_576
            }
        );
        assert_eq!(
            t[7],
            Rlimit {
                cur: 4096,
                max: 4096
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
