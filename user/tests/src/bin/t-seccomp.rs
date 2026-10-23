//! t-seccomp - a `SECCOMP_MODE_FILTER` program, judged live on this machine.
//!
//! `seccomp.rs`'s host tests can run a filter against a made-up syscall block,
//! and this file is about the half they cannot reach: the answer a *real* call
//! gets, decided by the gate in `syscall::linux::dispatch` before the table ever
//! runs. Each pin below corresponds to a measurement taken on Linux first
//! (`NOTES.md` §8.2 holds them); several are not what the man page sounds like,
//! which is why they are here rather than in a comment:
//!
//! * a kill action (`KILL_THREAD`, `KILL_PROCESS`) ends the task with
//!   **`SIGSYS`**, not `SIGKILL`;
//! * an action the kernel does not recognise is treated as `KILL_PROCESS`;
//! * `TRACE` with nobody tracing fails the call with `ENOSYS` rather than
//!   allowing it, as does `USER_NOTIF` with no listener;
//! * the availability probe compares whole words, so asking about `ERRNO|1` is
//!   a refusal even though asking about `ERRNO` is not;
//! * a tracer's `PTRACE_SETREGS` at a `TRACE` stop picks the call that runs -
//!   and the `-1` sentinel there *cancels* it instead of replacing it.
//!
//! # The door, and the number the filter compares against
//!
//! `rustix` binds no part of `seccomp(2)`, so the whole call is the raw one -
//! `libc::syscall(libc::SYS_seccomp, ..)` with the operations, the actions and
//! the two BPF structs taken from `libc`'s `linux/seccomp.h` and `linux/filter.h`
//! mirrors. That is not a shortcut: the *numbers* here are the claims (the
//! opcode `0x20` that loads the syscall word, the action words in the high half
//! of a verdict, `SECCOMP_SET_MODE_FILTER` = 1 against `GET_ACTION_AVAIL` = 2),
//! and `rustix` has no vocabulary for any of them.
//!
//! The one number that must **not** be written down is the syscall the filters
//! judge. The no-std original compared against a literal `39` (and `110`) - this
//! first machine's numbers for `getpid`/`getppid` - which was invisible while the
//! suite ran on one machine only. `aarch64`'s are 172 and 173, and the second
//! machine's sweep *does* stage this crate, so a literal here would install a
//! filter that judges a call nothing makes and then pin the absence of an effect
//! as a claim. They come from `libc::SYS_getpid`/`SYS_getppid`, the maintained
//! per-machine table.
//!
//! The *call* is raw too, and that was measured rather than assumed: a filter
//! judges `seccomp_data.nr`, so the only call it can judge is the syscall, and a
//! libc wrapper is a second thing that can answer. Measured on a host Linux under
//! a filter denying `getpid`: `syscall(SYS_getpid)` answers `-1`/`EPERM`, while
//! `getpid()` answers `-1` with `errno` left at **0** - so the first draft, which
//! asked `libc::getpid()`, reported five false failures about filters that were
//! working. Every judged call below is `libc::syscall(libc::SYS_*)`.
//!
//! # The one divergence
//!
//! `SECCOMP_GET_ACTION_AVAIL` for `USER_NOTIF`: this kernel has no supervisor to
//! summon, so it refuses the action at the availability probe, where a host Linux
//! serves it. That refusal is the claim; a host run prints what its own kernel
//! answered and carries on. `USER_NOTIF`'s *effect* once installed - `ENOSYS`
//! with no listener - matches on both machines and is pinned below.
//!
//! # Ordering
//!
//! Ordering matters here, and it is the reason the file reads in three acts. A
//! filter cannot be un-installed and is inherited across `fork`, so every child
//! carries its parent's list: the children that pin one action's answer run
//! **before** this process installs anything, and the child that pins
//! inheritance-and-stacking runs after. Getting that wrong does not fail
//! loudly; it makes the wrong filter the one being answered, because when two
//! programs judge the same call the more restrictive wins, and `ERRNO` outranks
//! every action below it except the kills and `TRAP`.
//!
//! One care taken throughout: each filter judges exactly one number (`getpid`)
//! and allows everything else, so installing one does not silence this test's
//! own output. The drafts that judged every call measured their own collapse
//! instead of the kernel.

