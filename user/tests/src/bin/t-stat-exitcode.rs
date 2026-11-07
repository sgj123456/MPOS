//! `/proc/<pid>/stat`'s `exit_code` (field 52) - the encoded wait status of a
//! zombie, or `0` while the task (or its group) is still alive.
//!
//! Measured on a current host (`build/scratch/statmore/q.c`, Linux 7.2.14,
//! `ulimit -c 0`): a child that calls `exit(42)` and is not yet reaped reads
//! `10752` (`42 << 8`) and `waitpid` reports that same word; `SIGTERM` reads
//! `15`, `SIGKILL` `9`, and `SIGSEGV` `139` (`WTERMSIG 11 | WCOREDUMP 128`),
//! each identical to the `waitpid` status. The core-dump bit follows the
//! signal and the process's dumpability, not whether a core file was written.
//! A live task, and a group whose leader has exited while a sibling still
//! runs (`build/scratch/statmore/r.c`), both read `0`.
//!
//! Field 52 is a promise about *which* word: Linux's `wait4` status, not a
//! raw exit code and not a flag. It is also a promise about *whose* word: the
//! zombie being read, not the reader. Until `docs/notes/` §6.112 this kernel
//! printed a borrowed `"0"` for every task, so a dead child and its live
//! parent were indistinguishable. Each death below therefore asserts the
//! field's own value *and* its agreement with `waitpid`; a literal `0` fails
//! every one, and an implementation that printed the raw `code` (42 rather
//! than 10752), or forgot the core bit, fails the agreement claim.

use mpos_user_tests::Report;

/// Everything after the command name's last `)`. `/proc/<pid>/stat` lets
/// `comm` contain spaces and parentheses, so the numbered fields begin here;
/// the first token is field 3 (`state`).
fn stat_tail(text: &str) -> Option<&str> {
    Some(&text[text.rfind(')')? + 1..])
}

/// A numbered field parsed as `u64`. Unlike `t-starttime`'s `i64` parser this
/// also carries the infinity-shaped words elsewhere in `stat`, though field 52
/// itself is always a small non-negative status word.
fn stat_field(text: &str, field: usize) -> Option<u64> {
    let mut tokens = stat_tail(text)?.split_whitespace();
    let mut index = 3;
    loop {
        let token = tokens.next()?;
        if index == field {
            return token.parse().ok();
        }
        index += 1;
    }
}

fn exit_code_of(path: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    stat_field(&text, 52)
}

/// Field 3, the task's state letter. A zombie is `Z`.
fn state_of(path: &str) -> Option<char> {
    let text = std::fs::read_to_string(path).ok()?;
    stat_tail(&text)?.split_whitespace().next()?.chars().next()
}

fn path_of(pid: i32) -> String {
    format!("/proc/{pid}/stat")
}

/// How the child is to die. Each is a *process-directed* death so the whole
/// group ends and the zombie's status is the group's status.
#[derive(Clone, Copy)]
enum Death {
    Exit42,
    SigTerm,
    SigKill,
    SigSegv,
}

impl Death {
    fn signal(self) -> Option<i32> {
        match self {
            Death::Exit42 => None,
            Death::SigTerm => Some(libc::SIGTERM),
            Death::SigKill => Some(libc::SIGKILL),
            Death::SigSegv => Some(libc::SIGSEGV),
        }
    }
}

/// Fork a child that resets the four dispositions to default, unblocks them,
/// announces readiness on a pipe, then dies the requested way. Returns the
/// child's pid and the pipe's read end. The readiness word matters: the parent
/// must not read `/proc/<pid>/stat` before the fork has even happened, and it
/// must not reap before reading the zombie (reaping removes the file).
fn spawn_zombie(death: Death) -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: two ints this process owns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `fork` in a single-threaded test process; the child touches
    // only async-signal-safe calls before it dies.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // SAFETY: dispositions, mask, one pipe write, then a death. No
        // destructor, no allocator, no back into Rust.
        unsafe {
            libc::close(fds[0]);
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            for sig in [libc::SIGTERM, libc::SIGKILL, libc::SIGSEGV, libc::SIGABRT] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in [libc::SIGTERM, libc::SIGSEGV] {
                libc::sigaddset(&mut set, sig);
            }
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());

            let ready: u32 = 1;
            let bytes = ready.to_ne_bytes();
            libc::write(fds[1], bytes.as_ptr().cast(), bytes.len());

            match death {
                Death::Exit42 => libc::_exit(42),
                Death::SigTerm | Death::SigKill | Death::SigSegv => {
                    // A process-directed signal ends the group; nothing is
                    // reached after it.
                    libc::kill(libc::getpid(), death.signal().unwrap());
                    libc::_exit(0);
                }
            }
        }
    }
    // SAFETY: the parent closes only the write end it no longer needs.
    unsafe { libc::close(fds[1]) };
    Some((pid, fds[0]))
}

