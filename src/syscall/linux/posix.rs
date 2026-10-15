//! The POSIX completion set's process half: identity, capabilities,
//! scheduling, memory advice, and the machine-level calls a libc reaches
//! for on its way through anything.
//!
//! Every handler here answers the man page's contract with the argument
//! shaping this kernel can keep: identity is one user (root) whose
//! "become root" calls are already true and whose privilege changes are
//! refused the way Linux refuses a non-root caller's; scheduling is one
//! policy (SCHED_OTHER) with the priority range the man page's table
//! spells; memory is eagerly resident, which makes the lock family's
//! promise vacuously true and the advice calls accepted.
//!
//! This module lives beside `fs` and `proc` rather than inside `proc`,
//! because it is the surface *completed* rather than the machinery: the
//! calls are table answers and validations over the scheduler's exported
//! state, and nothing here needs to build a signal frame or switch a
//! stack. `pause` and `rt_sigsuspend` are the exception that proves the
//! rule - they are a wait, and they spell it through
//! `block_until_deadline`, the same three-way wait every other blocking
//! call uses.

use alloc::vec;
use alloc::vec::Vec;

use crate::sched::WaitEnd;
use crate::syscall::{ARG_COUNT, buffer_ok, err};

/// The x86_64 page size, as the memory calls here round with it.
const PAGE_SIZE: usize = 4096;

/// Whether `pid` names a live task, and which thread group it belongs to.
/// `0` is the caller. The answer is the man page's pair of questions at
/// once: a caller wants to know "does it exist" (ESRCH when not) and,
/// for the tgkill family, "is the group the one I named".
fn live_thread(pid: isize) -> Option<usize> {
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        return Some(crate::sched::current().0);
    }
    crate::sched::snapshot(pid as usize).map(|snap| snap.tid)
}

// ---------------------------------------------------------------------------
// Identity: one user, whose changes are true or refused
// ---------------------------------------------------------------------------

/// One word of a `setreuid`/`setresuid` call: `usize::MAX` is the id that
/// stays, spelled the way the C headers spell `-1`.
const NO_ID: usize = usize::MAX;

/// `getuid()` — the real user id: who the process was started as, rather
/// than who it is acting as now.
pub fn handle_getuid(_args: [usize; ARG_COUNT]) -> isize {
    crate::sched::current_identity().ruid as isize
}

/// `geteuid()` — the effective user id: who privilege checks act as, and the
/// word every `setuid`-family permission rule branches on.
pub fn handle_geteuid(_args: [usize; ARG_COUNT]) -> isize {
    crate::sched::current_identity().euid as isize
}

/// `getgid()` — the real group id.
pub fn handle_getgid(_args: [usize; ARG_COUNT]) -> isize {
    crate::sched::current_identity().rgid as isize
}

/// `getegid()` — the effective group id.
pub fn handle_getegid(_args: [usize; ARG_COUNT]) -> isize {
    crate::sched::current_identity().egid as isize
}

/// `setuid(uid)` — one id with two meanings, decided by the caller's
/// privilege: root sets the real, effective and saved ids all at once,
/// because for root the call is the whole identity change; anyone else may
/// only move the effective id to one they already hold somewhere - the real
/// or the saved - which is what lets a process that dropped its privileges
/// raise itself back.
pub fn handle_setuid(args: [usize; ARG_COUNT]) -> isize {
    identity_set_one(args[0], true)
}

/// `setgid(gid)` — [`handle_setuid`], spelled for groups.
pub fn handle_setgid(args: [usize; ARG_COUNT]) -> isize {
    identity_set_one(args[0], false)
}

/// The most supplementary groups a process may carry, the figure Linux
/// reports from `sysconf(_SC_NGROUPS_MAX)`.
const NGROUPS_MAX: usize = 65536;

/// `setgroups(n, list)` — install the supplementary groups, the set a
/// `:group` rule in `doas.conf` matches membership against. Only a
/// privileged process may name one: the login that assigns a user's groups
/// runs as root, and letting the launched program re-pick them would be a
/// way to gain groups it was not given. A count of zero - the "clear the
/// extras" call - needs no list at all.
pub fn handle_setgroups(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (count, list) = (args[0], args[1]);
    if count > NGROUPS_MAX {
        return err::EINVAL;
    }
    if crate::sched::current_identity().euid != 0 {
        return err::EPERM;
    }
    let mut groups = Vec::new();
    if count > 0 {
        if list == 0 || !buffer_ok(list, count * 4, false, from_user) {
            return err::EFAULT;
        }
        groups = vec![0u32; count];
        unsafe {
            for (i, g) in groups.iter_mut().enumerate() {
                *g = ((list + i * 4) as *const u32).read_unaligned();
            }
        }
    }
    let mut identity = crate::sched::current_identity();
    identity.groups = groups;
    crate::sched::set_current_identity(identity);
    0
}

/// `getgroups(size, list)` — report the supplementary groups. A `size` of
/// zero asks for the count alone without touching the list, which is how a
/// libc probes for the buffer it needs; a list too small for the count is
/// the man page's EINVAL rather than a silent truncation.
pub fn handle_getgroups(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (size, list) = (args[0] as isize, args[1]);
    if size < 0 {
        return err::EINVAL;
    }
    let identity = crate::sched::current_identity();
    let n = identity.groups.len();
    if size == 0 {
        return n as isize;
    }
    if (size as usize) < n {
        return err::EINVAL;
    }
    if n > 0 {
        if list == 0 || !buffer_ok(list, n * 4, true, from_user) {
            return err::EFAULT;
        }
        unsafe {
            for (i, g) in identity.groups.iter().enumerate() {
                ((list + i * 4) as *mut u32).write_unaligned(*g);
            }
        }
    }
    n as isize
}

fn identity_set_one(word: usize, uid: bool) -> isize {
    if word == NO_ID || word > u32::MAX as usize {
        return err::EINVAL;
    }
    let word = word as u32;
    let mut identity = crate::sched::current_identity();
    // Both halves of this question are about the *effective user* id, because
    // that is what capabilities follow: a set-uid program with `euid` 0 holds
    // `CAP_SETUID` and `CAP_SETGID` alike, whatever its effective group id
    // happens to be. Reading `egid` for the group half refuses the one caller
    // that matters - the set-uid helper dropping to the ids it was installed
    // to hand over, which is exactly the shape `su` and `doas` are.
    let privileged = identity.euid == 0;
    if privileged {
        if uid {
            identity.ruid = word;
            identity.euid = word;
            identity.suid = word;
        } else {
            identity.rgid = word;
            identity.egid = word;
            identity.sgid = word;
        }
    } else {
        let (real, saved) = if uid {
            (identity.ruid, identity.suid)
        } else {
            (identity.rgid, identity.sgid)
        };
        if word != real && word != saved {
            return err::EPERM;
        }
        if uid {
            identity.euid = word;
        } else {
            identity.egid = word;
        }
    }
    crate::sched::set_current_identity(identity);
    0
}

/// `setreuid(ruid, euid)` — the real and effective ids, each -1 to stay.
/// The man page's rules: an unprivileged caller may only move an id to one
/// of the three it already holds, and a privileged one may set both freely -
/// with the saved id following the new effective one, so a root process that
/// temporarily drops its privileges can find its way back through the saved
/// id the drop left behind.
pub fn handle_setreuid(args: [usize; ARG_COUNT]) -> isize {
    identity_set_re(args, true)
}

/// `setregid(rgid, egid)` — [`handle_setreuid`], spelled for groups.
pub fn handle_setregid(args: [usize; ARG_COUNT]) -> isize {
    identity_set_re(args, false)
}

