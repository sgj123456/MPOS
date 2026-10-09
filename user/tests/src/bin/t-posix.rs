//! t-posix - the POSIX completion set's process half, on the machine.
//!
//! Pinned here, per each call's man page: a fresh process is root in all
//! three identity words, and the `setuid` family follows Linux's rules -
//! root may set any id (the dance `sudo` performs: lower the effective id,
//! raise it back through the saved one), while a caller that has lowered
//! itself may only reach ids it already holds and sees EPERM for the rest;
//! `setreuid`'s saved id follows the new effective one for root and never
//! moves for anyone else, root's capability state is everything permitted and
//! nothing inheritable, `getpriority`'s raw syscall answers in the kernel's
//! own 40..1 encoding (nice 0 reads as 20), the one policy is SCHED_OTHER
//! with the priority bounds the man page's table spells, the persona query
//! answers the default, seccomp's filter mode refuses a request that names no
//! program before it reads one, while its availability probe answers, and the
//! namespace pair refuses with EOPNOTSUPP/EINVAL for the flags it cannot
//! keep, and the memory advice family answers for a range that is mapped.
//!
//! # The migration, and what the host's probe measured
//!
//! The doors split the way the claims split. Everything whose answer is the
//! value itself travels through `libc` (the `setuid` family, `sched_*`,
//! `unshare`, `setns`, `tgkill`, `vhangup`, `personality`, the memory advice
//! family); what a wrapper would translate travels raw (`getpriority`/
//! `setpriority`, whose glibc spelling adds 20 to the kernel encoding this
//! program pins; `capget`/`capset`, the two-record v3 layout written straight
//! into the syscall; `seccomp`, which musl does not wrap). The constants are
//! `libc::CAP_LAST_CAP` and `libc::SIG*`, and the mmap'd page is
//! `rustix::mm::mmap_anonymous`.
//!
//! **The probe (`.workbuddy/probe_posix.c`, `probe_seccomp_mode.c`)
//! measured four divergences, each one a gate or a service this kernel does
//! not have.** The host's `getcpu` answers the core it really ran on (12 in
//! the probe) where this single-core kernel pins 0; `personality(0x1000)`
//! the host accepts (Linux stores any word) where this kernel validates the
//! persona table; `vhangup` the host answers `EPERM` for - the container's
//! root lacks `CAP_SYS_TTY_CONFIG`, and the privilege check runs before the
//! no-terminal `ENOTTY` this kernel pins; and `seccomp(STRICT)` the host
//! refuses outright with `EFAULT`, flags or no flags, so a strict child
//! there exits early and the death-by-`SIGKILL` claim is named as
//! unmeasurable rather than false. `unshare(CLONE_NEWNS|CLONE_NEWTIME)` the
//! host *serves* to root (real namespaces both) where this kernel answers
//! its deliberate `EOPNOTSUPP` - the plan's refuse-rather-than-approximate
//! rule - and the strict-mode stray-flags `EINVAL` meets the host's
//! `EFAULT` from the same refusal. Everything else the probe measured
//! agrees: the identity dance, the capability words (`ffffffff`/`0x1ff`,
//! nothing inheritable, the round-trip through `capset`), the priority
//! encoding and its clamp, the whole scheduler table, `GET_ACTION_AVAIL`'s
//! `EOPNOTSUPP` with the caller's word untouched, the namespace refusals,
//! `setns`, `tgkill`'s probes, and every answer of the memory advice family
//! including the freshly-mapped-not-resident pin.

use mpos_user_tests::{Report, host_run};
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};

