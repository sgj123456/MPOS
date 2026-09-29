//! t-seccomp - a `SECCOMP_MODE_FILTER` program, judged live on this machine.
//!
//! `seccomp.rs`'s host tests can run a filter against a made-up syscall block,
//! and this file is about the half they cannot reach: the answer a *real* call
//! gets, decided by the gate in `syscall::linux::dispatch` before the table ever
//! runs. Each pin below corresponds to a measurement taken on Linux first
//! (PLAN.md §8.2 holds them); several are not what the man page sounds like,
//! which is why they are here rather than in a comment:
//!
//! * a kill action (`KILL_THREAD`, `KILL_PROCESS`) ends the task with
//!   **`SIGSYS`**, not `SIGKILL`;
//! * an action the kernel does not recognise is treated as `KILL_PROCESS`;
//! * `TRACE` with nobody tracing fails the call with `ENOSYS` rather than
//!   allowing it, as does `USER_NOTIF` with no listener;
//! * the availability probe compares whole words, so asking about `ERRNO|1` is
//!   a refusal even though asking about `ERRNO` is not.
//!
//! Ordering matters here, and it is the reason the file reads in three acts. A
//! filter cannot be un-installed and is inherited across `fork`, so every child
//! carries its parent's list: the children that pin one action's answer run
//! **before** this process installs anything, and the child that pins
//! inheritance-and-stacking runs after. Getting that wrong does not fail loudly
//! - it makes the wrong filter the one being answered, because when two
//! programs judge the same call the more restrictive wins, and `ERRNO`
//! outranks every action below it except the kills and `TRAP`.
//!
//! One care taken throughout: each filter judges exactly one number (`getpid`)
//! and allows everything else, so installing one does not silence this test's
//! own output. The drafts that judged every call measured their own collapse
//! instead of the kernel.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The judged number, and the one the second filter below takes instead.
const GETPID: u32 = 39;
const GETPPID: u32 = 110;

// classic BPF opcodes (`linux/filter.h`), the three this needs.
const LD_W_ABS: u16 = 0x20;
const JMP_JEQ_K: u16 = 0x15;
const RET_K: u16 = 0x06;

// The actions, in the high half of a filter's return value.
const RET_KILL_THREAD: u32 = 0x0000_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_USER_NOTIF: u32 = 0x7fc0_0000;
const RET_TRACE: u32 = 0x7ff0_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

/// An action naming nothing at all.
const RET_UNKNOWN: u32 = 0x1234_0000;

/// `SIGSYS`, the signal every seccomp kill delivers - see §8.2 for the
/// measurement that replaced `SIGKILL` with it.
const SIGSYS: i32 = 31;

/// Eight bytes, as `struct sock_filter` lays them out.
fn insn(code: u16, jt: u8, jf: u8, k: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&code.to_le_bytes());
    out[2] = jt;
    out[3] = jf;
    out[4..8].copy_from_slice(&k.to_le_bytes());
    out
}

/// Judge one number with `action` and let every other call through.
///
/// The shape `libseccomp` emits: load the number, compare it, answer. The false
/// path skipping over one instruction is what makes this a filter rather than a
/// verdict on everything.
fn judge(nr: u32, action: u32) -> [[u8; 8]; 4] {
    [
        insn(LD_W_ABS, 0, 0, 0),
        insn(JMP_JEQ_K, 0, 1, nr),
        insn(RET_K, 0, 0, action),
        insn(RET_K, 0, 0, RET_ALLOW),
    ]
}

/// `struct sock_fprog`: a length counting *instructions*, padding, then the
/// pointer. `repr(C)` puts the pointer at 8, which is where the kernel reads it.
#[repr(C)]
struct Prog {
    len: u16,
    insns: *const [[u8; 8]; 4],
}

fn install(nr: u32, action: u32) -> isize {
    let program = judge(nr, action); // lives until the install returns
    let prog = Prog {
        len: 4,
        insns: &program,
    };
    lin::seccomp(1, 0, &prog as *const Prog as usize)
}

/// The low seven bits of a status word name the terminating signal; 0x7f there
/// is a *stop* rather than a death, and 0 means it simply exited.
fn died_of(status: i32) -> i32 {
    let sig = status & 0x7f;
    if sig == 0 || sig == 0x7f { 0 } else { sig }
}