fn identity_set_re(args: [usize; ARG_COUNT], uid: bool) -> isize {
    let identity = crate::sched::current_identity();
    // Both halves of this question are about the *effective user* id, because
    // that is what capabilities follow: a set-uid program with `euid` 0 holds
    // `CAP_SETUID` and `CAP_SETGID` alike, whatever its effective group id
    // happens to be. Reading `egid` for the group half refuses the one caller
    // that matters - the set-uid helper dropping to the ids it was installed
    // to hand over, which is exactly the shape `su` and `doas` are.
    let privileged = identity.euid == 0;
    // Privileged, the saved id follows the new effective one; an
    // unprivileged caller's saved id never moves - that is the floor the
    // raise-itself-back rule stands on.
    let saved = if privileged && args[1] != NO_ID {
        args[1]
    } else {
        NO_ID
    };
    identity_set_res([args[0], args[1], saved, 0, 0, 0], uid)
}

/// `setresuid(ruid, euid, suid)` — three ids, each -1 to stay: the real,
/// the effective and the saved, all three settable in one call. Privileged
/// callers set any of the three to anything; unprivileged ones may only
/// move one to a value already among the three.
pub fn handle_setresuid(args: [usize; ARG_COUNT]) -> isize {
    identity_set_res(args, true)
}

/// `setresgid(rgid, egid, sgid)` — [`handle_setresuid`], spelled for groups.
pub fn handle_setresgid(args: [usize; ARG_COUNT]) -> isize {
    identity_set_res(args, false)
}

fn identity_set_res(args: [usize; ARG_COUNT], uid: bool) -> isize {
    let mut identity = crate::sched::current_identity();
    let held: [u32; 3] = if uid {
        [identity.ruid, identity.euid, identity.suid]
    } else {
        [identity.rgid, identity.egid, identity.sgid]
    };
    // Both halves of this question are about the *effective user* id, because
    // that is what capabilities follow: a set-uid program with `euid` 0 holds
    // `CAP_SETUID` and `CAP_SETGID` alike, whatever its effective group id
    // happens to be. Reading `egid` for the group half refuses the one caller
    // that matters - the set-uid helper dropping to the ids it was installed
    // to hand over, which is exactly the shape `su` and `doas` are.
    let privileged = identity.euid == 0;
    let mut words = held;
    for (slot, &word) in words.iter_mut().zip(args.iter()) {
        if word == NO_ID {
            continue;
        }
        if word > u32::MAX as usize {
            return err::EINVAL;
        }
        if !privileged && !held.contains(&(word as u32)) {
            return err::EPERM;
        }
        *slot = word as u32;
    }
    if uid {
        identity.ruid = words[0];
        identity.euid = words[1];
        identity.suid = words[2];
    } else {
        identity.rgid = words[0];
        identity.egid = words[1];
        identity.sgid = words[2];
    }
    crate::sched::set_current_identity(identity);
    0
}

/// `getresuid(ruid, euid, suid)` — the real, effective and saved ids, one
/// word each, read back in that order.
pub fn handle_getresuid(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    identity_words(args, from_user, true)
}

/// `getresgid(rgid, egid, sgid)` — [`handle_getresuid`], spelled for groups.
pub fn handle_getresgid(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    identity_words(args, from_user, false)
}

fn identity_words(args: [usize; ARG_COUNT], from_user: bool, uid: bool) -> isize {
    let ptrs = [args[0], args[1], args[2]];
    for &ptr in &ptrs {
        if ptr == 0 || !buffer_ok(ptr, 8, true, from_user) {
            return err::EFAULT;
        }
    }
    let identity = crate::sched::current_identity();
    let words: [u32; 3] = if uid {
        [identity.ruid, identity.euid, identity.suid]
    } else {
        [identity.rgid, identity.egid, identity.sgid]
    };
    for (&ptr, word) in ptrs.iter().zip(words) {
        unsafe { (ptr as *mut u64).write_unaligned(word as u64) };
    }
    0
}

/// `setfsuid(fsuid)` — the filesystem id, which decides who a file
/// operation acts as. The man page's own observation is that the call
/// answers with the *previous* fsuid whether or not it took, so a caller
/// can probe; the answer here is the effective id it would move, because
/// the fsuid tracks the effective id and no file operation consults it -
/// modes are not enforced, so a separate word would be one more thing to
/// keep in step with nothing reading it.
pub fn handle_setfsuid(_args: [usize; ARG_COUNT]) -> isize {
    crate::sched::current_identity().euid as isize
}

/// `setfsgid(fsgid)` — [`handle_setfsuid`], spelled for groups.
pub fn handle_setfsgid(_args: [usize; ARG_COUNT]) -> isize {
    crate::sched::current_identity().egid as isize
}

/// `capget(header, data)` — the capability state.
///
/// The header carries the version, and the version decides the data's
/// shape: version 1 is the 32-bit one, a single 12-byte record; versions
/// 2 and 3 are the 64-bit ones, two records. Anything else is EINVAL, the
/// refusal a kernel older than the caller's headers makes. The state
/// reported is the process's own, out of `crate::cred`: a fresh process
/// holds every capability effective and permitted with nothing inheritable,
/// and a `capset`, a full uid drop or a `PR_CAPBSET_DROP` all move it.
pub fn handle_capget(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const CAP_V1: u32 = 0x1998_0330;
    const CAP_V3: u32 = 0x2008_0522;
    const CAP_V2: u32 = 0x2007_1026;
    let (header, data) = (args[0], args[1]);
    if header == 0 || !buffer_ok(header, 8, true, from_user) {
        return err::EFAULT;
    }
    let (version, pid) = unsafe {
        (
            (header as *const u32).read_unaligned(),
            ((header + 4) as *const i32).read_unaligned(),
        )
    };
    // The header's pid names whose capabilities are asked about; 0 is the
    // caller, and a name that is not a live task is ESRCH - the state of
    // nobody. The caller's own pid is accepted too, which is the shape a
    // libc's wrapper hands over.
    // The number is the caller's namespace's: the caller's own is recognized by
    // resolving it, and any other must name a live task *there* - which is what
    // replaces the old "the number is a slot" test.
    if pid != 0 && super::proc::pid_task(pid as isize) != Some(crate::sched::current_tgid()) {
        if super::proc::pid_task(pid as isize).is_none() {
            return err::ESRCH;
        }
    }
    let record_bytes = match version {
        CAP_V1 => 12,
        CAP_V2 | CAP_V3 => 24,
        _ => return err::EINVAL,
    };
    // The caller learns which shape the kernel filled: the header's
    // version word is rewritten with the one the answer is in.
    unsafe { (header as *mut u32).write_unaligned(version) };
    if data == 0 {
        return 0;
    }
    if !buffer_ok(data, record_bytes, true, from_user) {
        return err::EFAULT;
    }
    let creds = crate::sched::current_credentials();
    unsafe {
        let at = |offset: usize| (data + offset) as *mut u32;
        // The caller's *stored* sets, split into the 32-bit words the ABI
        // spells them in: version 1 has one word per field, the 64-bit
        // versions two. A process on this image starts holding everything, so
        // the usual answer is still all-ones - but it is now state that
        // `capset` can lower, that a full uid drop clears, and that a dropped
        // bounding capability is absent from.
        for i in 0..record_bytes / 12 {
            let shift = i * 32;
            at(i * 12).write_unaligned((creds.effective >> shift) as u32); // effective
            at(i * 12 + 4).write_unaligned((creds.permitted >> shift) as u32); // permitted
            at(i * 12 + 8).write_unaligned((creds.inheritable >> shift) as u32); // inheritable
        }
    }
    0
}