use mpos_user_tests::{Report, host_run};
use rustix::io::Errno;
use std::os::raw::c_long;

/// The one `ptrace(2)` word musl's `libc` does not name, and the event number
/// the stop's status carries. `TRACESECCOMP` is bit 7 of the `PTRACE_O_*` set
/// (`linux/ptrace.h`), and the seccomp event is 7 in the `PTRACE_EVENT_*` list.
const PTRACE_O_TRACESECCOMP: c_long = 0x80;
const PTRACE_EVENT_SECCOMP: i32 = 7;

/// The register file's width and the slot a tracer rewrites to redirect a
/// syscall. Both are the machine's: x86_64's `user_regs_struct` is 27 words and
/// names the number `orig_rax`, the 16th; aarch64's `user_pt_regs` is 34 and
/// carries it in `x8`, the 9th. `t-ptrace` states the same two widths.
#[cfg(not(target_arch = "aarch64"))]
const REG_WORDS: usize = 27;
#[cfg(target_arch = "aarch64")]
const REG_WORDS: usize = 34;
#[cfg(not(target_arch = "aarch64"))]
const NUMBER_SLOT: usize = 15;
#[cfg(target_arch = "aarch64")]
const NUMBER_SLOT: usize = 8;

/// The judged number, and the one the second filter below takes instead. Read
/// from `libc`'s per-machine table rather than written down - see the header.
fn getpid_nr() -> u32 {
    libc::SYS_getpid as u32
}
fn getppid_nr() -> u32 {
    libc::SYS_getppid as u32
}

// classic BPF opcodes (`linux/filter.h`), the three this needs.
const LD_W_ABS: u16 = 0x20;
const JMP_JEQ_K: u16 = 0x15;
const RET_K: u16 = 0x06;

/// An action naming nothing at all: not one of the eight the uapi defines. The
/// number is made up on purpose, and the claim is that the kernel treats it as
/// `KILL_PROCESS` rather than as permission.
const RET_UNKNOWN: u32 = 0x1234_0000;

/// One `struct sock_filter`, through `libc`'s `linux/filter.h` layout.
fn insn(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Judge one number with `action` and let every other call through.
///
/// The shape `libseccomp` emits: load the number, compare it, answer. The false
/// path skipping over one instruction is what makes this a filter rather than a
/// verdict on everything.
fn judge(nr: u32, action: u32) -> [libc::sock_filter; 4] {
    [
        insn(LD_W_ABS, 0, 0, 0),
        insn(JMP_JEQ_K, 0, 1, nr),
        insn(RET_K, 0, 0, action),
        insn(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ]
}

/// One `ptrace` call; the number is `libc::SYS_ptrace`, the per-arch table,
/// never written down here (the reason `t-ptrace` states at length).
fn ptrace(request: c_long, pid: libc::pid_t, addr: usize, data: usize) -> Result<isize, i32> {
    // SAFETY: `syscall` is a libc entry point; the widths are the ABI's, and
    // every call below passes a pointer this process owns, or null.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ptrace,
            request,
            pid as c_long,
            addr as c_long,
            data as c_long,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(rc as isize)
    }
}

/// `seccomp(2)` through the raw door: `(operation, flags, argument)`.
fn seccomp(op: usize, flags: usize, arg: usize) -> Result<isize, Errno> {
    let ret = unsafe { libc::syscall(libc::SYS_seccomp, op, flags, arg) };
    if ret == -1 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    } else {
        Ok(ret as isize)
    }
}

