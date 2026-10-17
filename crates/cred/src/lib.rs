#![no_std]

//! The process credential model: capability sets, the dumpable flag and
//! `KEEPCAPS` - the state `capget`/`capset` and four `prctl` options read and
//! write, and the two consumers that make it real.
//!
//! Why this exists. `prctl`'s `PR_SET_DUMPABLE`, `PR_SET_KEEPCAPS` and
//! `PR_CAPBSET_*` used to be refused with `EINVAL`, and that refusal was
//! honest only because this kernel had nothing for them to describe: `capget`
//! reported every capability effective and permitted for every task, `capset`
//! stored nothing, and no path in the tree asked a capability question at all.
//! Storing a value nothing consults is a shape that looks finished while
//! doing less, so the options stayed refused (`docs/uapi/` §5.3 names the
//! prerequisite). This module is that prerequisite.
//!
//! What is real here, and what is not. The four sets, the dumpable tri-state
//! and the `KEEPCAPS` bit are stored per thread group and are *read*: the
//! effective set answers `ptrace_may_access`, the bounding set is applied at
//! `execve`, a change of the uid/gid identity re-derives the sets the way
//! Linux measures (below), and `capget`/`capset` round-trip all of it. What is
//! deliberately absent is file capabilities: this kernel has no `security.capability`
//! xattr and does not read one, so an `execve` can only ever *restrict* the
//! sets, never grant from a file. That is a named divergence, and it is the
//! conservative direction - a capability a Linux kernel would hand a binary
//! here is simply not held.
//!
//! Every rule below was measured on this host's Linux (7.2.7, 2026-10-01)
//! rather than taken from a man page, and the probes were:
//!
//! ```text
//! exec after dropping a bounding cap   CapPrm/CapEff became the bounding set
//! root, capset cut CAP_CHOWN, exec     got CAP_CHOWN back (pP' = bounding)
//! setresuid(0,65534,0)  (suid stays 0) CapPrm kept, CapEff cleared
//! setresuid(65534,65534,65534)         CapPrm and CapEff both cleared
//! the same, with PR_SET_KEEPCAPS(1)    CapPrm kept, CapEff cleared
//! PR_SET_DUMPABLE(0|1); (2|3|-1)       accepted; EINVAL
//! PR_CAPBSET_READ(0..=40); (41|100)    1 or 0; EINVAL
//! PR_CAPBSET_DROP(100), unprivileged    EPERM - the capability is asked first
//! entering a user namespace (7.2.9)    CapEff/CapPrm/cap_bset full, CapInh 0
//! ```
//!
//! The dumpable flag is reset to `SUID_DUMP_DISABLE` when the identity moves,
//! which is Linux's `set_dumpable(mm, suid_dumpable)`: the sysctl behind it
//! defaults to 0, and the machine this was measured on had it set to 2, which
//! is the one place these measurements disagree with the default this kernel
//! implements. `NOTES.md` records the sysctl as the gap.

/// The last capability number x86_64 Linux names. `CAP_BPF` is 39 and
/// `CAP_CHECKPOINT_RESTORE` is 40, which is where the kernel's own
/// `CAP_LAST_CAP` stops.
pub const CAP_LAST_CAP: u32 = 40;

/// Every capability this kernel's model can hold, as a bitmask.
pub const CAP_FULL: u64 = if CAP_LAST_CAP == 63 {
    u64::MAX
} else {
    (1u64 << (CAP_LAST_CAP + 1)) - 1
};