/// `capset(header, data)` — set the capability state. The header is
/// validated (a version the kernel does not know is EINVAL, a pid that is
/// not a live task EPERM), the data read, and the sets stored - which is
/// what makes `capget` a round-trip rather than a constant. A caller that
/// does not hold `CAP_SETPCAP` is held to Linux's subset rules and refused
/// `EPERM`; one that does may set anything, exactly as `cap_capset` does.
pub fn handle_capset(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const CAP_V1: u32 = 0x1998_0330;
    const CAP_V2: u32 = 0x2007_1026;
    const CAP_V3: u32 = 0x2008_0522;
    let (header, data) = (args[0], args[1]);
    if header == 0 || !buffer_ok(header, 8, false, from_user) {
        return err::EFAULT;
    }
    let (version, pid) = unsafe {
        (
            (header as *const u32).read_unaligned(),
            ((header + 4) as *const i32).read_unaligned(),
        )
    };
    if pid != 0 && super::proc::pid_task(pid as isize) != Some(crate::sched::current_tgid()) {
        return err::EPERM;
    }
    let record_bytes = match version {
        CAP_V1 => 12,
        CAP_V2 | CAP_V3 => 24,
        _ => return err::EINVAL,
    };
    if data == 0 || !buffer_ok(data, record_bytes, false, from_user) {
        return err::EFAULT;
    }
    let mut effective = 0u64;
    let mut permitted = 0u64;
    let mut inheritable = 0u64;
    unsafe {
        let at = |offset: usize| (data + offset) as *const u32;
        for i in 0..record_bytes / 12 {
            let shift = i * 32;
            effective |= (at(i * 12).read_unaligned() as u64) << shift;
            permitted |= (at(i * 12 + 4).read_unaligned() as u64) << shift;
            inheritable |= (at(i * 12 + 8).read_unaligned() as u64) << shift;
        }
    }
    // The sets are the process's real ones now, so this call is what the
    // stored state becomes - and what `capget`, an `execve` and a uid change
    // will read afterwards. `crate::cred::Credentials::capset` holds the
    // rules, including the one that matters here: Linux asks `CAP_SETPCAP`
    // first and accepts anything from a caller that holds it.
    match crate::sched::with_current_credentials(|c| c.capset(effective, permitted, inheritable)) {
        Ok(()) => 0,
        Err(()) => err::EPERM,
    }
}

// ---------------------------------------------------------------------------
// Scheduling: one policy, and the man page's priority tables
// ---------------------------------------------------------------------------

/// `sched_setparam(pid, param)` — the scheduling parameter, which for the
/// one policy this kernel implements is the zero priority it already
/// runs at. A live task is required (ESRCH for nobody), and a priority
/// other than SCHED_OTHER's zero is EINVAL - the man page's own answer
/// for a priority outside the policy's range.
pub fn handle_sched_setparam(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (pid, param) = (args[0] as isize, args[1]);
    if live_thread(pid).is_none() {
        return err::ESRCH;
    }
    if param == 0 || !buffer_ok(param, 4, false, from_user) {
        return err::EFAULT;
    }
    let priority = unsafe { (param as *const i32).read_unaligned() };
    if priority != 0 {
        return err::EINVAL;
    }
    0
}

/// `sched_getparam(pid, param)` — the priority the task runs at, which is
/// the policy's zero.
pub fn handle_sched_getparam(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (pid, param) = (args[0] as isize, args[1]);
    if live_thread(pid).is_none() {
        return err::ESRCH;
    }
    if param == 0 || !buffer_ok(param, 4, true, from_user) {
        return err::EFAULT;
    }
    unsafe { (param as *mut i32).write_unaligned(0) };
    0
}

/// `sched_setscheduler(pid, policy, param)` — the policy. `SCHED_OTHER` is
/// the one this scheduler is, and setting it is the no-op success it is on
/// Linux. The real-time policies are known but unservable - this
/// scheduler has no priority inheritance, no bands above the four it
/// schedules - and the errno for a known request no kernel beneath it can
/// keep is EOPNOTSUPP.
pub fn handle_sched_setscheduler(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SCHED_OTHER: usize = 0;
    const SCHED_FIFO: usize = 1;
    const SCHED_RR: usize = 2;
    let (pid, policy, param) = (args[0] as isize, args[1], args[2]);
    if live_thread(pid).is_none() {
        return err::ESRCH;
    }
    match policy {
        // SCHED_OTHER is this scheduler, so switching to it is the success it
        // is on Linux - but only with the priority zero the policy's band
        // allows. Measured on a real host: `sched_setscheduler(0, SCHED_OTHER,
        // &{.sched_priority = 5})` answers `EINVAL`, the same answer
        // `sched_setparam` gives for a nonzero priority. Returning `0` while
        // ignoring the param was an accept-and-drop - a caller that set a
        // priority and read `sched_getparam` to confirm it would have been
        // handed a `0` that was never actually asked for. The `EFAULT`/range
        // checks mirror `setparam` exactly.
        SCHED_OTHER => {
            if param == 0 || !buffer_ok(param, 4, false, from_user) {
                return err::EFAULT;
            }
            if unsafe { (param as *const i32).read_unaligned() } != 0 {
                return err::EINVAL;
            }
            0
        }
        // The param is still validated for the refused policies, the same
        // way Linux validates before refusing with EPERM.
        SCHED_FIFO | SCHED_RR => {
            if param != 0 && !buffer_ok(param, 4, false, from_user) {
                return err::EFAULT;
            }
            err::EOPNOTSUPP
        }
        _ => err::EINVAL,
    }
}

/// `sched_getscheduler(pid)` — the policy in force, which is SCHED_OTHER:
/// the round-robin-with-bands this kernel runs is what that name means.
pub fn handle_sched_getscheduler(args: [usize; ARG_COUNT]) -> isize {
    if live_thread(args[0] as isize).is_none() {
        return err::ESRCH;
    }
    0
}

/// `sched_get_priority_max(policy)` / `sched_get_priority_min(policy)` —
/// the priority range a policy's band allows, in the table the man page
/// spells. SCHED_OTHER and its batch and idle relatives run at the static
/// zero; the real-time policies would span 1..99, and the answer here is
/// the table's, whatever the scheduler does with it.
pub fn handle_sched_get_priority_max(args: [usize; ARG_COUNT]) -> isize {
    priority_bound(args[0], true)
}

pub fn handle_sched_get_priority_min(args: [usize; ARG_COUNT]) -> isize {
    priority_bound(args[0], false)
}

fn priority_bound(policy: usize, max: bool) -> isize {
    match policy {
        0 | 3 | 5 | 6 => 0, // SCHED_OTHER, BATCH, IDLE, DEADLINE
        1 | 2 => {
            if max {
                99
            } else {
                1
            }
        } // SCHED_FIFO, SCHED_RR
        _ => err::EINVAL,
    }
}

/// `sched_rr_get_interval(pid, tp)` — the round-robin slice, as the
/// timespec the caller filled. The scheduler's slice is one tick, the
/// 10ms the timer runs at.
pub fn handle_sched_rr_get_interval(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (pid, tp) = (args[0] as isize, args[1]);
    if live_thread(pid).is_none() {
        return err::ESRCH;
    }
    if tp == 0 || !buffer_ok(tp, 16, true, from_user) {
        return err::EFAULT;
    }
    let hz = crate::hal::TICK_HZ as u64;
    unsafe {
        (tp as *mut i64).write_unaligned(0);
        ((tp + 8) as *mut i64).write_unaligned(1_000_000_000 / hz as i64);
    }
    0
}

/// `sched_setattr(pid, attr, flags)` — the extensible scheduling
/// attributes. The struct is negotiated by size, the way `openat2`'s is:
/// smaller than the first version is EINVAL, and the version this kernel
/// knows is the 48-byte one. `SCHED_OTHER` with the zero priority and a
/// nice value inside the range is the accepted shape; the deadline
/// policy is known but unservable, and the answer is the same
/// EOPNOTSUPP the plain family gives.
pub fn handle_sched_setattr(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SCHED_ATTR_SIZE: usize = 48;
    let (pid, attr, flags) = (args[0] as isize, args[1], args[2]);
    let Some(target) = live_thread(pid) else {
        return err::ESRCH;
    };
    if flags != 0 {
        return err::EINVAL;
    }
    if attr == 0 || !buffer_ok(attr, SCHED_ATTR_SIZE, false, from_user) {
        return err::EFAULT;
    }
    let (size, policy, nice) = unsafe {
        (
            (attr as *const u32).read_unaligned(),
            ((attr + 4) as *const u32).read_unaligned(),
            ((attr + 16) as *const i32).read_unaligned(),
        )
    };
    if size < SCHED_ATTR_SIZE as u32 {
        return err::EINVAL;
    }
    match policy {
        // SCHED_OTHER: apply the nice to the same per-task slot `setpriority`
        // writes and `sched_getattr` reads, so the value is real and round-trips
        // - B5's fix for `getpriority` applied to this modern door. Returning `0`
        // while dropping the nice (as this arm used to) was the §4.2
        // success-for-a-thing-not-done. The scheduler's bands stay the policy's,
        // so the nice carries no weight effect here (the named sub-gap
        // `setpriority` shares), but the stored value is honest.
        0 if (-20..=19).contains(&nice) => {
            if crate::sched::set_nice(target, nice) {
                0
            } else {
                err::ESRCH
            }
        }
        0 => err::EINVAL,
        1..=6 => err::EOPNOTSUPP,
        _ => err::EINVAL,
    }
}

