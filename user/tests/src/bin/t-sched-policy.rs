//! The policy word: which numbers are policies, and which door says "not a
//! policy" as opposed to "not here" (§6.122).
//!
//! Linux's `sched_setscheduler` takes a word that is *either* a policy number
//! *or* a policy number with `SCHED_RESET_ON_FORK` in it, and the two refusal
//! shapes around it answer different questions: `EINVAL` means "that word is
//! not a policy, or that priority is outside its band", while the settled
//! answer for a policy this scheduler does not serve is `EOPNOTSUPP`. A caller
//! that only knows the standard policies cannot tell our `EINVAL` from "you
//! spelled it wrong", which is why AGENTS.md §3 makes answering `EINVAL` for
//! something that exists one of the two worst things this kernel can do.
//!
//! Measured on Linux 7.2.14 (`build/scratch/statpolicy/d.c`, `e.c`, `r.c`, one
//! `fork` per case so no case inherits the previous one's policy):
//!
//! ```text
//! sched_setscheduler(0, OTHER,    {0})   -> 0
//! sched_setscheduler(0, BATCH,    {0})   -> 0
//! sched_setscheduler(0, IDLE,     {0})   -> 0
//! sched_setscheduler(0, EXT(7),   {0})   -> 0      SCHED_EXT is a policy here
//! sched_setscheduler(0, RESET|*,  {0})   -> 0      and getscheduler reads the flag back
//! sched_setscheduler(0, FIFO,     {0})   -> EINVAL(22)   the band first
//! sched_setscheduler(0, FIFO,   {100})   -> EINVAL(22)
//! sched_setscheduler(0, FIFO,    {10})   -> EPERM(1)     a valid band, then authority
//! sched_setscheduler(0, RR,       {0})   -> EINVAL(22)
//! sched_setscheduler(0, DEADLINE, {0})   -> EINVAL(22)   deadline has its own door
//! sched_setscheduler(0, RESET|DEADLINE)  -> EINVAL(22)
//! sched_setscheduler(0, 8 / 9 / 0x10000000) -> EINVAL(22)
//! sched_get_priority_max(0/1/2/3/5/6/7) -> 0/99/99/0/0/0/0
//! sched_get_priority_max(8)              -> EINVAL(22)
//! sched_getattr(48)                      -> policy 0, nice 0, rtprio 0
//! ```
//!
//! So the policy set at this door is `{0,1,2,3,5,7}` - `6` is a policy number
//! (`priority_max(6)` answers 0) that this door refuses, and 7 is `SCHED_EXT`,
//! which answers a zero-length band like the other fair classes.
//!
//! This machine serves one band (`SCHED_OTHER`), so the policies it cannot run
//! answer `EOPNOTSUPP` where Linux answers 0. Run with `MPOS_ON_HOST=1` the
//! same claims measure Linux, so every divergence below is written as "what
//! this arm can honestly answer" rather than a snapshot of one kernel.

use mpos_user_tests::Report;

const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_DEADLINE: i32 = 6;
const SCHED_EXT: i32 = 7;
/// The flag Linux lets a caller OR into any servable policy word.
const RESET_ON_FORK: i32 = 0x4000_0000;
const SIZE_V1: u32 = 48;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

/// Runs one case in a forked child and answers the errno it failed with (0 for
/// success). The policy doors are sticky - a successful `SCHED_BATCH` makes the
/// rest of the run batch - so every case gets a fresh process, exactly as
/// Linux's own probe did (`build/scratch/statpolicy/d.c`).
fn in_child<F: FnOnce() -> i32>(f: F) -> i32 {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return -1;
    }
    if pid == 0 {
        let code = f();
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if !libc::WIFEXITED(status) {
        return -1;
    }
    libc::WEXITSTATUS(status)
}

/// The errno a failed call reports; `0` when it succeeded.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