/// The number of the capability `ptrace_may_access` tests for.
pub const CAP_SYS_PTRACE: u32 = 19;
/// `CAP_DAC_OVERRIDE`, which the permission checks offer the triads they
/// refuse - and which grants a **read or write on a non-directory whatever the
/// bits are**, the clause that once required some triad to carry one having
/// gone from Linux (measured, `.workbuddy/probe_capable_scope.sh dac`).
pub const CAP_DAC_OVERRIDE: u32 = 1;
/// `CAP_DAC_READ_SEARCH`, the read-and-search half of the same override: it
/// grants a read on its own, and everything on a directory but a write.
pub const CAP_DAC_READ_SEARCH: u32 = 2;
/// `CAP_FOWNER`, which the sticky-bit rule asks
/// (`capable_wrt_inode_uidgid(dir, CAP_FOWNER)`) and the owner-shaped calls -
/// `chmod`, `chown`, `utimes` on a node the caller does not own - ask.
pub const CAP_FOWNER: u32 = 3;
/// `CAP_SETGID`, which an unprivileged `/proc/<pid>/gid_map` writer lacks - the
/// capability that lifts the one-line rule there.
pub const CAP_SETGID: u32 = 6;
/// `CAP_SETUID`, the counterpart of [`CAP_SETGID`] for `/proc/<pid>/uid_map`.
pub const CAP_SETUID: u32 = 7;
/// `CAP_SETPCAP`, which `PR_CAPBSET_DROP` requires.
pub const CAP_SETPCAP: u32 = 8;
/// `CAP_LINUX_IMMUTABLE`, which setting or clearing the `i` and `a` inode flags
/// requires - `FS_IOC_SETFLAGS`' own gate, and the one `chattr(1)` names in its
/// errors.
pub const CAP_LINUX_IMMUTABLE: u32 = 9;
/// `CAP_SYS_CHROOT`, which `chroot(2)` requires.
pub const CAP_SYS_CHROOT: u32 = 18;
/// `CAP_SYS_ADMIN`, which the namespace `unshare`/`setns` family requires.
pub const CAP_SYS_ADMIN: u32 = 21;
/// `CAP_SYS_BOOT`, which `reboot(2)` requires.
pub const CAP_SYS_BOOT: u32 = 22;
/// `CAP_SYS_TTY_CONFIG`, which `vhangup(2)` requires.
pub const CAP_SYS_TTY_CONFIG: u32 = 26;
/// `CAP_MKNOD`, which a **device** node's `mknod(2)` requires - and only a
/// device node's: `S_IFREG`, `S_IFIFO` and `S_IFSOCK` nodes are served to a
/// caller holding nothing, as is a character node whose `dev` is `0` (the
/// whiteout shape). Measured unprivileged rather than recalled
/// (`.workbuddy/probe_mknod_cap.c`); `mknod_needs_capability` in the kernel's
/// ABI module is the question this bit answers.
pub const CAP_MKNOD: u32 = 27;
/// `CAP_SYSLOG`, which every `syslog(2)` type requires - the gate Linux
/// checks *before* the type, so even an unknown type answers `EPERM` from a
/// caller without it.
pub const CAP_SYSLOG: u32 = 34;
/// `CAP_NET_RAW`, which every raw socket requires - `AF_PACKET` and `AF_INET`'s
/// `SOCK_RAW` alike, whatever protocol the caller names. Linux asks for it
/// *before* it looks at the protocol: measured on the host
/// (`.workbuddy/probe_rawip.c`, a caller that dropped to an unprivileged id),
/// `SOCK_RAW` with ICMP, with `IPPROTO_RAW`, with TCP and with a protocol number
/// nothing has are **all** `EPERM`, so the gate is not visible in one refusal's
/// errno.
pub const CAP_NET_RAW: u32 = 13;

/// `SUID_DUMP_DISABLE`: nobody may inspect the process without a capability.
pub const SUID_DUMP_DISABLE: u8 = 0;
/// `SUID_DUMP_USER`: its own user may inspect it.
pub const SUID_DUMP_USER: u8 = 1;
/// `SUID_DUMP_ROOT`: only root may inspect it. Never *set* through `prctl`
/// (Linux's `PR_SET_DUMPABLE` accepts only 0 and 1) but the kernel itself
/// uses it, so `PR_GET_DUMPABLE` can report it.
pub const SUID_DUMP_ROOT: u8 = 2;

/// The user and group ids six `identity` words, as `ptrace_may_access` and
/// the uid-change rule see them. Split out from `sched::Identity` so this
/// module is pure and host-testable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ids {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
}

impl Ids {
    /// Whether any of the six moved: the question the credential rules turn on.
    /// A `setgroups` that changes only the supplementary groups must *not*
    /// count - Linux does not touch the capability sets for it.
    pub fn differs(self, other: Ids) -> bool {
        self != other
    }

    /// Whether any of the six ids is still root's. Linux's `cap_emulate_setxuid`
    /// keeps the permitted set while *some* id is root and only clears the
    /// effective set; a full drop, where none is, clears both.
    pub fn holds_root(self) -> bool {
        self.ruid == 0
            || self.euid == 0
            || self.suid == 0
            || self.rgid == 0
            || self.egid == 0
            || self.sgid == 0
    }
}