/// The raw door, three arguments: the syscall's own answer, `-errno` on
/// failure.
fn sys3(nr: libc::c_long, a: usize, b: usize, c: usize) -> isize {
    // SAFETY: every caller passes either numbers or pointers into this
    // program's own memory, which outlives the call.
    let r = unsafe { libc::syscall(nr, a as libc::c_long, b as libc::c_long, c as libc::c_long) };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

fn sys2(nr: libc::c_long, a: usize, b: usize) -> isize {
    sys3(nr, a, b, 0)
}

/// `libc`'s error protocol on this target (musl) is a plain `-1` with the
/// errno beside it, not glibc's negative errno - so every wrapped call whose
/// errno a claim pins goes through this, which turns `-1` into `-errno` and
/// leaves every other answer alone.
fn rc(rc: libc::c_int) -> libc::c_int {
    if rc == -1 {
        -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        rc
    }
}

/// The scheduler doors, raw: this target's `libc::sched_getscheduler`
/// wrapper answers `ENOSYS` where the raw door succeeds (measured,
/// `.workbuddy/diag.rs`), and the claims pin the kernel's answers.
fn sched_getscheduler(pid: libc::c_int) -> isize {
    sys2(libc::SYS_sched_getscheduler, pid as usize, 0)
}

fn sched_getparam(pid: libc::c_int, param: *mut libc::sched_param) -> isize {
    sys2(libc::SYS_sched_getparam, pid as usize, param as usize)
}

fn sched_setscheduler(
    pid: libc::c_int,
    policy: libc::c_int,
    param: *const libc::sched_param,
) -> isize {
    sys3(
        libc::SYS_sched_setscheduler,
        pid as usize,
        policy as usize,
        param as usize,
    )
}

fn sched_setparam(pid: libc::c_int, param: *const libc::sched_param) -> isize {
    sys2(libc::SYS_sched_setparam, pid as usize, param as usize)
}

/// One `__user_cap_data_struct`: effective, permitted and inheritable, one
/// `u32` each - which is how `<linux/capability.h>` spells it, and the order
/// the kernel writes them in.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapRecord {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The version 3 data: *two* of those records, laid end to end. A struct of
/// three two-word fields would describe a different layout entirely - one the
/// kernel does not write - and would read the first record's inheritable
/// word as the second record's effective one.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    records: [CapRecord; 2],
}

impl CapData {
    fn every(&self, field: fn(&CapRecord) -> u32, want: u32) -> bool {
        self.records.iter().all(|record| field(record) == want)
    }
}

/// The v3 header: the version word and the pid to ask about.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

/// The uapi's `CAP_LAST_CAP` (`<uapi/linux/capability.h>`: 41 capabilities,
/// numbered 0..=40). `libc` does not name it on this target, and the value is
/// not this kernel's to choose - the probe's `0x1ff` high word pins it from
/// the host's own `CapEff` too.
const CAP_LAST_CAP: u32 = 40;

/// `tgkill(tgid, tid, sig)` through the raw door: `libc` does not wrap it on
/// this target.
fn tgkill(tgid: libc::pid_t, tid: libc::pid_t, sig: libc::c_int) -> libc::c_int {
    // SAFETY: numbers only; sig 0 is a probe that touches nothing.
    let r = unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            tgid as libc::c_long,
            tid as libc::c_long,
            sig as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as libc::c_int)
    } else {
        r as libc::c_int
    }
}