/// How a child ends after installing `action` and reaching for the judged call,
/// where the only two things it can do are die or answer. 70 means its install
/// failed, 40 means it answered `ENOSYS` and survived, 41 means it got an
/// ordinary answer and survived.
fn a_child_ends_with(action: u32) -> i32 {
    let kid = lin::fork();
    if kid == 0 {
        if install(GETPID, action) != 0 {
            lin::exit(70);
        }
        let got = lin::getpid();
        lin::exit(if got == lin::ENOSYS { 40 } else { 41 });
    }
    let mut status = 0i32;
    lin::waitpid(kid as usize, &mut status, 0);
    status
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-seccomp\n");
    let mut failed = false;

    // === act one: the availability probe answers the truth, action by action
    // === Asked before a filter relies on an action, because Linux kills the
    // process that used one it does not know. Refusing is the safe half of the
    // answer; promising something not implemented is the part that kills.
    for (label, action, want) in [
        (&b"KILL_THREAD is implemented"[..], RET_KILL_THREAD, 0isize),
        (&b"KILL_PROCESS is implemented"[..], RET_KILL_PROCESS, 0),
        (&b"TRAP is implemented"[..], RET_TRAP, 0),
        (&b"ERRNO is implemented"[..], RET_ERRNO, 0),
        (&b"TRACE is implemented"[..], RET_TRACE, 0),
        (&b"LOG is implemented"[..], RET_LOG, 0),
        (&b"ALLOW is implemented"[..], RET_ALLOW, 0),
        (
            &b"USER_NOTIF is refused: no supervisor can be summoned"[..],
            RET_USER_NOTIF,
            lin::EOPNOTSUPP,
        ),
        (
            &b"an action naming nothing is refused"[..],
            RET_UNKNOWN,
            lin::EOPNOTSUPP,
        ),
    ] {
        let mut word = action;
        let got = lin::seccomp(2, 0, &raw mut word as usize);
        lin::check_eq(&mut failed, label, got, want);
        lin::check_true(
            &mut failed,
            b"the probe leaves the caller's own word alone",
            word == action,
        );
    }
    // Measured on Linux: the comparison is over whole words, so data bits make
    // the request unrecognisable even though the action in them is supported.
    let mut word = RET_ERRNO | 1;
    lin::check_eq(
        &mut failed,
        b"an action carrying errno data is refused like an unknown one",
        lin::seccomp(2, 0, &raw mut word as usize),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"the probe refuses a flags word it would have to ignore",
        lin::seccomp(2, 1, &raw mut word as usize),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"the probe answers EFAULT for a pointer it cannot read",
        lin::seccomp(2, 0, 0),
        lin::EFAULT,
    );

    // === act two: one action per child, and nothing else installed yet =======
    // Each of these children carries exactly one filter - the one it installed
    // itself - which is why they come before this process has any.
    lin::check_eq(
        &mut failed,
        b"TRAP kills the caller with SIGSYS instead of the call happening",
        died_of(a_child_ends_with(RET_TRAP)) as isize,
        SIGSYS as isize,
    );
    lin::check_eq(
        &mut failed,
        b"KILL_PROCESS kills with SIGSYS, which is not SIGKILL",
        died_of(a_child_ends_with(RET_KILL_PROCESS)) as isize,
        SIGSYS as isize,
    );
    lin::check_eq(
        &mut failed,
        b"KILL_THREAD kills with SIGSYS too (no threads here to tell them apart)",
        died_of(a_child_ends_with(RET_KILL_THREAD)) as isize,
        SIGSYS as isize,
    );
    // The one Linux learned the hard way: an action it has never heard of must
    // not be treated as permission.
    lin::check_eq(
        &mut failed,
        b"an unrecognised action kills like KILL_PROCESS",
        died_of(a_child_ends_with(RET_UNKNOWN)) as isize,
        SIGSYS as isize,
    );
    // And the two that refuse the call rather than killing anyone - measured on
    // Linux, where "no tracer" and "no listener" both fail it with ENOSYS.
    lin::check_eq(
        &mut failed,
        b"TRACE with nobody attached answers ENOSYS and the caller lives",
        (a_child_ends_with(RET_TRACE) >> 8) as isize,
        40,
    );
    lin::check_eq(
        &mut failed,
        b"USER_NOTIF with no listener answers ENOSYS and the caller lives",
        (a_child_ends_with(RET_USER_NOTIF) >> 8) as isize,
        40,
    );
    lin::check_eq(
        &mut failed,
        b"LOG lets the call through, exactly as ALLOW does",
        (a_child_ends_with(RET_LOG) >> 8) as isize,
        41,
    );

    // === act three: install a filter here, and read what it does =============
    lin::check_eq(
        &mut failed,
        b"installing a filter answers 0",
        install(GETPID, RET_ERRNO | 1),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the judged call answers the filter's errno, not the table",
        lin::getpid(),
        lin::EPERM,
    );
    lin::check_true(
        &mut failed,
        b"a call the filter allows answers normally",
        lin::getppid() > 0,
    );
    lin::check_eq(
        &mut failed,
        b"PR_GET_SECCOMP answers 2 once a filter is installed",
        lin::prctl(lin::PR_GET_SECCOMP, 0, 0, 0, 0),
        2,
    );

    // A second installation does not replace the first: the list is cumulative,
    // and each program in it judges whichever numbers it is written to judge.
    lin::check_eq(
        &mut failed,
        b"a second filter installs on top of the first",
        install(GETPPID, RET_ERRNO | 38),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the first filter still judges its own call",
        lin::getpid(),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"the second filter judges a number the first spared",
        lin::getppid(),
        lin::ENOSYS,
    );

    // Inheritance, in a child that installs nothing of its own.
    {
        let kid = lin::fork();
        if kid == 0 {
            let pid = lin::getpid();
            let ppid = lin::getppid();
            lin::exit(if pid == lin::EPERM && ppid == lin::ENOSYS {
                40
            } else {
                41
            });
        }
        let mut status = 0i32;
        lin::waitpid(kid as usize, &mut status, 0);
        lin::check_eq(
            &mut failed,
            b"a child inherits both of its parent's filters",
            (status >> 8) as isize,
            40,
        );
    }

    // Stacking, which needs both filters in place: this child inherits
    // `ERRNO|1` on the judged call and adds a `TRAP` of its own, and it is the
    // *more* restrictive of the two that decides - so it dies of `SIGSYS`
    // rather than answering EPERM as the inherited filter alone would have it.
    lin::check_eq(
        &mut failed,
        b"the most restrictive of several filters decides, not the newest",
        died_of(a_child_ends_with(RET_TRAP)) as isize,
        SIGSYS as isize,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-seccomp: panicked\n")
}