/// Install one program that judges `nr` and allows the rest.
fn install(nr: u32, action: u32) -> Result<isize, Errno> {
    let program = judge(nr, action); // lives until the install returns
    let prog = libc::sock_fprog {
        len: program.len() as libc::c_ushort,
        filter: program.as_ptr() as *mut libc::sock_filter,
    };
    seccomp(
        libc::SECCOMP_SET_MODE_FILTER as usize,
        0,
        &prog as *const libc::sock_fprog as usize,
    )
}

/// The low seven bits of a status word name the terminating signal; 0x7f there
/// is a *stop* rather than a death, and 0 means it simply exited.
fn died_of(status: i32) -> i32 {
    let sig = status & 0x7f;
    if sig == 0 || sig == 0x7f { 0 } else { sig }
}

/// A claim that carries what it saw when it did not hold: the answer here is a
/// *number* the filter chose, so a bare `FAIL` would say nothing about which
/// number arrived.
fn check_with<T: std::fmt::Debug>(r: &mut Report, label: &str, ok: bool, detail: T) {
    if ok {
        r.check(label, true);
    } else {
        r.check(&format!("{label} (got {detail:?})"), false);
    }
}

/// The judged call, issued as the syscall it is.
///
/// A filter judges `seccomp_data.nr`, so the only call it *can* judge is the raw
/// one; a libc wrapper is a second thing that can answer. Measured on a host
/// Linux: under a filter denying `getpid`, `syscall(SYS_getpid)` answers
/// `-1`/`EPERM` while `getpid()` answers `-1` with `errno` left at 0 - so a claim
/// made through the wrapper would be about the wrapper's errno handling and not
/// about the filter. Both halves of a verdict, as (return, errno).
fn raw_getpid() -> (isize, Option<i32>) {
    let got = unsafe { libc::syscall(libc::SYS_getpid) } as isize;
    (got, std::io::Error::last_os_error().raw_os_error())
}

fn raw_getppid() -> (isize, Option<i32>) {
    let got = unsafe { libc::syscall(libc::SYS_getppid) } as isize;
    (got, std::io::Error::last_os_error().raw_os_error())
}

/// Whether a call was refused, and with `want`.
fn denied(got: (isize, Option<i32>), want: i32) -> bool {
    got.0 == -1 && got.1 == Some(want)
}

/// How a child ends after installing `action` and reaching for the judged call,
/// where the only two things it can do are die or answer. 70 means its install
/// failed, 40 means it answered `ENOSYS` and survived, 41 means it got an
/// ordinary answer and survived.
///
/// The child is raw `libc::fork`/`libc::_exit` on purpose: nothing of `std`'s
/// may run between them, and the parent wants the raw status word to read the
/// signal out of.
fn a_child_ends_with(action: u32) -> i32 {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        if install(getpid_nr(), action).map(|v| v == 0) != Ok(true) {
            unsafe { libc::_exit(70) };
        }
        let absent = denied(raw_getpid(), libc::ENOSYS);
        unsafe { libc::_exit(if absent { 40 } else { 41 }) };
    }
    let mut status = 0i32;
    if unsafe { libc::waitpid(kid, &mut status, 0) } != kid {
        return -1;
    }
    status
}

