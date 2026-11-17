//! t-ptrace-seize - `PTRACE_SEIZE` and `PTRACE_INTERRUPT`, the modern attach pair.
//!
//! This is the gap that stops Chromium's crash handler working. Crashpad, since
//! Linux 3.4, prefers `SEIZE`/`INTERRUPT` over `ATTACH`: `SEIZE` attaches
//! **without** stopping the tracee and `INTERRUPT` asks for a stop explicitly,
//! which is what lets a handler latch onto a live process without disturbing it.
//! Both requests were undefined here, so `ptrace`'s `_ => EIO` arm answered, and
//! crashpad concluded it could not inspect anything:
//!
//! ```text
//! [crashpad] ptrace: I/O error (5)   x12
//! [crashpad] thread ID 719 not found in process
//! ```
//!
//! Measured on Linux 7.3.4 (`build/scratch/seize/s.c`) before any claim:
//!
//! | what | Linux |
//! |---|---|
//! | `SEIZE(child, 0)` | `0`, and the child **keeps running** (`waitpid(WNOHANG)` is 0) |
//! | `INTERRUPT(child)` | `0`, and the child stops with **`SIGTRAP`** (`WSTOPSIG` 5) |
//! | `SEIZE` the same child twice | `EPERM` |
//! | `SEIZE` the caller itself | `EPERM` |
//! | `SEIZE` a pid naming nothing | `ESRCH` |
//! | `INTERRUPT` a task with no tracer | `ESRCH` |
//! | `SEIZE(child, PTRACE_O_TRACESYSGOOD)` | `0` - the options word is taken **at seize time** |
//! | `SEIZE(child, 0x200)` (unknown bit) | `EIO`, and no link is written |
//! | `INTERRUPT` an `ATTACH`ed (not seized) tracee | `EIO` |
//! | `INTERRUPT` a seized, running tracee | `status >> 8 == 0x8005` - `SIGTRAP` + `PTRACE_EVENT_STOP` |
//!
//! The last two rows are the second measurement pass
//! (`build/scratch/seize2/{p,q,r,t,v}.c`, same host): the refusals are two
//! different questions, and the interrupt stop is an *event* stop. The
//! `ATTACH`-era alternative is measured too - a child auto-attached from an
//! `ATTACH`ed tracee reports plain `SIGSTOP` (`0x13`), while the child of a
//! seized one is born with `0x8005` (`seize2/t.c`, `seize2/v.c`).
//!
//! **The `SIGTRAP` row is the one that separates the pair from `ATTACH`**, whose
//! stop is `SIGSTOP`. A tracer that cannot tell them apart cannot tell "I
//! attached" from "I asked you to pause".
//!
//! Numbers from libc's table; `rustix` binds no `ptrace`.

use mpos_user_tests::Report;

// **The request word's Rust type is the libc's, not the ABI's**: musl declares
// `ptrace(request: c_int, ...)` and glibc declares `c_uint`. One constant written
// as either fails the *other* build - which the gate caught, because the host
// suite builds against glibc and the guest against musl. Casting at the call site
// is what keeps one source file for both.
const PTRACE_ATTACH: u32 = 16;
const PTRACE_DETACH: u32 = 17;
const PTRACE_SEIZE: u32 = 0x4206;
const PTRACE_INTERRUPT: u32 = 0x4207;
const PTRACE_O_TRACESYSGOOD: usize = 1;
/// The event an interrupt stop carries in bits 23:16, and the word a tracer
/// compares `status >> 8` against - Linux's own discrimination of this stop
/// from a bare `SIGTRAP` (measured, `build/scratch/seize2/p.c`: `0x8005`).
const PTRACE_EVENT_STOP: i32 = 128;

fn ptrace(request: u32, pid: libc::pid_t, addr: usize, data: usize) -> isize {
    // SAFETY: the variadic shape libc's own wrapper takes; `request` decides
    // what the other three words mean. The `as` is the portable half: musl's
    // `c_int` and glibc's `c_uint` are the same four bytes.
    unsafe { libc::ptrace(request as _, pid, addr, data) as isize }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}