/// `sched_getattr(pid, attr, size, flags)` — the attributes in force,
/// written into the caller's struct up to the smaller of the two sizes.
/// The answer is the 48-byte first version: SCHED_OTHER, the task's real
/// nice, priority zero, no flags.
pub fn handle_sched_getattr(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SCHED_ATTR_SIZE: usize = 48;
    let (pid, attr, size, flags) = (args[0] as isize, args[1], args[2], args[3]);
    let Some(target) = live_thread(pid) else {
        return err::ESRCH;
    };
    if flags != 0 {
        return err::EINVAL;
    }
    if size < SCHED_ATTR_SIZE {
        return err::EINVAL;
    }
    if attr == 0 || !buffer_ok(attr, SCHED_ATTR_SIZE, true, from_user) {
        return err::EFAULT;
    }
    // Read the task's real nice from the same per-task slot `setpriority` and
    // `sched_setattr` write - not a hardcoded 0 - so a caller sees the value the
    // kernel is actually running the task at. A hardcoded 0 was the same
    // wrong-as-a-lie `getpriority` had before B5: it reported a number no matter
    // what had been set.
    let nice = crate::sched::nice_of(target).unwrap_or(0);
    unsafe {
        core::ptr::write_bytes(attr as *mut u8, 0, SCHED_ATTR_SIZE);
        (attr as *mut u32).write_unaligned(SCHED_ATTR_SIZE as u32);
        ((attr + 4) as *mut u32).write_unaligned(0); // policy: SCHED_OTHER
        ((attr + 16) as *mut i32).write_unaligned(nice);
    }
    0
}

/// `getpriority(which, who)` — the nice value, by process, group or user.
///
/// The raw system call answers in the kernel's own 40..1 encoding - the
/// man page's C-library/kernel differences note is explicit that the
/// `20-nice` translation is the wrapper's - so a task at nice 0 answers 20.
/// Over a set (a process group, a user's processes) Linux reports the *highest
/// priority*, i.e. the lowest nice, in the set, and `ESRCH` when the set is
/// empty. Each nice is now a real per-task value that `setpriority` records, so
/// a set-then-get round-trips instead of always reading 20.
pub fn handle_getpriority(args: [usize; ARG_COUNT]) -> isize {
    const PRIO_PROCESS: usize = 0;
    const PRIO_PGRP: usize = 1;
    const PRIO_USER: usize = 2;
    let (which, who) = (args[0], args[1]);
    // Resolve the set to its live nice values; `None`/empty means no target.
    let nices: Option<alloc::vec::Vec<i32>> = match which {
        PRIO_PROCESS => {
            let pid = if who == 0 {
                crate::sched::current().0
            } else {
                match super::proc::pid_task(who as isize) {
                    Some(id) => id.0,
                    None => return err::ESRCH,
                }
            };
            crate::sched::nice_of(pid).map(|n| alloc::vec![n])
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                crate::sched::pgid_of(crate::sched::current())
            } else {
                // A group number is the number of the task that leads it, read
                // in the caller's namespace.
                super::proc::pid_task(who as isize).and_then(crate::sched::pgid_of)
            };
            if who != 0 && pgid.is_none() {
                return err::ESRCH;
            }
            pgid.map(|g| {
                crate::sched::group_members(g)
                    .into_iter()
                    .filter_map(|id| crate::sched::nice_of(id.0))
                    .collect::<alloc::vec::Vec<i32>>()
            })
        }
        PRIO_USER => {
            let ruid = if who == 0 {
                crate::sched::current_identity().ruid
            } else {
                who as u32
            };
            let ns: alloc::vec::Vec<i32> = crate::sched::uid_members(ruid)
                .into_iter()
                .filter_map(|id| crate::sched::nice_of(id.0))
                .collect();
            if ns.is_empty() { None } else { Some(ns) }
        }
        _ => return err::EINVAL,
    };
    match nices {
        // Highest priority in the set = lowest nice; Linux's raw encoding is
        // `20 - nice`, so the lowest nice gives the largest answer.
        Some(ns) if !ns.is_empty() => 20 - *ns.iter().min().unwrap_or(&0) as isize,
        _ => err::ESRCH,
    }
}