/// The `TRACE` case a tracer turns on: the event the kernel owes a tracer that
/// asked for `PTRACE_O_TRACESECCOMP`.
///
/// The child hands itself over, installs a filter whose `TRACE` decision carries
/// a data word, and makes the judged call; the parent sets the option and reads
/// the stop that call produces. Three things are measured, each on Linux first
/// (`docs/notes/` §8.2): the stop's status carries event 7, its signal is a
/// plain `SIGTRAP`, and `PTRACE_GETEVENTMSG` answers the **data half** of the
/// filter's answer (`0x1234`) rather than a pid. The call then runs normally -
/// the stop is a notice, not a refusal - which is why the child exits 0.
fn seccomp_stop_reaches_a_tracer(r: &mut Report) {
    const DATA: u32 = 0x1234;
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: syscalls only between fork and `_exit`, each a straight call.
        unsafe {
            if ptrace(libc::PTRACE_TRACEME as c_long, 0, 0, 0) != Ok(0) {
                libc::_exit(11);
            }
            // The stop the tracer needs before it can set its options.
            libc::raise(libc::SIGSTOP);
            if install(getpid_nr(), libc::SECCOMP_RET_TRACE | DATA).map(|v| v == 0) != Ok(true) {
                libc::_exit(12);
            }
            // The judged call: the tracer sees the stop, rewrites the number
            // slot to `getppid`, and *that* call is what runs. Reading this
            // process's own real parent below is what tells the two apart - a
            // `getpid` answers this process's own id, a `getppid` its parent's,
            // and for a task that is not its own parent the two differ.
            let redirected = raw_getpid();
            let parent = raw_getppid();
            libc::_exit(if redirected.0 > 0 && redirected.0 == parent.0 {
                0
            } else {
                13
            });
        }
    }
    let mut status = 0i32;
    // SAFETY: `child` is this process's own child and `status` a local.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the seccomp tracee stops for TRACEME first",
        n == child && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );
    if n != child {
        return;
    }
    r.check(
        "a tracer can ask for PTRACE_O_TRACESECCOMP",
        ptrace(
            libc::PTRACE_SETOPTIONS as c_long,
            child,
            0,
            PTRACE_O_TRACESECCOMP as usize,
        ) == Ok(0),
    );
    r.check(
        "resuming the tracee from its own stop",
        ptrace(libc::PTRACE_CONT as c_long, child, 0, 0) == Ok(0),
    );

    // The next stop is the judged call's seccomp event: `PTRACE_CONT` does not
    // arm the syscall boundaries, so nothing else the child does stops it.
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    let stopped = n == child && libc::WIFSTOPPED(status);
    r.check(
        &format!(
            "a SECCOMP_RET_TRACE call produces a seccomp stop \
             (status {status:#x}: stopped={} event={})",
            libc::WIFSTOPPED(status),
            status >> 16,
        ),
        stopped && (status >> 16) == PTRACE_EVENT_SECCOMP,
    );
    r.check(
        "the seccomp stop's signal is a plain SIGTRAP",
        stopped && libc::WSTOPSIG(status) == libc::SIGTRAP,
    );
    let mut msg: u64 = 0;
    let got = ptrace(
        libc::PTRACE_GETEVENTMSG as c_long,
        child,
        0,
        &mut msg as *mut u64 as usize,
    );
    r.check(
        &format!(
            "PTRACE_GETEVENTMSG answers the answer's data half (got {msg:#x}, want {DATA:#x})"
        ),
        got == Ok(0) && msg == DATA as u64,
    );

    // ---- the redirect: rewriting the number slot changes the call. This is
    // ---- the sub-gap §8.2 named - a `PTRACE_SETREGS` here is how a
    // ---- syscall-table sandbox replaces a call - and the tracee's exit status
    // ---- is the proof: it exits 0 only if the redirected `getppid` answered,
    // ---- which the `getpid` it asked for cannot do.
    let mut regs = [0u64; REG_WORDS];
    r.check(
        "the tracer can read the tracee's registers at the seccomp stop",
        ptrace(
            libc::PTRACE_GETREGS as c_long,
            child,
            0,
            regs.as_mut_ptr() as usize,
        ) == Ok(0),
    );
    // The slot is the machine's (`orig_rax` or `x8`); the number is `libc`'s
    // per-machine `getppid`, never written down (the header's rule).
    regs[NUMBER_SLOT] = getppid_nr() as u64;
    r.check(
        "the tracer can rewrite the syscall number at the seccomp stop",
        ptrace(
            libc::PTRACE_SETREGS as c_long,
            child,
            0,
            regs.as_ptr() as usize,
        ) == Ok(0),
    );

    let resumed = ptrace(libc::PTRACE_CONT as c_long, child, 0, 0) == Ok(0);
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the redirected call runs, and the tracee exits normally",
        resumed && n == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
}