/// A thread group's credentials.
///
/// `Copy` on purpose: it is a handful of words, and every reader wants a
/// snapshot it can hold without the table's lock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Credentials {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
    pub bounding: u64,
    /// One of `SUID_DUMP_*`.
    pub dumpable: u8,
    /// `PR_SET_KEEPCAPS`: whether a full uid drop keeps the permitted set.
    pub keepcaps: bool,
    /// Whether the sets above are held in the **initial** user namespace.
    ///
    /// A capability is held *in* a namespace and the namespaces below it, and
    /// Linux's `cap_capable` answers by walking from the namespace being asked
    /// about *up* to `current_cred()->user_ns` - so the two questions this
    /// kernel asks are two different ones about the same word:
    /// [`has_effective`](Credentials::has_effective) is the caller's own
    /// namespace (which is what `ns_capable(current_user_ns(), cap)` asks, and
    /// what most checks here want - `unshare`'s `CAP_SYS_ADMIN`, `chroot`'s
    /// `CAP_SYS_CHROOT`), and [`capable`](Credentials::capable) is
    /// `capable(cap)`, true only for a credential that never left the initial
    /// one. Measured on a current host (`.workbuddy/probe_capable_scope.{c,sh}`):
    /// a user-namespace root - uid 0 in it, `CapEff` `0x1ffffffffff` - is
    /// **served** `chroot` and `unshare(CLONE_NEWNS)` and **refused**
    /// `mknod(c 1 3)`, `reboot`, `syslog` and `FS_IOC_SETFLAGS`, so those four
    /// ask `capable()` where the first two ask the caller's own namespace.
    ///
    /// A flag rather than a namespace pointer, because of which way the doors
    /// run: every door into a namespace here (`unshare`, `clone(CLONE_NEWUSER)`
    /// and `setns`, which refuses a caller's *own* namespace with `EINVAL`)
    /// moves a caller **deeper**, so this can only ever go from true to false.
    /// The general question - a capability held in some other namespace, an
    /// ancestor's or a sibling's - is not expressible in this word, and
    /// `docs/uapi/` §5.2 names the two call sites that need it (`setns`'s and
    /// `AF_PACKET`'s, which Linux asks of the *object's* namespace).
    pub user_ns_initial: bool,
}

impl Credentials {
    /// The state a fresh process starts in: every capability held and in the
    /// bounding set, nothing inheritable (Linux's own default - the
    /// `CapInh` word a login shell carries is set by `pam`, not by the
    /// kernel), dumpable by its own user, and in the initial user namespace.
    pub const fn initial() -> Self {
        Credentials {
            effective: CAP_FULL,
            permitted: CAP_FULL,
            inheritable: 0,
            bounding: CAP_FULL,
            dumpable: SUID_DUMP_USER,
            keepcaps: false,
            user_ns_initial: true,
        }
    }

    /// Whether `cap` is in the effective set - the question every privilege
    /// check in Linux is really asking.
    ///
    /// The namespace this is asked *of* is the caller's own, which is
    /// `ns_capable(current_user_ns(), cap)`: the sets are scoped to exactly
    /// that namespace when they are granted, so holding the bit *is* holding it
    /// there. A check whose Linux rule is `capable(cap)` - the initial user
    /// namespace - asks [`capable`](Credentials::capable) instead.
    pub fn has_effective(self, cap: u32) -> bool {
        cap <= CAP_LAST_CAP && self.effective & (1u64 << cap) != 0
    }

    /// Whether `cap` is held in the **initial** user namespace: `capable(cap)`,
    /// which is what `vfs_mknod`, `fileattr_set`'s immutable bit and `syslog`'s
    /// reader gate ask outright, and what `reboot`'s answer comes to for a
    /// caller with no pid namespace of its own (Linux asks *that* one -
    /// `ns_capable(task_active_pid_ns(current)->user_ns, CAP_SYS_BOOT)`, which
    /// `docs/uapi/` §5.2 names as the refinement this word cannot express).
    ///
    /// The measured difference between this and
    /// [`has_effective`](Credentials::has_effective) is the whole
    /// user-namespace rung: entering one hands the caller every capability *in
    /// it*, and a capability held in a child namespace is not one held in the
    /// parent - so a user-namespace root passes the checks that ask its own
    /// namespace and fails these
    /// ([`user_ns_initial`](Credentials::user_ns_initial) carries the
    /// measurements).
    pub fn capable(self, cap: u32) -> bool {
        self.user_ns_initial && self.has_effective(cap)
    }