/// `setpriority(which, who, prio)` — set the nice value.
///
/// The nice is now stored on the task (the scheduler's priority *bands* remain
/// its policy and a caller cannot move a task between them, so the value has no
/// weight effect here - a named sub-gap), but the call is no longer a
/// accept-and-drop: over `PRIO_PROCESS` it targets the caller (or the named
/// process, `ESRCH` if it is gone), and over `PRIO_PGRP`/`PRIO_USER` it applies
/// to every member, `ESRCH` when the set is empty. The `which` validation is
/// Linux's; the unprivileged-lowering rule is moot for this single-rooted
/// kernel, which accepts the whole range.
///
/// The nice is **clamped** to `-20..=19`, not refused: Linux's `setpriority`
/// clamps (`if (niceval < MIN_NICE) niceval = MIN_NICE;`, `kernel/sys.c`), and
/// its man page lists `EINVAL` only for a `which` that is none of the three.
/// Measured on a current host, where `setpriority(PRIO_PROCESS, 0, 20)` answers
/// `0` and leaves the task at `19`, and `-21` answers `EPERM` - a *privilege*
/// refusal for the raise the clamp turns it into, which a range check running
/// first would have answered `EINVAL`. This returned `EINVAL` for both, which is
/// a refusal Linux does not make, and the one program that pinned it had written
/// `EINVAL` down too (`t-priority`, migration batch 10).
pub fn handle_setpriority(args: [usize; ARG_COUNT]) -> isize {
    const PRIO_PROCESS: usize = 0;
    const PRIO_PGRP: usize = 1;
    const PRIO_USER: usize = 2;
    let (which, who, prio) = (args[0], args[1], args[2] as isize);
    if !matches!(which, PRIO_PROCESS | PRIO_PGRP | PRIO_USER) {
        return err::EINVAL;
    }
    let prio = prio.clamp(-20, 19) as i32;
    match which {
        PRIO_PROCESS => {
            let pid = if who == 0 {
                crate::sched::current().0
            } else {
                who
            };
            // `set_nice` answers `false` when the process is gone (or was never
            // there), which is Linux's `ESRCH` for `setpriority(PRIO_PROCESS,
            // pid, ..)` on a pid that does not name a task.
            if crate::sched::set_nice(pid, prio) {
                0
            } else {
                err::ESRCH
            }
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                crate::sched::pgid_of(crate::sched::current())
            } else {
                // The group's leader, named in the caller's namespace.
                super::proc::pid_task(who as isize).and_then(crate::sched::pgid_of)
            };
            if who != 0 && pgid.is_none() {
                return err::ESRCH;
            }
            let members = pgid.map(crate::sched::group_members).unwrap_or_default();
            if members.is_empty() {
                return err::ESRCH;
            }
            for id in members {
                crate::sched::set_nice(id.0, prio);
            }
            0
        }
        _ => {
            // PRIO_USER.
            let ruid = if who == 0 {
                crate::sched::current_identity().ruid
            } else {
                who as u32
            };
            let members = crate::sched::uid_members(ruid);
            if members.is_empty() {
                return err::ESRCH;
            }
            for id in members {
                crate::sched::set_nice(id.0, prio);
            }
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Memory: eager, resident, unpinned-unable - the advice family
// ---------------------------------------------------------------------------

/// `msync(addr, len, flags)` — a file mapping's pages back to the file
/// they came from. The mappings this kernel makes are private copies
/// filled at map time, so the sync is the range's own checks and the
/// success: nothing dirty exists to flush, and the EINVALs the man page
/// names — an unaligned range, SYNC beside ASYNC, an unknown bit — are
/// the whole of what a caller can find out.
pub fn handle_msync(args: [usize; ARG_COUNT]) -> isize {
    const MS_ASYNC: usize = 1;
    const MS_INVALIDATE: usize = 2;
    const MS_SYNC: usize = 4;
    let (addr, _len, flags) = (args[0], args[1], args[2]);
    if addr % PAGE_SIZE != 0 {
        return err::EINVAL;
    }
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || flags & MS_SYNC != 0 && flags & MS_ASYNC != 0
    {
        return err::EINVAL;
    }
    0
}

/// `mincore(addr, len, vec)` — which pages of a range are resident, one
/// byte each. The answer comes from the page tables, so a range that is
/// mapped but whose pages have never been touched reads as *not* resident -
/// which is the truth, and the point: with anonymous demand paging a
/// mapping is an address space, and the pages behind it appear on use. The
/// questions the caller is really asking are whether the range is mapped at
/// all (an unmapped page is `ENOMEM`, the man page's own answer) and whether
/// the vec is large enough.
pub fn handle_mincore(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (addr, len, vec) = (args[0], args[1], args[2]);
    if addr % PAGE_SIZE != 0 {
        return err::EINVAL;
    }
    let pages = len.div_ceil(PAGE_SIZE);
    if pages > 0 && (vec == 0 || !buffer_ok(vec, pages, true, from_user)) {
        return err::EFAULT;
    }
    for i in 0..pages {
        let page = addr + i * PAGE_SIZE;
        // A hole in the range is ENOMEM: nothing claims the address.
        if !crate::sched::user_mapped(page) {
            return err::ENOMEM;
        }
        // Resident means a frame is there *now*; a lazily-mapped page that has
        // not been touched is mapped and not resident, and saying so is the
        // whole of what this call is for.
        let resident = crate::sched::user_frame_at(page).is_some() as u8;
        unsafe { (vec as *mut u8).add(i).write_unaligned(resident) };
    }
    0
}

/// `mlock(addr, len)` — pin a range into memory. Nothing here pages out,
/// so every page is already pinned and the call is its validations: an
/// unaligned range is EINVAL, an unmapped page is the man page's ENOMEM,
/// and the success is the promise the man page makes and this kernel
/// keeps by never having broken it.
pub fn handle_mlock(args: [usize; ARG_COUNT]) -> isize {
    lock_range(args[0], args[1])
}

/// `munlock(addr, len)` — the unpinning half, whose validations are the
/// pinning half's: the range is named the same way, and the pages must be
/// mapped for the man page's ENOMEM to be answerable.
pub fn handle_munlock(args: [usize; ARG_COUNT]) -> isize {
    lock_range(args[0], args[1])
}

fn lock_range(addr: usize, len: usize) -> isize {
    if addr % PAGE_SIZE != 0 {
        return err::EINVAL;
    }
    let end = match addr.checked_add(len) {
        Some(end) => end,
        None => return err::EINVAL,
    };
    if len == 0 {
        return 0;
    }
    let mut page = addr;
    while page < end {
        // A page that is *claimed* counts: with demand paging the range is the
        // process's from `mmap`, and a page nobody has touched is one this
        // kernel has nothing to page out either.
        if !crate::sched::user_mapped(page) {
            return err::ENOMEM;
        }
        page += PAGE_SIZE;
    }
    0
}

/// `mlock2(addr, len, flags)` — the flag-taking form. `MLOCK_ONFAULT`
/// promises to pin the pages a later fault brings in, which is a promise
/// about a page-out this kernel never performs; it is accepted, and any
/// other bit is the man page's EINVAL.
pub fn handle_mlock2(args: [usize; ARG_COUNT]) -> isize {
    const MCL_ONFAULT: usize = 0x10;
    if args[2] & !MCL_ONFAULT != 0 {
        return err::EINVAL;
    }
    lock_range(args[0], args[1])
}

/// `mlockall(flags)` — pin the whole address space. Everything is
/// already resident; the flags are validated because the man page's
/// EINVAL for an unknown bit is the caller's only way to learn the
/// kernel did not hear the rest.
pub fn handle_mlockall(args: [usize; ARG_COUNT]) -> isize {
    const MCL_CURRENT: usize = 1;
    const MCL_FUTURE: usize = 2;
    if args[0] & !(MCL_CURRENT | MCL_FUTURE) != 0 {
        return err::EINVAL;
    }
    0
}

/// `munlockall()` — nothing was pinned that was not already resident, so
/// nothing needs the unpinning.
pub fn handle_munlockall(_args: [usize; ARG_COUNT]) -> isize {
    0
}

// ---------------------------------------------------------------------------
// The machine: its persona, its processors, its clock, its namespaces
// ---------------------------------------------------------------------------

/// `personality(persona)` — the process's execution-environment word.
/// This kernel has exactly one persona — the default one, `PER_LINUX` —
/// and no ASLR to toggle or emulation layout to switch, so the query
/// (`0xffffffff`) answers 0 and a set of the default answers with the
/// previous one; anything else is the man page's EINVAL, the refusal for
/// a persona this kernel does not have.
pub fn handle_personality(args: [usize; ARG_COUNT]) -> isize {
    match args[0] {
        0xffff_ffff => 0, // the query: the persona in force is the default
        0 => 0,           // the default itself: already in force
        _ => err::EINVAL,
    }
}

/// `vhangup()` — virtually hang up the controlling terminal.
///
/// Linux hangs up `current->signal->tty` and answers `0`; when the caller has
/// no controlling terminal, `SYSCALL_DEFINE0(vhangup)` answers `-ENOTTY` — there
/// is no terminal to hang up, and the success a caller who has none is not owed.
/// That is the case a daemonised or headless caller (and every `t-*` program the
/// self-test runs, none of which has acquired a terminal with `TIOCSCTTY`)
/// reaches, so it is answered with the honest `ENOTTY` rather than a `0` that
/// claims a hang-up this call never performed.
///
/// When a controlling terminal *does* exist this answers `EOPNOTSUPP`, not the
/// `0` Linux would give after hanging the terminal up: the kernel keeps the
/// controlling-terminal bookkeeping (`TIOCSCTTY` records one, `Task::ctty` holds
/// it) but has no tty-layer operation that re-initialises the terminal and turns
/// already-open descriptors into `EIO`. Returning the success anyway would be
/// the §4.2 "silently did the thing you didn't do" this batch exists to remove;
/// a refusal tells a logout/getty the truth - it then tears the terminal down by
/// closing its own descriptors, which is what it does next regardless. The
/// hang-up itself is `NOTES.md` §12.2 B4, a tty-layer feature. This branch is
/// reached only by a caller that has acquired a terminal, which no headless
/// self-test program does, so it is not gate-exercised - but a fake `0` is not
/// made honest by being untested.
pub fn handle_vhangup(_args: [usize; ARG_COUNT]) -> isize {
    // The capability gate Linux puts first: without `CAP_SYS_TTY_CONFIG` the
    // answer is `EPERM`, reached before the no-terminal `ENOTTY` the man page
    // spends its words on (host-measured, `NOTES.md` §15.11 batch 32).
    if !crate::sched::current_credentials().has_effective(crate::cred::CAP_SYS_TTY_CONFIG) {
        return err::EPERM;
    }
    match crate::sched::controlling_tty_of_current() {
        None => err::ENOTTY,
        Some(_) => err::EOPNOTSUPP,
    }
}

/// `getcpu(cpu, node, tcache)` — which processor and NUMA node the caller
/// runs on. One core, node zero, and the tcache the man page retires is
/// accepted and forgotten.
pub fn handle_getcpu(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (cpu, node) = (args[0], args[1]);
    for ptr in [cpu, node] {
        if ptr != 0 && !buffer_ok(ptr, 4, true, from_user) {
            return err::EFAULT;
        }
    }
    for ptr in [cpu, node] {
        if ptr != 0 {
            unsafe { (ptr as *mut u32).write_unaligned(0) };
        }
    }
    0
}

/// Move the wall clock's anchor so it reads `epoch` now: the tick
/// counter keeps running, the epoch it is measured from moves, and the
/// timers that asked to hear about a set are told.
fn set_wall_clock(epoch: u64) {
    let hz = crate::hal::TICK_HZ as u64;
    let anchor = epoch.saturating_sub(crate::hal::ticks() / hz);
    crate::hal::set_boot_epoch(anchor);
    crate::vfs::timerfd::cancel_on_clock_set();
}

/// `settimeofday(tv, tz)` — set the wall clock. The validation is the man
/// page's own — a negative `tv_sec`, a `tv_usec` outside a second's
/// worth, is EINVAL — and the timezone pointer is accepted and not acted
/// on, which is what Linux itself does with it.
pub fn handle_settimeofday(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (tv, _tz) = (args[0], args[1]);
    if tv == 0 {
        return 0;
    }
    if !buffer_ok(tv, 16, false, from_user) {
        return err::EFAULT;
    }
    let (sec, usec) = unsafe {
        (
            (tv as *const i64).read_unaligned(),
            ((tv + 8) as *const i64).read_unaligned(),
        )
    };
    if sec < 0 || !(0..1_000_000).contains(&usec) {
        return err::EINVAL;
    }
    set_wall_clock(sec as u64);
    0
}

/// `clock_settime(clk_id, tp)` — the clock-setting spelling, where only
/// the wall clock family is settable: the man page's EINVAL for a clock
/// that cannot be set is what every monotonic name gets, and the two
/// realtime ones land on the same anchor `settimeofday` moves.
pub fn handle_clock_settime(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const CLOCK_REALTIME: usize = 0;
    const CLOCK_REALTIME_ALARM: usize = 8;
    let (clk, tp) = (args[0], args[1]);
    if !matches!(clk, CLOCK_REALTIME | CLOCK_REALTIME_ALARM) {
        return err::EINVAL;
    }
    if tp == 0 || !buffer_ok(tp, 16, false, from_user) {
        return err::EFAULT;
    }
    let (sec, nsec) = unsafe {
        (
            (tp as *const i64).read_unaligned(),
            ((tp + 8) as *const i64).read_unaligned(),
        )
    };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return err::EINVAL;
    }
    set_wall_clock(sec as u64);
    0
}

/// `unshare(flags)` — leave namespaces.
///
/// **Five kinds are served**, each with its own pin: the names
/// (`CLONE_NEWUTS`, whose whole effect is that the caller gets a *copy* of what
/// it was reading - `t-ns-uts` holds both halves, that the child's view differs
/// *and* that the parent's does not), the mount table (`CLONE_NEWNS`,
/// `t-ns-mnt`, the same pair), the **numbers** (`CLONE_NEWPID`, where the caller
/// keeps its own and its *children* move - `t-ns-pid`), the **wire**
/// (`CLONE_NEWNET`, where the caller itself moves and its open sockets come
/// along - `t-ns-net`) and the **clocks** (`CLONE_NEWTIME`, the second kind
/// whose only moved slot is the children's - `t-ns-time`). The kinds still
/// answering `EOPNOTSUPP` are USER, IPC and CGROUP, because a namespace with
/// nothing behind it isolates nothing; each one's reason and the order they are
/// built in are the plan's §6.3.
///
/// The flag word itself is argued with in `abi::unshare_verdict`, which is where
/// the three refusals live: an unrecognised bit is `EINVAL` *ahead* of
/// everything (measured — `CLONE_PIDFD|CLONE_NEWNS` is `EINVAL` on a real
/// kernel, not the namespace's `EPERM`); an unserved namespace is
/// `EOPNOTSUPP` **for the whole word**, so nothing is unshared; and a served one
/// asked for without `CAP_SYS_ADMIN` is `EPERM`. The whole-word refusal is a
/// deliberate divergence from Linux's `ksys_unshare`, which unshares as it walks
/// the word and can leave a caller halfway with nothing saying so.
///
/// The thread-group flags are Linux's now, measured rather than recalled
/// (`.workbuddy/probe_unshare_flags.c`, host 7.2.9, one child per flag). For a
/// caller with no other thread to split from all six answer `0`, and two of them
/// *split* the caller's slot: `CLONE_FS` copies the process's filesystem context
/// (Linux's `copy_fs_struct`, so a `chdir` from here moves only this task) and
/// `CLONE_FILES` copies the descriptor table (Linux's `copy_files`, so a `close`
/// here no longer closes the number for a sibling). `CLONE_SYSVSEM` is accepted
/// and is nothing to do - there is no SysV semaphore undo list here to split.
/// The remaining three are refused `EINVAL` as soon as a sibling exists, which
/// is Linux's own answer and the reason it gives: the address space or the
/// handler table really is shared, so "unshare it for me alone" is not a request
/// the call can honour. That refusal is decided in `abi::unshare_verdict`, ahead
/// of the namespace work, as Linux's `check_unshare_flags` is.
///
/// One more gap rides with the call rather than the namespace:
/// `clone(CLONE_NEWUTS)` — the *child* getting the copy — is still `EINVAL` from
/// `validate_clone_flags`, which refuses namespace bits wholesale. Serving it is
/// a separate decision (where in `copy_process`'s order the copy happens, and
/// what `CLONE_NEWUTS|CLONE_THREAD` means for a group that shares an address
/// space) and it wants its own measurement, so it is named here rather than
/// half-built.
pub fn handle_unshare(args: [usize; ARG_COUNT]) -> isize {
    // `CAP_SYS_ADMIN` is the gate on every privileged namespace kind, asked
    // of the caller's *effective capability set* - the question the credential
    // model exists to answer, not the crude `euid == 0` it replaced (a caller
    // that has dropped the capability is refused like Linux's, whatever its
    // identity says).
    let privileged = crate::sched::current_credentials().has_effective(crate::cred::CAP_SYS_ADMIN);
    // The sibling fact is read here rather than in the leaf: it is scheduler
    // state, and the verdict is a pure function of it (which is how the host
    // tests argue with all four refusals without a task table).
    let has_siblings = crate::sched::thread_group_has_siblings();
    match super::abi::unshare_verdict(args[0], privileged, has_siblings) {
        Err(e) => e,
        Ok(bits) => {
            // The one kind with a precondition, and it is asked **first**:
            // Linux's `copy_pid_ns` refuses the whole word rather than moving
            // part of it, so a caller that has already made a pid namespace
            // hears `EINVAL` and finds every other bit of its word unmoved too.
            // The rule itself and its measurement are
            // [`crate::sched::may_mint_pid_ns`]'s.
            if bits & crate::syscall::linux::nr::CLONE_NEWPID != 0
                && !crate::sched::may_mint_pid_ns()
            {
                return crate::err::EINVAL;
            }
            if bits & crate::syscall::linux::nr::CLONE_NEWUTS != 0 {
                crate::sched::unshare_uts();
            }
            if bits & crate::syscall::linux::nr::CLONE_NEWNS != 0 {
                // The caller moves to a copy of the table it was resolving
                // through (Linux's `copy_mnt_ns` → `unshare_mnt_ns`): what it
                // mounts or unmounts from here is visible to its own future
                // children only, never to the namespace it came from. Its
                // siblings keep the old table — the same per-task rule as UTS
                // above, and `t-ns-mnt` holds both halves of the divergence.
                crate::sched::unshare_mnt();
            }
            if bits & crate::syscall::linux::nr::CLONE_NEWPID != 0 {
                // The caller's *children* move into the new namespace: the
                // first of them will be 1 there, and the caller keeps the
                // number it had. See `sched::unshare_pid` for why the two are
                // different fields.
                crate::sched::unshare_pid();
            }
            if bits & crate::syscall::linux::nr::CLONE_NEWNET != 0 {
                // The caller **moves** into a fresh namespace, which is Linux's
                // rule for this kind and the opposite of PID's: its address and
                // route tables, its port space and its *future* sockets are the
                // new stack's. Its existing sockets come with it, because each
                // one remembers the stack it was made in (`net`'s `HOMES`) - so a
                // connection opened before the call goes on working.
                crate::sched::set_net_ns(Some(crate::net::mint_namespace()));
            }
            if bits & crate::syscall::linux::nr::CLONE_NEWTIME != 0 {
                // The caller's *children* move into a fresh, empty namespace,
                // and the caller does **not** - the same asymmetry PID above
                // has, and the same field pair carries it. The offsets are not
                // set here: they are written to `/proc/self/timens_offsets`
                // afterwards, which is the only moment they can be (the first
                // task in the namespace freezes them), and which is why a
                // caller's sequence is unshare, write, fork.
                crate::sched::unshare_time();
            }
            if bits & crate::syscall::linux::nr::CLONE_FS != 0 {
                crate::sched::unshare_fs();
            }
            if bits & crate::syscall::linux::nr::CLONE_FILES != 0 {
                crate::sched::unshare_files();
            }
            0
        }
    }
}

/// `setns(fd, nstype)` — join a namespace a descriptor names.
///
/// A namespace is named by a descriptor now, because `/proc/<pid>/ns/<kind>` is
/// openable: the link's own inode carries the namespace and this call joins
/// *that object* rather than copying the caller's, so the caller and every
/// process already in the namespace see each other's mounts and names from here
/// on. That is the whole difference from `unshare`, which copies.
///
/// The order of the questions is the one a real kernel answers in, measured
/// (the probe is `.workbuddy/probe_setns.c`, host 7.2.9):
///
/// 1. a number that names no descriptor is `EBADF`;
/// 2. a descriptor that is not a namespace — a pipe, a plain file, a directory
///    — is `EINVAL`, which is Linux's "not a namespace" answer (`-22` for each
///    of the three);
/// 3. the flag word is checked *next*, and everything but zero or exactly this
///    descriptor's kind is `EINVAL`: a sibling's bit, two bits at once and a
///    word that is no namespace bit all answer `-22` (see
///    `abi::setns_verdict` for the four measurements);
/// 4. **then** the capability. The probe was an unprivileged caller, and its
///    call with the *matching* bit was refused `EPERM` while the mismatched
///    ones were refused `EINVAL` — which is what fixes this order rather than
///    the other one. `CAP_SYS_ADMIN` is asked of the caller's *effective set*,
///    the same gate and the same question `unshare` asks.
///
/// A kind this kernel does not have is not reachable at all: `/proc/<pid>/ns`
/// has no entry for it, so no descriptor for one can exist and no branch here
/// has to answer for it. That absence *is* the refusal, and it is the same one a
/// real kernel gives a machine built without the namespace.
pub fn handle_setns(args: [usize; ARG_COUNT]) -> isize {
    let file = match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => file,
        Err(_) => return err::EBADF,
    };
    let Some(namespace) = file.inode().as_namespace() else {
        return err::EINVAL;
    };
    if let Err(e) = super::abi::setns_verdict(namespace.kind(), args[1]) {
        return e;
    }
    let privileged = crate::sched::current_credentials().has_effective(crate::cred::CAP_SYS_ADMIN);
    if !privileged {
        return err::EPERM;
    }
    match namespace {
        crate::vfs::ns::Namespace::Mount(table) => crate::sched::setns_mnt(table),
        // Joining a namespace: the caller moves into *that* stack, and its
        // existing sockets keep working against the ones they were made in -
        // which is what `HOMES` buys, and why this is a join by reference
        // (Linux's `switch_task_namespaces`) rather than a copy.
        crate::vfs::ns::Namespace::Net(id) => crate::sched::set_net_ns(Some(id)),
        crate::vfs::ns::Namespace::Pid(pidns) => crate::sched::setns_pid(pidns),
        crate::vfs::ns::Namespace::Uts(uts) => crate::sched::setns_uts(uts),
        // Joining a time namespace replaces the caller's `time_ns_for_children`
        // and nothing else: the caller goes on reading the clocks it was reading
        // and its next child wakes up in the namespace the descriptor named.
        // Measured - the same shape as `unshare` for this kind, and the one
        // asymmetry `setns` has against `/proc/<pid>/ns/pid`, where the
        // descriptor answers the *own* slot instead.
        crate::vfs::ns::Namespace::Time(ns) => crate::sched::setns_time(ns),
    }
    0
}