/// The seccomp stop's other use of the number slot: the `-1` sentinel
/// **cancels** the judged call rather than replacing it with another number.
///
/// This is the same write `entry_stop_rewrite_changes_the_call` makes at an
/// ordinary entry stop and `seccomp_stop_reaches_a_tracer` makes here to
/// redirect, with the one value that means "do not run this call at all".
/// Measured on this developer's Linux first (kernel 7.2,
/// `.workbuddy/probe-seccomp-cancel.c`): the seccomp event still arrives with
/// the judged number in the slot, and after the tracer writes `-1` the tracee's
/// `getpid` answers `-1` with `ENOSYS` - the call did not run, which for a
/// `getpid` is visible because a call that *did* run answers a positive pid.
fn seccomp_stop_cancel_skips_the_call(r: &mut Report) {
    const DATA: u32 = 0x1234;
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: syscalls only between fork and `_exit`, each a straight call.
        unsafe {
            if ptrace(libc::PTRACE_TRACEME as c_long, 0, 0, 0) != Ok(0) {
                libc::_exit(11);
            }
            libc::raise(libc::SIGSTOP);
            if install(getpid_nr(), libc::SECCOMP_RET_TRACE | DATA).map(|v| v == 0) != Ok(true) {
                libc::_exit(12);
            }
            // The judged call, cancelled by the tracer: a call that ran would
            // answer this process's own pid, so `-1`/`ENOSYS` is the proof it
            // did not.
            libc::_exit(if denied(raw_getpid(), libc::ENOSYS) {
                0
            } else {
                13
            });
        }
    }
    let mut status = 0i32;
    // SAFETY: `child` is this process's own child and `status` a local.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the cancel tracee stops for TRACEME first",
        n == child && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );
    if n != child {
        return;
    }
    r.check(
        "a tracer can ask for PTRACE_O_TRACESECCOMP on the cancel tracee",
        ptrace(
            libc::PTRACE_SETOPTIONS as c_long,
            child,
            0,
            PTRACE_O_TRACESECCOMP as usize,
        ) == Ok(0),
    );
    r.check(
        "resuming the cancel tracee from its own stop",
        ptrace(libc::PTRACE_CONT as c_long, child, 0, 0) == Ok(0),
    );

    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    let stopped = n == child && libc::WIFSTOPPED(status);
    r.check(
        &format!(
            "the judged call still produces a seccomp stop to cancel at \
             (status {status:#x}: stopped={} event={})",
            libc::WIFSTOPPED(status),
            status >> 16,
        ),
        stopped && (status >> 16) == PTRACE_EVENT_SECCOMP,
    );
    if !stopped {
        return;
    }

    let mut regs = [0u64; REG_WORDS];
    r.check(
        "the tracer can read the tracee's registers at the cancel stop",
        ptrace(
            libc::PTRACE_GETREGS as c_long,
            child,
            0,
            regs.as_mut_ptr() as usize,
        ) == Ok(0),
    );
    r.check(
        &format!(
            "the number slot reads the call the tracee asked for (got {}, want {})",
            regs[NUMBER_SLOT],
            getpid_nr()
        ),
        regs[NUMBER_SLOT] == getpid_nr() as u64,
    );
    // The sentinel, in the machine's slot: `-1` as the all-ones word.
    regs[NUMBER_SLOT] = u64::MAX;
    r.check(
        "the tracer cancels the judged call by writing -1 at the seccomp stop",
        ptrace(
            libc::PTRACE_SETREGS as c_long,
            child,
            0,
            regs.as_ptr() as usize,
        ) == Ok(0),
    );

    let resumed = ptrace(libc::PTRACE_CONT as c_long, child, 0, 0) == Ok(0);
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the cancelled call does not run, and the tracee exits normally",
        resumed && n == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
}