    /// Whether `cap` is in the bounding set, which is what `PR_CAPBSET_READ`
    /// answers.
    pub fn has_bounding(self, cap: u32) -> bool {
        cap <= CAP_LAST_CAP && self.bounding & (1u64 << cap) != 0
    }

    /// Apply a change of the uid/gid identity.
    ///
    /// Measured rules, all on Linux 7.2.7: the effective set is cleared
    /// whenever the identity moves to a non-root `euid` and *restored from the
    /// permitted set* when it moves back to 0 (a process that dropped its
    /// effective privileges and raised them back through the saved id has them
    /// again - `cap_emulate_setxuid`'s `new->cap_effective = new->cap_permitted`);
    /// the permitted set survives while any of the six ids is still root's, and
    /// is cleared by a full drop unless `KEEPCAPS` asked for it to stay; the
    /// bounding set is never touched by an identity change. The dumpable flag
    /// resets to `SUID_DUMP_DISABLE`, which is Linux's
    /// `set_dumpable(mm, suid_dumpable)` with the default sysctl.
    pub fn on_identity_change(&mut self, old: Ids, new: Ids) {
        if !old.differs(new) {
            return;
        }
        if new.euid == 0 {
            self.effective = self.permitted;
        } else {
            self.effective = 0;
        }
        if !new.holds_root() && !self.keepcaps {
            self.permitted = 0;
        }
        // Linux's sysctl is `fs.suid_dumpable`, whose compiled-in default is
        // 0. The machine these rules were measured on had it set to 2, so
        // this is the one measurement deliberately not copied: 0 is both
        // Linux's default and the safer side, and the sysctl is the named gap.
        self.dumpable = SUID_DUMP_DISABLE;
    }

    /// Apply `execve`. Measured: for an `euid` of 0 the permitted and
    /// effective sets become the bounding set - a root process that had
    /// lowered its permitted set regains every capability the bounding set
    /// still holds, which is why dropping a bounding capability is the way to
    /// take one away from a root program for good. For any other `euid` the
    /// sets are preserved (this kernel has no file capabilities, so there is
    /// nothing to grant from, and the measured non-root exec left both sets
    /// exactly as they were).
    pub fn on_exec(&mut self, euid: u32) {
        if euid == 0 {
            self.permitted = self.bounding;
            self.effective = self.bounding;
        }
    }

    /// The capability state a task holds the instant it is in a user namespace
    /// it entered: the effective, permitted and **bounding** sets are full
    /// again, and nothing is inheritable.
    ///
    /// This is the user-namespace rung's other half - the *grant*, the
    /// counterpart of the id translation `getuid`/`stat` do. Making a user
    /// namespace needs no capability because it **is** the grant: inside it the
    /// maker holds every capability, scoped to the namespace it just made, and
    /// that is what lets an unprivileged program mount, map ids and configure a
    /// network it owns.
    ///
    /// Measured on this host's Linux (7.2.9, `.workbuddy/probe-userns7.c`, run
    /// as root and as an unprivileged caller), all four doors into a namespace:
    /// a caller that cleared its sets with `capset(0,0,0)` reads
    /// `CapEff=CapPrm=0x1ffffffffff` the moment it has unshared, and a
    /// `unshare(CLONE_NEWNS)` - which needs `CAP_SYS_ADMIN` - then succeeds, so
    /// the grant is functional and not a reporting artifact; a capability
    /// dropped from the **bounding** set is back too (`cap_bset` full again);
    /// an inheritable bit set before the unshare is gone after it; and
    /// `clone(CLONE_NEWUSER)` and a `setns` into a child namespace grant it the
    /// same way. What the grant is *not*: it does not touch the task's
    /// identity, `dumpable` or `KEEPCAPS`, which are the namespace's business
    /// only through the id maps, and it grants nothing in the *parent*
    /// namespace - a capability is held in a namespace and the namespaces below
    /// it, so the sets are the *new* namespace's and
    /// [`capable`](Credentials::capable) - the initial-namespace question -
    /// answers false from here on. That is the whole of the scoping a caller
    /// can observe through this kernel's doors: none of them opens *upward*
    /// (`setns` refuses a caller's own namespace with `EINVAL`, and Linux's own
    /// rule is that the credential must be in an ancestor of the namespace it
    /// joins), so this flag moves one way only.
    pub fn on_enter_user_ns(&mut self) {
        self.effective = CAP_FULL;
        self.permitted = CAP_FULL;
        self.bounding = CAP_FULL;
        self.inheritable = 0;
        self.user_ns_initial = false;
    }