/// `seccomp(op, flags, args)` — the secure-computing mode. Strict mode is
/// honoured: it latches the thread to the four-call allow-list (`read`,
/// `write`, `exit`, `sigreturn`) enforced at dispatch, irreversible for the
/// thread's life. Filter mode installs a classic-BPF program
/// (`SECCOMP_SET_MODE_FILTER`), whose evaluation and eight actions are
/// `src/seccomp.rs` and the gate above.
///
/// `SECCOMP_GET_ACTION_AVAIL` answers **the truth about which of those eight
/// this kernel implements**, which is the whole point of the call existing: it
/// is asked before a filter relies on an action, precisely because Linux kills
/// the process on one it does not know. An "is this available?" that answered
/// no for everything was a request not to be asked, and a program told no falls
/// back; the risk was never answering no, it was answering yes.
pub fn handle_seccomp(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SET_MODE_STRICT: usize = 0;
    const SET_MODE_FILTER: usize = 1;
    const GET_ACTION_AVAIL: usize = 2;
    match args[0] {
        // Strict mode is the whole filter Linux started with: the thread may
        // make only `read`, `write`, `exit` and `sigreturn` from here on and
        // dies on anything else. It takes no flags and no filter argument, so
        // anything else in those slots is a caller mistake, and once latched it
        // cannot be cleared - it survives exec and is inherited across fork.
        SET_MODE_STRICT => {
            if args[1] != 0 || args[2] != 0 {
                return err::EINVAL;
            }
            crate::sched::set_seccomp_strict();
            0
        }
        SET_MODE_FILTER => super::abi::install_seccomp_filter(args[2], args[1], from_user),
        // The three answers the probe has, all measured: `EOPNOTSUPP` for an
        // action this kernel does not implement (Linux's own refusal, and not
        // `EINVAL`, which is reserved for an operand that is wrong), `EINVAL`
        // for a `flags` it would have to ignore, and `EFAULT` for a pointer it
        // cannot read. The order is the operands' own order - a caller that got
        // both wrong hears about the flag, which is the answer it can act on.
        GET_ACTION_AVAIL => {
            if args[1] != 0 {
                return err::EINVAL;
            }
            // `write = false`: this is a read of the caller's word. The pointer
            // must be readable rather than writable, and asking for write
            // permission here would fail a caller that put its action in
            // read-only memory - which is what `const`-declared data is.
            if args[2] == 0 || !buffer_ok(args[2], 4, false, from_user) {
                return err::EFAULT;
            }
            let asked = unsafe { (args[2] as *const u32).read_unaligned() };
            if !crate::seccomp::supported(asked) {
                return err::EOPNOTSUPP;
            }
            // Linux leaves the caller's word alone - the return value is what
            // carries the answer, and there is nothing useful to write back.
            0
        }
        _ => err::EINVAL,
    }
}