fn main() {
    let mut r = Report::new();

    // === act one: the availability probe answers the truth, action by action
    // === Asked before a filter relies on an action, because Linux kills the
    // process that used one it does not know. Refusing is the safe half of the
    // answer; promising something not implemented is the part that kills.
    // The fourth column is the one designed divergence, and it is about the
    // *probe* rather than about the filter: this kernel has no supervisor to
    // summon, so it refuses `USER_NOTIF` at the availability check, where a host
    // Linux serves it. The refusal is the claim; a host run prints what its own
    // kernel answered and carries on, so the divergence is named rather than
    // hidden. (`USER_NOTIF`'s *effect* once installed - ENOSYS with no listener -
    // is measured below, and that one matches on both.)
    for (label, action, refused_with, host_may_serve) in [
        (
            "KILL_THREAD is implemented",
            libc::SECCOMP_RET_KILL_THREAD,
            None,
            false,
        ),
        (
            "KILL_PROCESS is implemented",
            libc::SECCOMP_RET_KILL_PROCESS,
            None,
            false,
        ),
        ("TRAP is implemented", libc::SECCOMP_RET_TRAP, None, false),
        ("ERRNO is implemented", libc::SECCOMP_RET_ERRNO, None, false),
        ("TRACE is implemented", libc::SECCOMP_RET_TRACE, None, false),
        ("LOG is implemented", libc::SECCOMP_RET_LOG, None, false),
        ("ALLOW is implemented", libc::SECCOMP_RET_ALLOW, None, false),
        (
            "USER_NOTIF is refused: no supervisor can be summoned",
            libc::SECCOMP_RET_USER_NOTIF,
            Some(Errno::OPNOTSUPP),
            true,
        ),
        (
            "an action naming nothing is refused",
            RET_UNKNOWN,
            Some(Errno::OPNOTSUPP),
            false,
        ),
    ] {
        let mut word = action;
        let got = seccomp(
            libc::SECCOMP_GET_ACTION_AVAIL as usize,
            0,
            &raw mut word as usize,
        );
        let ok = match (refused_with, got) {
            (None, Ok(0)) => true,
            (Some(want), Err(got)) if got == want => true,
            (_, Ok(0)) if host_may_serve && host_run() => {
                println!(
                    "note: this host's kernel serves `{label}`, where mpos has no \
                     supervisor to summon and refuses it"
                );
                true
            }
            _ => false,
        };
        let shown = format!("{label} (got {got:?}, want {refused_with:?})");
        r.check(if ok { label } else { &shown }, ok);
        r.check(
            "the probe leaves the caller's own word alone",
            word == action,
        );
    }
    // Measured on Linux: the comparison is over whole words, so data bits make
    // the request unrecognisable even though the action in them is supported.
    let mut word = libc::SECCOMP_RET_ERRNO | 1;
    match seccomp(
        libc::SECCOMP_GET_ACTION_AVAIL as usize,
        0,
        &raw mut word as usize,
    ) {
        Err(e) if e == Errno::OPNOTSUPP => r.check(
            "an action carrying errno data is refused like an unknown one",
            true,
        ),
        other => r.check(
            &format!(
                "an action carrying errno data is refused like an unknown one (got {other:?})"
            ),
            false,
        ),
    }
    match seccomp(
        libc::SECCOMP_GET_ACTION_AVAIL as usize,
        1,
        &raw mut word as usize,
    ) {
        Err(e) if e == Errno::INVAL => r.check(
            "the probe refuses a flags word it would have to ignore",
            true,
        ),
        other => r.check(
            &format!("the probe refuses a flags word it would have to ignore (got {other:?})"),
            false,
        ),
    }
    match seccomp(libc::SECCOMP_GET_ACTION_AVAIL as usize, 0, 0) {
        Err(e) if e == Errno::FAULT => r.check(
            "the probe answers EFAULT for a pointer it cannot read",
            true,
        ),
        other => r.check(
            &format!("the probe answers EFAULT for a pointer it cannot read (got {other:?})"),
            false,
        ),
    }

    // === act two: one action per child, and nothing else installed yet =======
    // Each of these children carries exactly one filter - the one it installed
    // itself - which is why they come before this process has any.
    r.check(
        "TRAP kills the caller with SIGSYS instead of the call happening",
        died_of(a_child_ends_with(libc::SECCOMP_RET_TRAP)) == libc::SIGSYS,
    );
    r.check(
        "KILL_PROCESS kills with SIGSYS, which is not SIGKILL",
        died_of(a_child_ends_with(libc::SECCOMP_RET_KILL_PROCESS)) == libc::SIGSYS,
    );
    r.check(
        "KILL_THREAD kills with SIGSYS too (no threads here to tell them apart)",
        died_of(a_child_ends_with(libc::SECCOMP_RET_KILL_THREAD)) == libc::SIGSYS,
    );
    // The one Linux learned the hard way: an action it has never heard of must
    // not be treated as permission.
    r.check(
        "an unrecognised action kills like KILL_PROCESS",
        died_of(a_child_ends_with(RET_UNKNOWN)) == libc::SIGSYS,
    );
    // And the two that refuse the call rather than killing anyone - measured on
    // Linux, where "no tracer" and "no listener" both fail it with ENOSYS.
    r.check(
        "TRACE with nobody attached answers ENOSYS and the caller lives",
        (a_child_ends_with(libc::SECCOMP_RET_TRACE) >> 8) == 40,
    );
    r.check(
        "USER_NOTIF with no listener answers ENOSYS and the caller lives",
        (a_child_ends_with(libc::SECCOMP_RET_USER_NOTIF) >> 8) == 40,
    );
    r.check(
        "LOG lets the call through, exactly as ALLOW does",
        (a_child_ends_with(libc::SECCOMP_RET_LOG) >> 8) == 41,
    );

    // === act two-and-a-half: the TRACE case with a tracer that asked =========
    // The complement of the "nobody tracing" case above: a tracer that set
    // `PTRACE_O_TRACESECCOMP` gets the event stop, and the call then runs. This
    // comes before act three because it needs a process with no filter of its
    // own to be the tracer, and a child that inherits none.
    seccomp_stop_reaches_a_tracer(&mut r);
    seccomp_stop_cancel_skips_the_call(&mut r);

    // === act three: install a filter here, and read what it does =============
    match install(getpid_nr(), libc::SECCOMP_RET_ERRNO | 1) {
        Ok(0) => r.check("installing a filter answers 0", true),
        other => r.check(
            &format!("installing a filter answers 0 (got {other:?})"),
            false,
        ),
    }
    // The judged call answers the filter's errno, not the table's.
    let got = raw_getpid();
    check_with(
        &mut r,
        "the judged call answers the filter's errno, not the table",
        denied(got, libc::EPERM),
        got,
    );
    check_with(
        &mut r,
        "a call the filter allows answers normally",
        raw_getppid().0 > 0,
        raw_getppid(),
    );
    // `PR_GET_SECCOMP`: the number the option answers with a filter installed.
    let mode = unsafe { libc::syscall(libc::SYS_prctl, libc::PR_GET_SECCOMP) };
    check_with(
        &mut r,
        "PR_GET_SECCOMP answers 2 once a filter is installed",
        mode == 2,
        mode,
    );

    // A second installation does not replace the first: the list is cumulative,
    // and each program in it judges whichever numbers it is written to judge.
    match install(getppid_nr(), libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32) {
        Ok(0) => r.check("a second filter installs on top of the first", true),
        other => r.check(
            &format!("a second filter installs on top of the first (got {other:?})"),
            false,
        ),
    }
    let got = raw_getpid();
    check_with(
        &mut r,
        "the first filter still judges its own call",
        denied(got, libc::EPERM),
        got,
    );
    let got = raw_getppid();
    check_with(
        &mut r,
        "the second filter judges a number the first spared",
        denied(got, libc::ENOSYS),
        got,
    );

    // Inheritance, in a child that installs nothing of its own.
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        let both = denied(raw_getpid(), libc::EPERM) && denied(raw_getppid(), libc::ENOSYS);
        unsafe { libc::_exit(if both { 40 } else { 41 }) };
    }
    let mut status = 0i32;
    unsafe { libc::waitpid(kid, &mut status, 0) };
    check_with(
        &mut r,
        "a child inherits both of its parent's filters",
        (status >> 8) == 40,
        status >> 8,
    );

    // === the door's two halves: the promise is what makes it a door ===
    // Linux lets a caller install a filter when it has `CAP_SYS_ADMIN` **or** has
    // set `PR_SET_NO_NEW_PRIVS`, and the second half is what makes the
    // confinement meaningful: a filter a caller could walk out of by exec'ing a
    // setuid program would not confine it. Measured on a current host, the
    // *negative* half could not be: this sandbox's own runtime sets the promise
    // on every process it starts, so a caller without it does not exist here -
    // which is why that claim is accepted on a host run and why the pin walks
    // both halves in the guest, where the child can drop to nobody with the
    // promise still unset.
    let door = rustix::pipe::pipe();
    if let Ok((tail, head)) = door {
        use std::os::fd::AsRawFd;
        let at = head.as_raw_fd();
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe {
                libc::setgid(65534);
                libc::setuid(65534);
            }
            let mut bits = 0i32;
            let bare = install(getpid_nr(), libc::SECCOMP_RET_ALLOW);
            if bare == Err(Errno::ACCESS) {
                bits |= 1;
            }
            let promised = unsafe {
                libc::syscall(
                    libc::SYS_prctl,
                    libc::PR_SET_NO_NEW_PRIVS,
                    1usize,
                    0usize,
                    0usize,
                    0usize,
                )
            } == 0;
            if promised {
                bits |= 2;
            }
            let armed = install(getpid_nr(), libc::SECCOMP_RET_ALLOW);
            if armed == Ok(0) {
                bits |= 4;
            }
            unsafe {
                libc::write(at, (&bits as *const i32).cast(), 4);
                libc::_exit(0);
            }
        }
        drop(head);
        let mut said = [0u8; 4];
        let n = rustix::io::read(&tail, &mut said).unwrap_or(0);
        let bits = i32::from_ne_bytes(said);
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        r.check(
            "a non-root caller without the promise is refused the filter",
            host_run() || bits & 1 != 0,
        );
        if host_run() && bits & 1 == 0 {
            println!(
                "note:  this host sets PR_SET_NO_NEW_PRIVS on every process, so its unprotected half cannot be asked"
            );
        }
        r.check(
            "PR_SET_NO_NEW_PRIVS is accepted there",
            n == 4 && bits & 2 != 0,
        );
        r.check(
            "and with it the same caller installs the filter",
            bits & 4 != 0,
        );
    } else {
        r.check("the seccomp door's report pipe is made", false);
    }

    // Stacking, which needs both filters in place: this child inherits
    // `ERRNO|1` on the judged call and adds a `TRAP` of its own, and it is the
    // *more* restrictive of the two that decides - so it dies of `SIGSYS`
    // rather than answering EPERM as the inherited filter alone would have it.
    r.check(
        "the most restrictive of several filters decides, not the newest",
        died_of(a_child_ends_with(libc::SECCOMP_RET_TRAP)) == libc::SIGSYS,
    );

    r.finish()
}