fn main() {
    println!("t-posix");
    let mut r = Report::new();

    // Identity: a fresh process is root three words over - real, effective
    // and saved, user and group alike.
    let mut ids = [1u32; 3];
    r.check(
        "getresuid succeeds",
        unsafe { libc::getresuid(&mut ids[0], &mut ids[1], &mut ids[2]) } == 0,
    );
    r.check(
        "the real, effective and saved uids are root's",
        ids == [0, 0, 0],
    );
    let mut gids = [1u32; 3];
    r.check(
        "getresgid succeeds",
        unsafe { libc::getresgid(&mut gids[0], &mut gids[1], &mut gids[2]) } == 0,
    );
    r.check("the gids are root's", gids == [0, 0, 0]);

    // The res spelling, as the privilege dance sudo actually performs: root
    // lowers its effective id, reads its own three words back, and raises
    // itself again through the saved one - the word that never moved. The
    // dance itself, and everything that follows from it: the host's kernel
    // answers `EINVAL` for this exact shape (raw and glibc alike,
    // `.workbuddy/probe_diag.c`) where this kernel lowers the effective id -
    // so on a host run the dance is named and not made, and the claims that
    // depend on being lowered are skipped with it.
    let lowered = rc(unsafe { libc::setresuid(u32::MAX, 1, u32::MAX) });
    r.check(
        "setresuid(-1, 1, -1) succeeds for root",
        lowered == 0 || host_run() && lowered == -(libc::EINVAL as libc::c_int),
    );
    if lowered != 0 && !host_run() {
        r.finish();
    }
    if lowered == 0 {
        r.check(
            "getresuid still succeeds",
            unsafe { libc::getresuid(&mut ids[0], &mut ids[1], &mut ids[2]) } == 0,
        );
        r.check(
            "the dance reads back real 0, effective 1, saved 0",
            ids == [0, 1, 0],
        );
        r.check(
            "geteuid reads the lowered id",
            unsafe { libc::geteuid() } == 1,
        );
        r.check(
            "getuid still reads the real one",
            unsafe { libc::getuid() } == 0,
        );
        r.check(
            "a lowered caller cannot set an id it does not hold",
            rc(unsafe { libc::setresuid(2, u32::MAX, u32::MAX) }) == -(libc::EPERM as libc::c_int),
        );
        r.check(
            "the way back through the saved id succeeds",
            unsafe { libc::setresuid(u32::MAX, 0, u32::MAX) } == 0,
        );
        r.check("geteuid reads root again", unsafe { libc::geteuid() } == 0);
    } else {
        println!(
            "note: the host's setresuid(-1, 1, -1) answers EINVAL (probe_diag.c); the dance and the claims that depend on it are named and not made on a host run"
        );
    }

    // The re spelling: for root the saved id follows the new effective one,
    // and becoming what one already is always succeeds.
    r.check(
        "setreuid(0, 0) succeeds",
        unsafe { libc::setreuid(0, 0) } == 0,
    );
    r.check(
        "setreuid(-1, 0) succeeds",
        unsafe { libc::setreuid(u32::MAX, 0) } == 0,
    );
    if lowered == 0 {
        r.check(
            "setreuid(0, 100) succeeds for root",
            unsafe { libc::setreuid(0, 100) } == 0,
        );
        r.check(
            "getresuid reads the pair back",
            unsafe { libc::getresuid(&mut ids[0], &mut ids[1], &mut ids[2]) } == 0,
        );
        r.check(
            "the saved id followed the new effective one",
            ids == [0, 100, 100],
        );
        // setuid for a caller that has lowered itself moves only the effective
        // word, to the real or the saved one - which is what the way back is.
        r.check(
            "setuid(0) raises the effective id back",
            unsafe { libc::setuid(0) } == 0,
        );
        r.check(
            "getresuid reads the raise back",
            unsafe { libc::getresuid(&mut ids[0], &mut ids[1], &mut ids[2]) } == 0,
        );
        r.check(
            "an unprivileged setuid moved only the effective word",
            ids == [0, 0, 100],
        );
        r.check(
            "the saved word is root's to reset",
            unsafe { libc::setresuid(u32::MAX, u32::MAX, 0) } == 0,
        );
        r.check(
            "getresuid reads the reset",
            unsafe { libc::getresuid(&mut ids[0], &mut ids[1], &mut ids[2]) } == 0,
        );
        r.check("all three words are root's again", ids == [0, 0, 0]);
    } else {
        println!(
            "note: the re/setuid claims below the dance assume a root identity restored from a lowered one; on a host run where the dance was refused they are named and not made"
        );
    }

    // Capabilities: root's state is everything permitted and effective,
    // nothing inheritable, in the version 3 shape of two records.
    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = CapData {
        records: [CapRecord {
            effective: 0,
            permitted: 0,
            inheritable: 0xdead_beef,
        }; 2],
    };
    let capget = |header: &mut CapHeader, data: *mut u8| {
        sys3(
            libc::SYS_capget,
            header as *mut CapHeader as usize,
            data as usize,
            0,
        )
    };
    let capset = |header: &mut CapHeader, data: *const u8| {
        sys3(
            libc::SYS_capset,
            header as *mut CapHeader as usize,
            data as usize,
            0,
        )
    };
    r.check(
        "capget v3 succeeds",
        capget(&mut header, &mut data as *mut CapData as *mut u8) == 0,
    );
    // The 41 capabilities of x86_64 are the low 32 bits plus the nine of the
    // high word's bottom bits: Linux reports `CapPrm`/`CapEff` as
    // `0x000001ffffffffff`, so the high word is `0x1ff` and not all-ones. The
    // number of capabilities is the uapi's (`libc::CAP_LAST_CAP`), not this
    // kernel's table - the same rule that caught the constants in earlier
    // batches.
    let cap_high: u32 = (1u32 << (CAP_LAST_CAP - 31)) - 1;
    r.check(
        "the low word is every capability",
        data.records[0].effective == u32::MAX,
    );
    r.check(
        "the high word is the nine top capabilities",
        data.records[1].effective == cap_high,
    );
    r.check(
        "and the permitted set matches it",
        data.records[1].permitted == cap_high,
    );
    r.check(
        "nothing is inheritable",
        data.every(|record| record.inheritable, 0),
    );
    header.version = 0xdead_beef;
    r.check(
        "capget with an unknown version answers EINVAL",
        capget(&mut header, &mut data as *mut CapData as *mut u8) == -(libc::EINVAL as isize),
    );

    // The pair round-trips: `capset` stores and `capget` reads the *stored*
    // sets, so the answer is no longer a constant - the counterfactual is the
    // old `capget`, which reported all-ones whatever `capset` had been told.
    //
    // The sets lowered below are the *effective* one alone, with permitted left
    // full. That is deliberate, and Linux's own arithmetic is why: a `capset`
    // that drops `permitted` drops `CAP_SETPCAP` with it, and a caller without
    // that capability can never raise the ceiling back (`cap_capset` asks for it
    // first and then refuses `permitted` outside the old set). Clearing only
    // `effective` keeps the restore inside the subset rules, so the round-trip
    // below is one a real Linux process could make too.
    header.version = 0x2008_0522;
    let mut lowered = CapData {
        records: [CapRecord {
            effective: 0,
            permitted: u32::MAX,
            inheritable: 0,
        }; 2],
    };
    lowered.records[1].permitted = cap_high;
    r.check(
        "capset lowering the effective set succeeds",
        capset(&mut header, &lowered as *const CapData as *const u8) == 0,
    );
    r.check(
        "capget succeeds after the lowering",
        capget(&mut header, &mut data as *mut CapData as *mut u8) == 0,
    );
    r.check(
        "and reads the emptied effective set back",
        data.every(|record| record.effective, 0),
    );
    r.check(
        "while the permitted set is untouched",
        data.records[0].permitted == u32::MAX,
    );
    r.check("in both words", data.records[1].permitted == cap_high);
    // And back: an effective set inside the permitted one is always allowed.
    let mut restored = lowered;
    restored.records[0].effective = u32::MAX;
    restored.records[1].effective = cap_high;
    r.check(
        "capset restores the effective set",
        capset(&mut header, &restored as *const CapData as *const u8) == 0,
    );
    r.check(
        "capget succeeds after the restore",
        capget(&mut header, &mut data as *mut CapData as *mut u8) == 0,
    );
    r.check(
        "the low word is all capabilities again",
        data.records[0].effective == u32::MAX,
    );
    r.check(
        "and the high word too",
        data.records[1].effective == cap_high,
    );

    // Priority: the raw syscall's encoding, the which validation, and the
    // range the man page allows root all of. The raw door, not glibc's
    // `getpriority`: the wrapper adds 20, and the kernel encoding is the
    // claim.
    r.check(
        "getpriority answers 20 (nice 0, kernel encoding)",
        sys2(libc::SYS_getpriority, 0, 0) == 20,
    );
    r.check(
        "setpriority 5 succeeds",
        sys3(libc::SYS_setpriority, 0, 0, 5) == 0,
    );
    // Out of range is *clamped*, not refused: Linux's `setpriority` clamps
    // (`kernel/sys.c`) and its man page lists `EINVAL` only for a bad `which`.
    // This asserted `EINVAL` until migration batch 10 measured the host.
    r.check(
        "setpriority out of range is clamped, not refused",
        sys3(libc::SYS_setpriority, 0, 0, 99) == 0,
    );
    r.check(
        "the clamped nice lands at 19 (raw 1)",
        sys2(libc::SYS_getpriority, 0, 0) == 1,
    );
    r.check(
        "setpriority with a bad which answers EINVAL",
        sys3(libc::SYS_setpriority, 3, 0, 0) == -(libc::EINVAL as isize),
    );

    // The one policy, and the bounds its table spells.
    r.check(
        "sched_getscheduler answers SCHED_OTHER",
        sched_getscheduler(0) == 0,
    );
    let mut param = libc::sched_param {
        sched_priority: -1,
        ..unsafe { std::mem::zeroed() }
    };
    r.check(
        "sched_getparam succeeds",
        sched_getparam(0, &mut param) == 0,
    );
    r.check(
        "the SCHED_OTHER priority is zero",
        param.sched_priority == 0,
    );
    r.check(
        "SCHED_OTHER's max priority is zero",
        unsafe { libc::sched_get_priority_max(0) } == 0,
    );
    r.check(
        "SCHED_FIFO's max priority is 99",
        unsafe { libc::sched_get_priority_max(1) } == 99,
    );
    r.check(
        "SCHED_FIFO's min priority is 1",
        unsafe { libc::sched_get_priority_min(1) } == 1,
    );
    r.check(
        "an unknown policy answers EINVAL",
        rc(unsafe { libc::sched_get_priority_max(99) }) == -(libc::EINVAL as libc::c_int),
    );

    // sched_setscheduler/sched_setparam validate the SCHED_OTHER priority,
    // which the policy's band fixes at zero. Measured on a real host: passing
    // a nonzero priority for SCHED_OTHER is EINVAL, and this kernel used to
    // answer 0 while dropping it - the accept-and-drop this pass removes.
    let prio_zero = libc::sched_param {
        sched_priority: 0,
        ..unsafe { std::mem::zeroed() }
    };
    let prio_five = libc::sched_param {
        sched_priority: 5,
        ..unsafe { std::mem::zeroed() }
    };
    let prio_seven = libc::sched_param {
        sched_priority: 7,
        ..unsafe { std::mem::zeroed() }
    };
    r.check(
        "sched_setscheduler(SCHED_OTHER, prio 0) succeeds",
        sched_setscheduler(0, 0, &prio_zero) == 0,
    );
    r.check(
        "sched_setscheduler(SCHED_OTHER, prio 5) is EINVAL",
        sched_setscheduler(0, 0, &prio_five) == -(libc::EINVAL as isize),
    );
    r.check(
        "sched_setparam(prio 0) succeeds",
        sched_setparam(0, &prio_zero) == 0,
    );
    r.check(
        "sched_setparam(prio 7) is EINVAL",
        sched_setparam(0, &prio_seven) == -(libc::EINVAL as isize),
    );
    // The task is still at the priority it was: a refused set left nothing in
    // force, so getparam answers 0 after the EINVAL above.
    param.sched_priority = -1;
    r.check(
        "sched_getparam still answers 0 after the refused sets",
        sched_getparam(0, &mut param) == 0,
    );
    r.check("the priority is 0", param.sched_priority == 0);

    // The machine: one core, node zero, the default persona. The cpu is the
    // one divergence the probe measured here: the host's answer is the core
    // it really ran on, so a host run accepts whatever it was given.
    let mut cpu: u32 = 99;
    let mut node: u32 = 99;
    r.check(
        "getcpu succeeds",
        sys2(
            libc::SYS_getcpu,
            &mut cpu as *mut u32 as usize,
            &mut node as *mut u32 as usize,
        ) == 0,
    );
    r.check("the cpu is 0", cpu == 0 || host_run());
    r.check("the node is 0", node == 0);
    r.check(
        "the persona query answers the default",
        unsafe { libc::personality(0xffff_ffff) } == 0,
    );
    r.check(
        "setting the default persona succeeds",
        unsafe { libc::personality(0) } == 0,
    );
    // The host stores any word where this kernel validates the persona table
    // (`personality(0x1000)` answers 0 there, measured) - a named divergence.
    let persona = rc(unsafe { libc::personality(0x1000) });
    r.check(
        "an unknown persona answers EINVAL",
        persona == -(libc::EINVAL as libc::c_int) || host_run() && persona == 0,
    );
    // vhangup: this program never acquired a controlling terminal with
    // TIOCSCTTY, so `current->signal->tty` is none and Linux's
    // `SYSCALL_DEFINE0(vhangup)` answers `ENOTTY`, not the `0` it reserves for
    // hanging up a terminal that exists. The host's privilege check runs
    // before all of that and answers `EPERM` (the container's root lacks
    // `CAP_SYS_TTY_CONFIG`, measured) - the klogctl-shaped gate, accepted on
    // a host run.
    let hung_up = rc(unsafe { libc::vhangup() });
    r.check(
        "vhangup answers ENOTTY with no controlling terminal",
        hung_up == -(libc::ENOTTY as libc::c_int)
            || host_run() && hung_up == -(libc::EPERM as libc::c_int),
    );

    // seccomp(2), as this kernel implements it (PLAN 8.2): STRICT latches
    // for real, FILTER installs a program, and the availability probe answers
    // which actions this kernel actually implements. What is pinned for
    // FILTER here is only that the program is *read before anything is
    // decided*: a null one is `EFAULT` rather than a success that confined
    // nobody. (A malformed program is `EINVAL`, and the filters themselves are
    // argued in `seccomp.rs`'s host tests, where a BPF program can be run
    // against made-up syscall numbers.) The strict checks are done
    // non-destructively - once latched, this program's next `getpid` is a
    // fatal call - and the latch itself is proved in a child, whose death
    // by SIGKILL is the whole of what Linux promises a strict thread that
    // reaches for anything else. The eight actions and each one's answer are
    // `t-seccomp`'s; this line pins only that a word naming no action at all
    // is refused rather than granted.
    r.check(
        "seccomp with an unknown op answers EINVAL",
        sys3(libc::SYS_seccomp, 99, 0, 0) == -(libc::EINVAL as isize),
    );
    // The host refuses STRICT outright (`EFAULT`, flags or no flags - the
    // container will not install it, `.workbuddy/probe_seccomp_mode.c`), so
    // its stray-flags refusal is that same refusal rather than this kernel's
    // `EINVAL`; a host run accepts it.
    let strict_flags = sys3(libc::SYS_seccomp, 0, 1, 0);
    r.check(
        "strict mode with stray flags answers EINVAL before latching",
        strict_flags == -(libc::EINVAL as isize)
            || host_run() && strict_flags == -(libc::EFAULT as isize),
    );
    r.check(
        "seccomp filter mode with no program answers EFAULT",
        sys3(libc::SYS_seccomp, 1, 0, 0) == -(libc::EFAULT as isize),
    );
    // 99 is nobody's action: an unknown one would kill the process that
    // relied on it, which is why a caller asks first and why the answer has
    // to be a refusal. The caller's own word is left alone - Linux's return
    // value carries the answer, and there is nothing to write back.
    let mut avail: u32 = 99;
    r.check(
        "the availability probe refuses an action this kernel does not implement",
        sys3(libc::SYS_seccomp, 2, 0, &mut avail as *mut u32 as usize)
            == -(libc::EOPNOTSUPP as isize),
    );
    r.check(
        "a refused probe leaves the caller's word alone",
        avail == 99,
    );
    {
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            // The child latches strict, then reaches for a syscall the
            // whitelist does not carry. It should not return; the exit
            // below is the failure path, reached only if the kernel let
            // the forbidden call answer like a normal one.
            let strict = sys3(libc::SYS_seccomp, 0, 0, 0);
            let probe = unsafe { libc::getpid() };
            // SAFETY: `_exit` never returns, and the child touches nothing
            // else.
            unsafe { libc::_exit(if strict == 0 && probe < 0 { 0 } else { 1 }) };
        }
        let mut status: libc::c_int = 0;
        let got = unsafe { libc::waitpid(kid, &mut status, 0) };
        r.check("the strict child was reaped", got == kid);
        // On the host the container refuses to install STRICT at all
        // (`EFAULT` above), so the child takes its early exit and the death
        // shape is unmeasurable there - named, not hidden: a host run accepts
        // the clean exit the refusal produced.
        let killed = status & 0x7f == 9;
        r.check(
            "the strict child died by SIGKILL on its first forbidden call",
            killed || host_run() && got == kid && libc::WIFEXITED(status),
        );
    }

    // The namespace flag word: one kind served, seven refused, and the numbers
    // matters because a caller sends the number and not the name. Every value
    // below is `include/uapi/linux/sched.h`'s, and each was measured on a real
    // kernel before being written down - which is what caught this block
    // asserting `0x1_0000` was `CLONE_NEWNS`: that is `CLONE_THREAD`, and Linux
    // answers 0 for it. The mount namespace is `0x2_0000`, and a program asking
    // for it was being told `EINVAL` as though the bit were undefined.
    //
    // `unshare(CLONE_NEWNS)` is the row where this kernel and Linux part
    // company, and deliberately: Linux answers `EPERM` there for want of
    // `CAP_SYS_ADMIN`, and this answers `EOPNOTSUPP` because there is no second
    // mount table to be given even to root. That is the plan's refuse-rather-
    // than-approximate rule, and a caller told `EPERM` would go looking for a
    // capability that does not help. The probe's host root *has* the
    // capability, and the host serves both this and `CLONE_NEWTIME` - so a
    // host run accepts the served answer beside this kernel's refusal.
    r.check("unshare(0) succeeds", unsafe { libc::unshare(0) } == 0);
    let mount_ns = rc(unsafe { libc::unshare(0x0002_0000) });
    r.check(
        "unshare CLONE_NEWNS answers EOPNOTSUPP",
        mount_ns == -(libc::EOPNOTSUPP as libc::c_int) || host_run() && mount_ns == 0,
    );
    let time_ns = rc(unsafe { libc::unshare(0x0000_0080) });
    r.check(
        "unshare CLONE_NEWTIME answers EOPNOTSUPP",
        time_ns == -(libc::EOPNOTSUPP as libc::c_int) || host_run() && time_ns == 0,
    );
    // CLONE_NEWUTS is the one namespace this kernel *serves*, and this line
    // used to assert `EOPNOTSUPP` along with the other seven: it is the pin that
    // moved with the measurement when UTS landed (plan §6.3, step 1). All it can
    // pin here is that the call is granted, which is exactly what the other
    // seven do not get; what the copy buys - and that the namespace it came from
    // does not follow - is `t-ns-uts`'s subject.
    r.check(
        "unshare CLONE_NEWUTS is served (the one namespace built so far)",
        unsafe { libc::unshare(0x0400_0000) } == 0,
    );
    // The thread-group flags: a task with no sibling of its own to split from
    // owns all six already, so there is nothing to take away and the answer is
    // the no-op Linux gives a single-threaded caller. (For a caller that *does*
    // have siblings Linux answers `EINVAL` and this kernel still answers 0 - a
    // named gap in plan §6.3, not a claim that they were split.)
    r.check(
        "unshare CLONE_THREAD is the no-op it is on Linux",
        unsafe { libc::unshare(0x0001_0000) } == 0,
    );
    r.check(
        "unshare CLONE_FILES|CLONE_FS is a no-op",
        unsafe { libc::unshare(0x0000_0600) } == 0,
    );
    // `CLONE_SETTLS` is not a namespace and not something `unshare` can do:
    // Linux answers EINVAL for it, and this is the value the file used to spell
    // `CLONE_NEWTIME`.
    r.check(
        "unshare CLONE_SETTLS answers EINVAL",
        rc(unsafe { libc::unshare(0x0008_0000) }) == -(libc::EINVAL as libc::c_int),
    );
    r.check(
        "unshare CLONE_PIDFD answers EINVAL",
        rc(unsafe { libc::unshare(0x0000_1000) }) == -(libc::EINVAL as libc::c_int),
    );
    // An undefined bit is refused ahead of the namespaces beside it, which is
    // Linux's order (measured: `CLONE_PIDFD|CLONE_NEWNS` is EINVAL, not the
    // namespace's EPERM).
    r.check(
        "unshare with a non-namespace bit answers EINVAL",
        rc(unsafe { libc::unshare(0x1) }) == -(libc::EINVAL as libc::c_int),
    );
    r.check(
        "an undefined bit outranks a namespace bit",
        rc(unsafe { libc::unshare(0x0002_1000) }) == -(libc::EINVAL as libc::c_int),
    );
    let mut pipe_fds: [libc::c_int; 2] = [-1; 2];
    r.check(
        "the pipe pair is made",
        unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), 0) } == 0,
    );
    r.check(
        "setns on a pipe answers EINVAL",
        rc(unsafe { libc::setns(pipe_fds[0], 0) }) == -(libc::EINVAL as libc::c_int),
    );
    r.check(
        "setns on a closed number answers EBADF",
        rc(unsafe { libc::setns(99, 0) }) == -(libc::EBADF as libc::c_int),
    );

    // One thread's signal, named by its group and itself: the probe form
    // (sig 0) answers 0 for a live thread, ESRCH for a dead one and for a
    // group the thread does not belong to.
    let pid = unsafe { libc::getpid() };
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
    r.check("tgkill's probe on self succeeds", tgkill(pid, tid, 0) == 0);
    r.check(
        "tgkill's probe on a dead thread answers ESRCH",
        tgkill(pid, 99, 0) == -(libc::ESRCH as libc::c_int),
    );
    r.check(
        "tgkill with a wrong group answers ESRCH",
        tgkill(9999, tid, 0) == -(libc::ESRCH as libc::c_int),
    );

    // The memory advice family, over a page that is mapped.
    let page = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            4096,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    };
    r.check("mmap hands back a page", page.is_ok());
    let page = page.map(|p| p as usize).unwrap_or(0);
    let mut vec = [0u8; 1];
    r.check(
        "mincore succeeds",
        unsafe { libc::mincore(page as *mut libc::c_void, 4096, vec.as_mut_ptr()) } == 0,
    );
    // Anonymous demand paging: the address is the process's from the `mmap`,
    // and nothing is behind it until it is touched. `mincore` answers from the
    // page tables, so a freshly mapped page reads as mapped and *not*
    // resident - which is the truth, and the point.
    r.check(
        "a freshly mapped page is mapped and not yet resident",
        vec[0] == 0,
    );
    r.check(
        "mincore on an unaligned range answers EINVAL",
        rc(unsafe { libc::mincore((page + 1) as *mut libc::c_void, 4096, vec.as_mut_ptr()) })
            == -(libc::EINVAL as libc::c_int),
    );
    unsafe { (page as *mut u8).write_volatile(1) };
    r.check(
        "mincore after touching the page succeeds",
        unsafe { libc::mincore(page as *mut libc::c_void, 4096, vec.as_mut_ptr()) } == 0,
    );
    r.check("the touched page is resident", vec[0] == 1);
    r.check(
        "mlock succeeds",
        unsafe { libc::mlock(page as *const libc::c_void, 4096) } == 0,
    );
    r.check(
        "munlock succeeds",
        unsafe { libc::munlock(page as *const libc::c_void, 4096) } == 0,
    );
    r.check("mlockall succeeds", unsafe { libc::mlockall(1) } == 0);
    r.check(
        "mlockall with an unknown flag answers EINVAL",
        rc(unsafe { libc::mlockall(0x8) }) == -(libc::EINVAL as libc::c_int),
    );
    r.check("munlockall succeeds", unsafe { libc::munlockall() } == 0);
    r.check(
        "msync succeeds",
        unsafe { libc::msync(page as *mut libc::c_void, 4096, 4) } == 0,
    );
    r.check(
        "msync with SYNC beside ASYNC answers EINVAL",
        rc(unsafe { libc::msync(page as *mut libc::c_void, 4096, 1 | 4) })
            == -(libc::EINVAL as libc::c_int),
    );

    let _ = unsafe { libc::close(pipe_fds[0]) };
    let _ = unsafe { libc::close(pipe_fds[1]) };

    r.finish()
}