/// `setdomainname(name, len)` — name the domain. The storage is the hostname's
/// twin: one fact, one home, so the call that sets it and the readers that
/// report it cannot disagree. The length bound is the `utsname` field's, as
/// `sethostname`'s is.
///
/// Like the hostname it names the running task's *namespace*, so a caller that
/// unshared first names only its own. And like the hostname it used to be
/// stored somewhere nothing read: `uname` reported the literal `(none)` and
/// `/proc/sys/kernel/domainname` hardcoded the same string while dropping every
/// write, so this call had no observable effect through any door. Both readers
/// go through the namespace now (the plan's §6.3 has the whole account).
pub fn handle_setdomainname(args: [usize; ARG_COUNT]) -> isize {
    let (ptr, len) = (args[0], args[1]);
    // The same `CAP_SYS_ADMIN` gate, in the same place: measured on a real
    // kernel as a non-root caller, `setdomainname(NULL, 100)` answers `EPERM`,
    // so the capability is decided before the length and the pointer just as it
    // is for `sethostname`.
    if crate::sched::current_identity().euid != 0 {
        return err::EPERM;
    }
    if len > 64 {
        return err::EINVAL;
    }
    let mut bytes = [0u8; 64];
    if !crate::sched::read_user(ptr, &mut bytes[..len]) {
        return err::EFAULT;
    }
    let Ok(name) = core::str::from_utf8(&bytes[..len]) else {
        return err::EINVAL;
    };
    crate::sched::current_uts().set_domainname(name);
    0
}