/// Wait (without reaping) until `/proc/<pid>/stat` reports `Z`. Reading the
/// file does not collect the child, so the zombie - and its field 52 - stays
/// readable for the claims that follow.
fn wait_zombie(pid: i32) -> bool {
    for _ in 0..400 {
        if state_of(&path_of(pid)) == Some('Z') {
            return true;
        }
        let spec = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        // SAFETY: a one-millisecond sleep with no remainder pointer.
        unsafe { libc::nanosleep(&spec, std::ptr::null_mut()) };
    }
    false
}

/// Read one death end-to-end: the zombie's field 52, the parent's own field
/// while that zombie exists, and the `waitpid` status that finally reaps it.
struct Reading {
    field: Option<u64>,
    parent_field: Option<u64>,
    wait_status: u64,
    became_zombie: bool,
}

fn read_death(death: Death) -> Option<Reading> {
    let (pid, read_fd) = spawn_zombie(death)?;
    let mut buf = [0u8; 4];
    // SAFETY: one readiness word from the child's pipe.
    let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
    unsafe { libc::close(read_fd) };
    if n != 4 {
        return None;
    }

    let became_zombie = wait_zombie(pid);
    let field = exit_code_of(&path_of(pid));
    let parent_field = exit_code_of("/proc/self/stat");

    let mut status = 0i32;
    // SAFETY: reap exactly the child we forked; it is already a zombie.
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    Some(Reading {
        field,
        parent_field,
        wait_status: status as u32 as u64,
        became_zombie: became_zombie && reaped == pid,
    })
}

/// Keep a SIGSEGV on the host from writing a core file. The `WCOREDUMP` bit
/// is still set in the status word - that is what the agreement claim reads -
/// so this changes no answer, only the host's disk.
fn no_core_files() {
    let rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a zero `RLIMIT_CORE` for this test process alone.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &rl) };
}

fn main() {
    no_core_files();
    let mut r = Report::new();

    let live = exit_code_of("/proc/self/stat");
    r.check("stat prints field 52 as a u64 word", live.is_some());
    r.check(
        &format!("a live task's field 52 is zero ({live:?})"),
        live == Some(0),
    );

    // `exit(42)`: the absolute Linux word, and the agreement with waitpid.
    // A borrowed "0" fails both; a raw `code` (42) fails the first.
    let exit = read_death(Death::Exit42);
    let (ef, es, ep, ez) = exit.as_ref().map_or((None, 0, None, false), |x| {
        (x.field, x.wait_status, x.parent_field, x.became_zombie)
    });
    r.check(
        &format!("exit(42) leaves 10752 in the zombie's field 52 ({ef:?})"),
        ez && ef == Some(10752),
    );
    r.check(
        &format!("and that is the word waitpid reports ({ef:?} against {es})"),
        ef == Some(es) && es == 10752,
    );
    r.check(
        &format!("while the living parent's own field 52 stays zero ({ep:?})"),
        ep == Some(0),
    );

    // Signal deaths: the low seven bits are the signal, with the core bit
    // where Linux puts it. Each is asserted against waitpid so the pin does
    // not depend on the host's core-dump configuration.
    let term = read_death(Death::SigTerm);
    let (tf, ts, tz) = term.as_ref().map_or((None, 0, false), |x| {
        (x.field, x.wait_status, x.became_zombie)
    });
    r.check(
        &format!("SIGTERM leaves 15 in the zombie's field 52 ({tf:?})"),
        tz && tf == Some(15),
    );
    r.check(
        &format!("and that is the word waitpid reports ({tf:?} against {ts})"),
        tf == Some(ts) && ts == 15,
    );

    let kill = read_death(Death::SigKill);
    let (kf, ks, kz) = kill.as_ref().map_or((None, 0, false), |x| {
        (x.field, x.wait_status, x.became_zombie)
    });
    r.check(
        &format!("SIGKILL leaves 9 in the zombie's field 52 ({kf:?})"),
        kz && kf == Some(9),
    );
    r.check(
        &format!("and that is the word waitpid reports ({kf:?} against {ks})"),
        kf == Some(ks) && ks == 9,
    );

    // SIGSEGV is where the core bit lives. The absolute value is 139 on a
    // dumpable process, but the agreement with waitpid is the claim that
    // survives any host configuration, so both are asserted.
    let segv = read_death(Death::SigSegv);
    let (sf, ss, sz) = segv.as_ref().map_or((None, 0, false), |x| {
        (x.field, x.wait_status, x.became_zombie)
    });
    r.check(
        &format!("SIGSEGV leaves a signal-death word in field 52 ({sf:?})"),
        sz && sf.is_some() && sf != Some(0),
    );
    r.check(
        &format!("and that is exactly the word waitpid reports ({sf:?} against {ss})"),
        sf == Some(ss) && ss != 0,
    );

    r.finish();
}