/// A child that **keeps making system calls**, which is what an interrupt stop
/// needs to land on.
///
/// A child sitting in `pause()` never returns to the syscall boundary this
/// kernel takes an interrupt stop at, so the first version of this fixture made
/// the pin hang: `INTERRUPT` answered 0 and the stop never arrived. That is a
/// property of *this kernel's* implementation (the stop is taken where a syscall
/// begins or ends, not by preempting a parked task) and a real tracer's subject -
/// a renderer, a compositor - is a process making calls constantly. The fixture
/// therefore loops on `getpid`, and the claim says what it measures.
fn busy_child() -> Option<libc::pid_t> {
    // SAFETY: `fork` in a test is the call under test's fixture.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        loop {
            // SAFETY: a cheap, side-effect-free call, taken many times a second.
            unsafe { libc::syscall(libc::SYS_getpid) };
        }
    }
    (pid > 0).then_some(pid)
}

fn main() {
    println!("t-ptrace-seize");
    let mut r = Report::new();

    // === SEIZE attaches without stopping ===================================
    let Some(child) = busy_child() else {
        r.check("a child is forked", false);
        r.finish();
    };
    std::thread::sleep(std::time::Duration::from_millis(120));

    let seized = ptrace(PTRACE_SEIZE, child, 0, 0);
    r.check(&format!("PTRACE_SEIZE is served ({seized})"), seized == 0);

    // **The claim that distinguishes SEIZE from ATTACH**: the tracee is still
    // running afterwards. `waitpid(WNOHANG)` answering 0 is the measurement.
    let mut status = 0;
    // SAFETY: `child` is this test's child and `status` is its own stack word.
    let early = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
    r.check(
        &format!("and the tracee KEEPS RUNNING ({early})"),
        early == 0,
    );

    // === INTERRUPT asks for a stop, and it is SIGTRAP ======================
    let interrupted = ptrace(PTRACE_INTERRUPT, child, 0, 0);
    r.check(
        &format!("PTRACE_INTERRUPT is served ({interrupted})"),
        interrupted == 0,
    );
    // SAFETY: as above; a blocking wait for the stop just requested.
    let stopped = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!("and the tracee stops ({stopped})"),
        stopped == child && libc::WIFSTOPPED(status),
    );
    // **`SIGTRAP`, not `SIGSTOP`** - the row that separates this pair from
    // `PTRACE_ATTACH`.
    let sig = if libc::WIFSTOPPED(status) {
        libc::WSTOPSIG(status)
    } else {
        -1
    };
    r.check(
        &format!("with SIGTRAP, not SIGSTOP ({sig})"),
        sig == libc::SIGTRAP,
    );
    // **And it is an *event* stop**, not a bare `SIGTRAP`: Linux reports
    // `status >> 8 == SIGTRAP | (PTRACE_EVENT_STOP << 8)`, and crashpad is one
    // of the tracers that reads that word to tell "I asked for this pause" from
    // a signal the tracee raised itself.
    r.check(
        &format!("carrying PTRACE_EVENT_STOP (status>>8=0x{:x})", status >> 8),
        (status >> 8) == (libc::SIGTRAP | (PTRACE_EVENT_STOP << 8)),
    );

    // === the refusals ======================================================
    r.check(
        "SEIZE of an already-traced child is EPERM",
        refused(ptrace(PTRACE_SEIZE, child, 0, 0), libc::EPERM),
    );
    r.check(
        "SEIZE of the caller itself is EPERM",
        refused(
            ptrace(PTRACE_SEIZE, unsafe { libc::getpid() }, 0, 0),
            libc::EPERM,
        ),
    );
    r.check(
        "SEIZE of a pid naming nothing is ESRCH",
        refused(ptrace(PTRACE_SEIZE, 99999, 0, 0), libc::ESRCH),
    );
    r.check("INTERRUPT of a task with no tracer is ESRCH", {
        let Some(stray) = busy_child() else {
            r.check("a second child is forked", false);
            r.finish();
        };
        std::thread::sleep(std::time::Duration::from_millis(120));
        let rc = ptrace(PTRACE_INTERRUPT, stray, 0, 0);
        // SAFETY: the child this block forked.
        unsafe {
            libc::kill(stray, libc::SIGKILL);
            libc::waitpid(stray, core::ptr::null_mut(), 0);
        }
        refused(rc, libc::ESRCH)
    });

    // === the two refusals that tell the doors apart =======================
    //
    // A bit the kernel does not know is `EIO` at `SEIZE`, not `SETOPTIONS`'
    // `EINVAL`: Linux judges the *request* before the options table (measured,
    // `build/scratch/seize2/p.c`). `0x200` is a word no `PTRACE_O_*` names.
    if let Some(unknown) = busy_child() {
        std::thread::sleep(std::time::Duration::from_millis(120));
        r.check(
            "SEIZE with an unknown option bit is EIO",
            refused(ptrace(PTRACE_SEIZE, unknown, 0, 0x200), libc::EIO),
        );
        // ... and the refusal wrote no link: the same child seizes cleanly now.
        let rc = ptrace(PTRACE_SEIZE, unknown, 0, 0);
        r.check(
            &format!("and the refused SEIZE left no tracer ({rc})"),
            rc == 0,
        );
        // SAFETY: the child this block forked.
        unsafe {
            libc::kill(unknown, libc::SIGKILL);
            libc::waitpid(unknown, core::ptr::null_mut(), 0);
        }
    }

    // `ATTACH` then `INTERRUPT` is `EIO` on Linux: the request only means
    // something for a tracee seized with `SEIZE` (measured,
    // `build/scratch/seize2/p.c`). The untraced case above is `ESRCH`, so the
    // two refusals are distinct questions and the pin holds both.
    if let Some(attached) = busy_child() {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let armed = ptrace(PTRACE_ATTACH, attached, 0, 0);
        // SAFETY: the child this block forked, and its own stop word. The wait
        // is guarded by the attach's answer: a failed attach leaves the child
        // running, and an unguarded `waitpid` would spend the program's budget
        // instead of failing a claim.
        let mut st = 0;
        let stopped = if armed == 0 {
            unsafe { libc::waitpid(attached, &mut st, 0) }
        } else {
            -1
        };
        r.check(
            &format!(
                "ATTACH stops the tracee as before ({armed}, {stopped}, stopped={})",
                libc::WIFSTOPPED(st)
            ),
            armed == 0 && stopped == attached && libc::WIFSTOPPED(st),
        );
        r.check(
            "INTERRUPT of an ATTACHed (not seized) tracee is EIO",
            refused(ptrace(PTRACE_INTERRUPT, attached, 0, 0), libc::EIO),
        );
        // SAFETY: detaching resumes the child; then it is killed and reaped.
        unsafe {
            ptrace(PTRACE_DETACH, attached, 0, 0);
            libc::kill(attached, libc::SIGKILL);
            libc::waitpid(attached, core::ptr::null_mut(), 0);
        }
    }

    // === the options word is taken at seize time ===========================
    //
    // `ATTACH` takes no options and `SETOPTIONS` sets them afterwards; `SEIZE`
    // takes them in its `data` word, which is a shape a tracer depends on.
    if let Some(second) = busy_child() {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let rc = ptrace(PTRACE_SEIZE, second, 0, PTRACE_O_TRACESYSGOOD);
        r.check(
            &format!("SEIZE with an options word is served ({rc})"),
            rc == 0,
        );
        // SAFETY: the child this block forked, and its stop/exit word.
        unsafe {
            libc::kill(second, libc::SIGKILL);
            libc::waitpid(second, core::ptr::null_mut(), 0);
        }
    }

    // Clean up the first child: it is stopped, so it needs resuming first.
    // SAFETY: `child` is this test's, and the tracer detaches by killing.
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, core::ptr::null_mut(), 0);
    }

    r.finish()
}