// ---------------------------------------------------------------------------
// The signals a process aims at itself or one thread of another
// ---------------------------------------------------------------------------

/// `rt_sigpending(set, sigsetsize)` — which signals have arrived and are
/// waiting, as a mask. The pending set is not the blocked set: a signal can
/// be both (arrived, and held back from delivery), and the man page's
/// contract is that this answers about the arrivals alone.
///
/// The answer goes out in the wire's `sigset_t` (signal *n* in bit *n-1*), the
/// same conversion `rt_sigprocmask` takes its sets in through: the kernel's own
/// arithmetic names signal *n* by bit *n*, and a caller reading the kernel's
/// spelling would see every signal one place off.
pub fn handle_rt_sigpending(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (set, sigsetsize) = (args[0], args[1]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    if set == 0 || !buffer_ok(set, 8, true, from_user) {
        return err::EFAULT;
    }
    let pending = crate::sched::signal::to_wire(crate::sched::current_pending());
    unsafe { (set as *mut u64).write_unaligned(pending) };
    0
}

/// `tgkill(tgid, tid, sig)` — a signal to one thread, named by its group
/// and itself. Both names are checked, in the man page's order: a thread
/// that does not exist is ESRCH, and a live thread whose group is not
/// the one named is ESRCH too — the caller named a process the thread
/// does not belong to.
pub fn handle_tgkill(args: [usize; ARG_COUNT]) -> isize {
    let (tgid, tid, sig) = (args[0] as isize, args[1] as isize, args[2]);
    if sig > crate::sched::signal::MAX_SIGNAL as usize {
        return err::EINVAL;
    }
    // Both names are read in the caller's namespace: the thread's own number
    // and the group it must belong to. Resolving each first is what makes a
    // number from another namespace `ESRCH` rather than a signal to whichever
    // task wears that slot.
    let (Some(tid), Some(tgid)) = (super::proc::pid_task(tid), super::proc::pid_task(tgid)) else {
        return err::ESRCH;
    };
    let Some(snap) = crate::sched::snapshot(tid.0) else {
        return err::ESRCH;
    };
    if snap.pid != tgid.0 {
        return err::ESRCH;
    }
    if crate::sched::send_signal(tid, sig as u8) {
        0
    } else {
        err::ESRCH
    }
}

/// `rt_sigqueueinfo(tgid, sig, uinfo)` — `kill` with a `siginfo_t`
/// attached. The rules are the man page's: the siginfo must be readable,
/// and a caller signalling *another* process may not forge the codes the
/// kernel itself uses — `si_code` at or above zero is SI_USER's ground,
/// and sending that to somebody else is EPERM. An `SI_QUEUE` record carries
/// its payload's `si_value` word to the target's handler; any other code
/// arrives as the kernel's own SI_USER record.
pub fn handle_rt_sigqueueinfo(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SIGINFO_BYTES: usize = 128;
    let (tgid, sig, uinfo) = (args[0] as isize, args[1], args[2]);
    if sig > crate::sched::signal::MAX_SIGNAL as usize {
        return err::EINVAL;
    }
    if uinfo == 0 || !buffer_ok(uinfo, SIGINFO_BYTES, false, from_user) {
        return err::EFAULT;
    }
    let si_code = unsafe { ((uinfo + 8) as *const i32).read_unaligned() };
    let target = super::proc::pid_task(tgid);
    if target != Some(crate::sched::current_tgid()) && si_code >= 0 {
        return err::EPERM;
    }
    let Some(target) = target else {
        return err::ESRCH;
    };
    // The sender, as the record names it: its *own* number, which is what Linux
    // writes (`task_tgid_vnr(current)`) and what a receiver in the same
    // namespace reads back as the sender's `getpid`.
    let (pid, uid) = {
        let cur = crate::sched::current_identity();
        (crate::sched::current_pid_number() as i32, cur.ruid as i32)
    };
    // `union sigval` sits at offset 24 of `siginfo_t` on x86_64; an `SI_QUEUE`
    // sender put its word there. The buffer was validated readable above, so
    // the direct read matches the `si_code` read just before it.
    let info = if si_code == crate::sched::signal::SI_QUEUE {
        let value = unsafe { ((uinfo + 24) as *const u64).read_unaligned() };
        crate::sched::signal::SigInfo::queue(sig as u8, pid, uid, value)
    } else {
        crate::sched::signal::SigInfo::user(sig as u8, pid, uid)
    };
    if crate::sched::send_signal_info(target, sig as u8, info) {
        0
    } else {
        err::ESRCH
    }
}

/// `rt_sigsuspend(mask, sigsetsize)` — swap the signal mask and wait for
/// a signal the new mask lets through.
///
/// The wait is `block_until_deadline`'s: a signal that would run a
/// handler ends it as `Interrupted`, without consuming the signal, so
/// the gate delivers the handler on the way out of this very call —
/// which is the man page's contract, that the handler runs *before* the
/// EINTR the call answers with. The old mask goes back either way, as
/// the mask a handler's `rt_sigreturn` restores is the frame's own copy.
pub fn handle_rt_sigsuspend(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (mask_ptr, sigsetsize) = (args[0], args[1]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    if mask_ptr == 0 || !buffer_ok(mask_ptr, 8, false, from_user) {
        return err::EFAULT;
    }
    let wanted =
        crate::sched::signal::from_wire(unsafe { (mask_ptr as *const u64).read_unaligned() });
    // The two the kernel keeps for itself cannot be waited *in* either.
    let reserved = crate::sched::signal::bit(crate::sched::signal::SIGKILL)
        | crate::sched::signal::bit(crate::sched::signal::SIGSTOP);
    let old = crate::sched::current_blocked();
    crate::sched::set_current_blocked(wanted & !reserved);

    let answer = crate::sched::block_until_deadline(|| false, u64::MAX);
    crate::sched::set_current_blocked(old);
    match answer {
        WaitEnd::Interrupted => err::EINTR,
        _ => err::EINTR,
    }
}

/// `pause()` — wait for a signal that would run a handler. The only way
/// out is one: `block_until_deadline`'s condition never holds and its
/// clock never runs out, so the `Interrupted` the waiter ends on is the
/// signal's own arrival, and the EINTR the call answers with is the one
/// the man page says a caught signal turns into.
pub fn handle_pause() -> isize {
    loop {
        match crate::sched::block_until_deadline(|| false, u64::MAX) {
            WaitEnd::Interrupted => return err::EINTR,
            _ => continue,
        }
    }
}