// Every policy door is called with a raw syscall. The Rust target's musl stubs
// `sched_setscheduler`/`sched_getscheduler` out to `ENOSYS` without entering the
// kernel at all (measured: the wrapper answers 38 while the raw call on the
// same binary succeeds), so a wrapper call in this pin would measure the stub
// rather than the kernel - the same trap `t-sched-attr.rs` avoids.
fn setscheduler(policy: i32, prio: i32) -> i32 {
    in_child(|| {
        // `libc::sched_param` is the generic-unix superset on this target, so
        // the one field this door reads is set on a zeroed value.
        let mut sp: libc::sched_param = unsafe { core::mem::zeroed() };
        sp.sched_priority = prio;
        let r = unsafe {
            libc::syscall(
                libc::SYS_sched_setscheduler,
                0 as libc::c_long,
                policy as libc::c_long,
                &sp as *const libc::sched_param as libc::c_long,
            )
        };
        if r < 0 { last_errno() } else { 0 }
    })
}

fn getscheduler() -> i64 {
    unsafe { libc::syscall(libc::SYS_sched_getscheduler, 0 as libc::c_long) }
}

fn priority_max(policy: i32) -> i32 {
    unsafe { libc::sched_get_priority_max(policy) }
}

/// One `sched_setattr` call with a 48-byte v1 struct whose only interesting
/// fields are `policy` (offset 4) and `nice` (offset 16).
fn setattr(policy: i32) -> i32 {
    in_child(|| {
        let mut bytes = [0u8; SIZE_V1 as usize];
        bytes[0..4].copy_from_slice(&SIZE_V1.to_ne_bytes());
        bytes[4..8].copy_from_slice(&(policy as u32).to_ne_bytes());
        let r = unsafe {
            libc::syscall(
                libc::SYS_sched_setattr,
                0 as libc::c_long,
                bytes.as_mut_ptr() as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        if r < 0 { last_errno() } else { 0 }
    })
}

/// The field `number` of `/proc/self/stat`: the line is `pid (comm) state ...`,
/// so the tokens after the command's closing parenthesis start at field 3 and
/// field N is the `(N-3)`-th of them. Getting this off by one reads the
/// neighbouring field, which is how a first draft reported `processor` as 17 -
/// that number is `exit_signal`'s SIGCHLD.
fn stat_field(text: &str, number: usize) -> Option<u64> {
    let close = text.rfind(')')?;
    let tail = &text[close + 1..];
    tail.split_whitespace()
        .nth(number.checked_sub(3)?)?
        .parse()
        .ok()
}

fn stat_text() -> Option<String> {
    std::fs::read_to_string("/proc/self/stat").ok()
}

fn main() {
    let mut report = Report::new();
    let host = mpos_user_tests::host_run();

    // What a policy this scheduler cannot run must answer: Linux serves it (0),
    // this kernel says the policy exists but the scheduler has one band.
    let unsupported = if host { 0 } else { EOPNOTSUPP };

    let named = getscheduler();
    report.check(
        &format!("sched_getscheduler names the one policy this machine serves ({named})"),
        named == SCHED_OTHER as i64,
    );

    let other = setscheduler(SCHED_OTHER, 0);
    report.check(
        &format!("sched_setscheduler(SCHED_OTHER, 0) is served ({other})"),
        other == 0,
    );

    // ---- policies Linux serves and this band cannot --------------------------
    let batch = setscheduler(SCHED_BATCH, 0);
    report.check(
        &format!("SCHED_BATCH is a policy, so the answer is about support ({batch})"),
        batch == unsupported,
    );
    let idle = setscheduler(SCHED_IDLE, 0);
    report.check(
        &format!("SCHED_IDLE likewise ({idle})"),
        idle == unsupported,
    );
    let ext_max = priority_max(SCHED_EXT);
    report.check(
        &format!("SCHED_EXT is a policy number too, so its band is the fair one ({ext_max})"),
        ext_max == 0,
    );
    let ext = setscheduler(SCHED_EXT, 0);
    report.check(
        &format!("and its door is a support refusal, not a bad word ({ext})"),
        ext == unsupported,
    );
    let reset = setscheduler(RESET_ON_FORK | SCHED_OTHER, 0);
    report.check(
        &format!("SCHED_RESET_ON_FORK|SCHED_OTHER is a policy word too ({reset})"),
        reset == unsupported,
    );

    // ---- the band is judged before support is ---------------------------------
    let fifo_zero = setscheduler(SCHED_FIFO, 0);
    report.check(
        &format!("SCHED_FIFO at priority 0 is EINVAL, the band before support ({fifo_zero})"),
        fifo_zero == EINVAL,
    );
    let fifo_high = setscheduler(SCHED_FIFO, 100);
    report.check(
        &format!("and 100 is outside the band too ({fifo_high})"),
        fifo_high == EINVAL,
    );
    let fifo_ok = setscheduler(SCHED_FIFO, 10);
    report.check(
        &format!("a real-time priority inside the band is a support refusal ({fifo_ok})"),
        fifo_ok == if host { 1 } else { EOPNOTSUPP },
    );
    let rr_zero = setscheduler(SCHED_RR, 0);
    report.check(
        &format!("SCHED_RR at priority 0 is EINVAL as well ({rr_zero})"),
        rr_zero == EINVAL,
    );

    // ---- policy numbers that are not policies at all --------------------------
    let deadline = setscheduler(SCHED_DEADLINE, 0);
    report.check(
        &format!("SCHED_DEADLINE at this door is EINVAL, Linux's own answer ({deadline})"),
        deadline == EINVAL,
    );
    let reset_deadline = setscheduler(RESET_ON_FORK | SCHED_DEADLINE, 0);
    report.check(
        &format!("and the flag does not make it a word this door takes ({reset_deadline})"),
        reset_deadline == EINVAL,
    );
    let eight = setscheduler(8, 0);
    report.check(
        &format!("policy 8 is not a policy number ({eight})"),
        eight == EINVAL,
    );
    let high_word = setscheduler(0x1000_0000, 0);
    report.check(
        &format!("and neither is 0x10000000 ({high_word})"),
        high_word == EINVAL,
    );
    let batch_param = setscheduler(SCHED_BATCH, 1);
    report.check(
        &format!(
            "a batch policy with a non-zero priority is EINVAL, the param first ({batch_param})"
        ),
        batch_param == EINVAL,
    );

    // ---- the sibling that answers about bands ---------------------------------
    let max_eight = priority_max(8);
    let max_eight_errno = last_errno();
    report.check(
        &format!(
            "sched_get_priority_max refuses the word Linux refuses ({max_eight}, errno {max_eight_errno})"
        ),
        max_eight < 0 && max_eight_errno == EINVAL,
    );

    // ---- the same word through the other door ---------------------------------
    let attr_ext = setattr(SCHED_EXT);
    report.check(
        &format!("sched_setattr's SCHED_EXT is a support refusal too ({attr_ext})"),
        attr_ext == unsupported,
    );
    let attr_eight = setattr(8);
    report.check(
        &format!("and policy 8 there is EINVAL ({attr_eight})"),
        attr_eight == EINVAL,
    );

    // ---- /proc/<pid>/stat's three scheduling fields ---------------------------
    let Some(stat) = stat_text() else {
        report.check("the stat line is readable", false);
        report.finish();
    };
    let policy = stat_field(&stat, 41);
    report.check(
        &format!("stat's policy field is the policy sched_getscheduler names ({policy:?})"),
        policy == Some((named & !(RESET_ON_FORK as i64)) as u64),
    );
    if !host {
        let rt_priority = stat_field(&stat, 40);
        report.check(
            &format!("and its rt_priority is 0, because nothing here runs in a real-time band ({rt_priority:?})"),
            rt_priority == Some(0),
        );
    }
    if !host {
        // The measuring host has many CPUs and moves a task between them; this
        // kernel has one and keeps the field at 0 for every task.
        let processor = stat_field(&stat, 39);
        report.check(
            &format!("and its processor is 0 on a machine with one CPU ({processor:?})"),
            processor == Some(0),
        );
    }

    report.finish();
}