    /// `capset`: validate one record's three sets and store them. `Err(())` is
    /// the `EPERM` Linux answers.
    ///
    /// Linux's `cap_capset` checks `CAP_SETPCAP` *first* and, holding it,
    /// accepts anything; only a caller without it is held to the subset rules -
    /// the new effective set inside the new permitted set, the new permitted
    /// set inside the *old* permitted set (a ceiling may be lowered and never
    /// raised), and a capability added to inheritable only if it is already in
    /// permitted or inheritable. Root holds `CAP_SETPCAP`, so on this
    /// single-user machine the checks almost never fire - which is exactly why
    /// they are written down rather than assumed away: the day a program drops
    /// `CAP_SETPCAP` they are the only thing between it and everything.
    pub fn capset(&mut self, effective: u64, permitted: u64, inheritable: u64) -> Result<(), ()> {
        if !self.has_effective(CAP_SETPCAP) {
            if effective & !permitted != 0 {
                return Err(());
            }
            if permitted & !self.permitted != 0 {
                return Err(());
            }
            if inheritable & !(self.inheritable | self.permitted) != 0 {
                return Err(());
            }
        }
        self.effective = effective & CAP_FULL;
        self.permitted = permitted & CAP_FULL;
        self.inheritable = inheritable & CAP_FULL;
        Ok(())
    }

    /// `PR_CAPBSET_DROP`: take a capability out of the bounding set.
    ///
    /// The capability is asked *before* the number is range-checked, because
    /// that is the order Linux is measured in: an unprivileged
    /// `PR_CAPBSET_DROP(100)` answers `EPERM` where root's answers `EINVAL`.
    /// `cap` is the raw argument word rather than a `u32`, so a value too
    /// large to be a capability number is still refused rather than truncated
    /// into one.
    ///
    /// ```
    /// use mpos_cred::{CAP_LAST_CAP, CAP_SETPCAP, CapErr, Credentials};
    ///
    /// let mut creds = Credentials::initial();
    /// // A root process drops a capability it holds: accepted, and gone from the set.
    /// assert_eq!(creds.capbset_drop(CAP_SETPCAP as u64), Ok(()));
    /// assert!(!creds.has_bounding(CAP_SETPCAP));
    /// // A number past CAP_LAST_CAP is EINVAL - but only asked *after* the
    /// // privilege question: for a caller without CAP_SETPCAP the same call
    /// // is EPerm whatever the number says.
    /// assert_eq!(
    ///     creds.capbset_drop(CAP_LAST_CAP as u64 + 1),
    ///     Err(CapErr::Invalid)
    /// );
    /// let mut lowered = Credentials::initial();
    /// lowered.effective &= !(1 << CAP_SETPCAP);
    /// assert_eq!(lowered.capbset_drop(0), Err(CapErr::Perm));
    /// ```
    pub fn capbset_drop(&mut self, cap: u64) -> Result<(), CapErr> {
        if !self.has_effective(CAP_SETPCAP) {
            return Err(CapErr::Perm);
        }
        if cap > CAP_LAST_CAP as u64 {
            return Err(CapErr::Invalid);
        }
        self.bounding &= !(1u64 << cap);
        Ok(())
    }
}

/// Which errno a capability operation owes its caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapErr {
    /// `EPERM` - the caller lacks the capability the operation needs.
    Perm,
    /// `EINVAL` - the capability number names nothing.
    Invalid,
}

/// `ptrace_may_access`: may `caller` inspect `target`?
///
/// Linux's `__ptrace_may_access` in the shape this kernel can honour: a task
/// may always inspect its own thread group; a caller holding `CAP_SYS_PTRACE`
/// in its effective set may inspect anything; and otherwise the two must share
/// a user id (any of real or effective, either direction) *and* the target must
/// be dumpable. Root holds the capability, so nothing a root program does here
/// changes - which is what keeps this from disturbing the single-user image.
///
/// What is not modelled, and is a named divergence: Linux also compares the
/// *group* ids and consults the security modules, and its `mm->dumpable` is
/// per-`mm` rather than per-credential. Neither matters on a machine where
/// every task is root; both would matter the moment there are two users.
pub fn ptrace_may_access(
    caller: Ids,
    caller_creds: &Credentials,
    target: Ids,
    target_creds: &Credentials,
    same_thread_group: bool,
) -> bool {
    if same_thread_group {
        return true;
    }
    if caller_creds.has_effective(CAP_SYS_PTRACE) {
        return true;
    }
    let shares_uid = caller.ruid == target.ruid
        || caller.ruid == target.euid
        || caller.euid == target.ruid
        || caller.euid == target.euid;
    if !shares_uid {
        return false;
    }
    target_creds.dumpable != SUID_DUMP_DISABLE
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    fn root() -> Ids {
        Ids {
            ruid: 0,
            euid: 0,
            suid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
        }
    }

    fn user(n: u32) -> Ids {
        Ids {
            ruid: n,
            euid: n,
            suid: n,
            rgid: n,
            egid: n,
            sgid: n,
        }
    }

    #[test]
    fn a_fresh_process_holds_every_capability() {
        let c = Credentials::initial();
        assert_eq!(c.permitted, CAP_FULL);
        assert_eq!(c.effective, CAP_FULL);
        assert_eq!(c.bounding, CAP_FULL);
        assert!(c.has_effective(CAP_SYS_PTRACE));
        // The boundary: bit 40 is the last one named, bit 41 is not a capability.
        assert!(c.has_effective(CAP_LAST_CAP));
        assert!(!c.has_effective(CAP_LAST_CAP + 1));
        assert_eq!(CAP_FULL.count_ones(), CAP_LAST_CAP + 1);
    }

    #[test]
    fn a_partial_identity_change_keeps_permitted_and_clears_effective() {
        // Measured: setresuid(0, 65534, 0) - the *saved* id is still root's.
        let mut c = Credentials::initial();
        let old = root();
        let new = Ids {
            ruid: 0,
            euid: 65534,
            suid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
        };
        c.on_identity_change(old, new);
        assert_eq!(c.permitted, CAP_FULL, "some id is still root's");
        assert_eq!(c.effective, 0, "the effective set is always cleared");
        assert_eq!(c.dumpable, SUID_DUMP_DISABLE);
    }

    #[test]
    fn returning_to_root_restores_the_effective_set() {
        // Measured: after setresuid(0, 65534, 0) - CapEff 0 - a
        // setresuid(0, 0, 0) left CapPrm and CapEff full again. This is the
        // raise-back a set-uid helper performs through its saved id, and
        // clearing the effective set on the way *back* would strand it.
        let mut c = Credentials::initial();
        let dropped = Ids {
            ruid: 0,
            euid: 65534,
            suid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
        };
        c.on_identity_change(root(), dropped);
        assert_eq!(c.effective, 0);
        c.on_identity_change(dropped, root());
        assert_eq!(c.permitted, CAP_FULL);
        assert_eq!(c.effective, CAP_FULL);
    }

    #[test]
    fn a_full_identity_change_clears_both_sets() {
        // Measured: setresuid(65534, 65534, 65534), no KEEPCAPS.
        let mut c = Credentials::initial();
        c.on_identity_change(root(), user(65534));
        assert_eq!(c.permitted, 0);
        assert_eq!(c.effective, 0);
        assert_eq!(c.bounding, CAP_FULL, "an identity change never bounds");
    }

    #[test]
    fn keepcaps_keeps_the_permitted_set_across_a_full_drop() {
        // Measured: the same drop with PR_SET_KEEPCAPS(1).
        let mut c = Credentials::initial();
        c.keepcaps = true;
        c.on_identity_change(root(), user(65534));
        assert_eq!(c.permitted, CAP_FULL);
        assert_eq!(c.effective, 0, "KEEPCAPS does not keep the effective set");
    }

    #[test]
    fn an_identity_that_did_not_move_changes_nothing() {
        // The case that matters is setgroups: it writes the identity record
        // with the same six ids and must not touch a capability.
        let mut c = Credentials::initial();
        let before = c;
        c.on_identity_change(root(), root());
        assert_eq!(c, before);
    }

    #[test]
    fn exec_widens_a_root_process_back_to_its_bounding_set() {
        // Measured: root cleared CAP_CHOWN from permitted, exec, and the child
        // had it again - pP' = bounding for an euid of 0.
        let mut c = Credentials::initial();
        c.capset(CAP_FULL & !1, CAP_FULL & !1, 0).expect("lowering");
        assert!(!c.has_effective(0));
        c.on_exec(0);
        assert_eq!(c.permitted, CAP_FULL);
        assert!(c.has_effective(0));
    }

    #[test]
    fn a_dropped_bounding_capability_survives_exec() {
        // Measured: PR_CAPBSET_DROP(0) then exec left CAP_CHOWN gone from the
        // child's permitted and effective sets. This is the whole observable
        // promise the option makes.
        let mut c = Credentials::initial();
        c.capbset_drop(0).expect("root holds CAP_SETPCAP");
        assert!(!c.has_bounding(0));
        c.on_exec(0);
        assert!(!c.has_effective(0));
        assert!(!c.has_bounding(0));
    }

    #[test]
    fn a_non_root_exec_preserves_the_sets() {
        // Measured: an euid of 65534 exec'd with permitted and effective
        // exactly as they were.
        let mut c = Credentials::initial();
        c.on_identity_change(root(), user(65534));
        c.keepcaps = true;
        let before = c;
        c.on_exec(65534);
        assert_eq!(c, before);
    }

    #[test]
    fn entering_a_user_namespace_grants_the_full_set() {
        // Measured (probe-userns7.c, root and unprivileged): a caller that
        // cleared its sets with capset(0,0,0) reads the full effective and
        // permitted sets the instant it has unshared; a capability it had
        // dropped from the *bounding* set is back; an inheritable bit it had
        // set is gone. The bounding drop comes first because it needs
        // CAP_SETPCAP, which the clear takes away.
        let mut c = Credentials::initial();
        c.capbset_drop(CAP_SYS_ADMIN as u64)
            .expect("root holds CAP_SETPCAP");
        c.capset(0, 0, 1).expect("clearing the sets");
        assert_eq!((c.effective, c.permitted, c.inheritable), (0, 0, 1));
        assert!(!c.has_bounding(CAP_SYS_ADMIN));
        c.on_enter_user_ns();
        assert_eq!(c.effective, CAP_FULL);
        assert_eq!(c.permitted, CAP_FULL);
        assert_eq!(c.bounding, CAP_FULL, "the bounding set comes back too");
        assert_eq!(c.inheritable, 0, "and nothing is inheritable");
        // The grant is the capability state and nothing else: the dumpable
        // flag is the namespace's business through the maps, not this.
        assert_eq!(c.dumpable, SUID_DUMP_USER);
    }

    #[test]
    fn the_two_capability_questions_are_the_initial_namespace_and_the_callers_own() {
        // Measured on a current host (`.workbuddy/probe_capable_scope.sh`, one
        // call per namespace): a user-namespace root - uid 0 in it, `CapEff`
        // `0x1ffffffffff` read back - is *served* `chroot` and
        // `unshare(CLONE_NEWNS)` and *refused* `mknod(c 1 3)`, `reboot`,
        // `syslog` and `FS_IOC_SETFLAGS`. So the first two are the caller's own
        // namespace (the bit) and the other four are the initial one (the bit
        // *and* the flag).
        let mut c = Credentials::initial();
        assert!(c.capable(CAP_MKNOD) && c.has_effective(CAP_MKNOD));
        c.on_enter_user_ns();
        assert!(
            c.has_effective(CAP_MKNOD),
            "the grant is a capability held in the namespace entered"
        );
        assert!(
            !c.capable(CAP_MKNOD),
            "and it is nothing in the namespace that was left"
        );
        // A capability the *bounding* set no longer holds is false for both,
        // inside and outside a namespace: the grant is the namespace's, not a
        // licence to hold what nothing holds.
        let mut c = Credentials::initial();
        c.capbset_drop(CAP_MKNOD as u64)
            .expect("root holds CAP_SETPCAP");
        c.capset(0, 0, 0).expect("clearing the sets");
        assert!(!c.capable(CAP_MKNOD) && !c.has_effective(CAP_MKNOD));
        // And the flag moves one way only: nothing this kernel can do puts a
        // credential back in the initial namespace (`setns` refuses a caller's
        // own namespace with `EINVAL`, and every other door moves deeper).
        c.on_enter_user_ns();
        assert!(!c.user_ns_initial);
        c.on_enter_user_ns();
        assert!(
            !c.user_ns_initial,
            "a second namespace is still not the first"
        );
    }

    #[test]
    fn capset_refuses_to_raise_a_ceiling_it_does_not_hold() {
        // Without CAP_SETPCAP the subset rules apply; with it Linux accepts
        // anything, so the capability is dropped first to reach them.
        let mut c = Credentials::initial();
        c.permitted &= !(1u64 << CAP_SETPCAP);
        c.effective &= !(1u64 << CAP_SETPCAP);
        assert!(!c.has_effective(CAP_SETPCAP));
        // effective outside permitted
        assert_eq!(c.capset(2, 1, 0), Err(()));
        // permitted outside the old permitted set
        c.capset(0, 0, 0).expect("lowering everything is allowed");
        assert_eq!(c.capset(0, 1, 0), Err(()));
        // inheritable outside inheritable | permitted
        assert_eq!(c.capset(0, 0, 1), Err(()));
    }

    #[test]
    fn a_caller_holding_setpcap_may_set_anything() {
        // Linux's cap_capset returns 0 as soon as CAP_SETPCAP is held, before
        // any subset check - and root always holds it, which is why the checks
        // above cannot fire on this image.
        let mut c = Credentials::initial();
        assert!(c.has_effective(CAP_SETPCAP));
        c.capset(CAP_FULL, CAP_FULL, CAP_FULL).expect("root");
        assert_eq!(c.inheritable, CAP_FULL);
    }

    #[test]
    fn capset_stores_what_it_accepts() {
        let mut c = Credentials::initial();
        c.capset(0b11, 0b111, 0b1000).expect("inside the ceiling");
        assert_eq!(
            (c.effective, c.permitted, c.inheritable),
            (0b11, 0b111, 0b1000)
        );
    }

    #[test]
    fn the_bounding_set_asks_the_capability_before_the_number() {
        // Measured: unprivileged PR_CAPBSET_DROP(100) is EPERM, root's is EINVAL.
        let mut c = Credentials::initial();
        c.effective = 0;
        assert_eq!(c.capbset_drop(100), Err(CapErr::Perm));
        let mut root_c = Credentials::initial();
        assert_eq!(root_c.capbset_drop(100), Err(CapErr::Invalid));
        assert_eq!(root_c.capbset_drop(40), Ok(()));
        assert!(!root_c.has_bounding(40));
    }

    #[test]
    fn a_root_caller_may_always_attach_and_a_stranger_may_not() {
        let root_c = Credentials::initial();
        let mut user_c = Credentials::initial();
        user_c.effective = 0;
        user_c.permitted = 0;
        // Own thread group: always.
        assert!(ptrace_may_access(
            user(1000),
            &user_c,
            user(1001),
            &root_c,
            true
        ));
        // Root holds CAP_SYS_PTRACE, so the dumpable flag does not stop it -
        // which is Linux's answer and the reason this cannot regress the gate.
        assert!(ptrace_may_access(
            root(),
            &root_c,
            user(1001),
            &root_c,
            false
        ));
        // A stranger without the capability: the uids must match...
        assert!(!ptrace_may_access(
            user(1000),
            &user_c,
            user(1001),
            &user_c,
            false
        ));
        assert!(ptrace_may_access(
            user(1000),
            &user_c,
            user(1000),
            &user_c,
            false
        ));
        // ...and the target must be dumpable.
        let mut hidden = user_c;
        hidden.dumpable = SUID_DUMP_DISABLE;
        assert!(!ptrace_may_access(
            user(1000),
            &user_c,
            user(1000),
            &hidden,
            false
        ));
    }
}
