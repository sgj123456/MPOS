//! `/proc/<pid>/stat`'s field 9 is a task-state bitmask, not a literal zero.
//!
//! The field was a borrowed `"0"`. What Linux answers, measured on a current
//! host (Linux 7.2.14, `build/scratch/statflags2/`), read twice - once with
//! address randomisation on and once under `setarch -R`, which is the only way
//! to tell a bit that describes the machine from a bit that describes the task:
//!
//! ```text
//! flags2.c, ASLR on            flags4.c, ASLR off (setarch -R)
//!   self (exec'd)   0x400000     self            0x0
//!   fork, no exec   0x400040     fork, no exec   0x40
//!   zombie exit(0)  0x40804c     zombie exit(0)  0x804c
//!   zombie SIGTERM  0x40844c     zombie SIGTERM  0x844c
//!   zombie SIGSEGV  0x40864c     zombie SIGSEGV  0x864c
//! ```
//!
//! Two facts fall out of reading the same states twice:
//!
//! * **`0x400000` is the machine's, not the task's** - it disappears under
//!   `setarch -R` and nothing else changes. `flags5.c` confirms it a third way:
//!   a child that `execl`ed and exited reads `0x40800c` with ASLR and `0x800c`
//!   without. So a claim that compared a whole word against a constant would be
//!   a claim about this host's sysctl and would go red on a machine that boots
//!   with `norandmaps`. **Every claim below is therefore a relation between two
//!   words this test read from the same machine.**
//! * The state bits are stable across both runs, which is what makes them
//!   assertable: `0x40` is set exactly for a task that was made by
//!   `fork`/`clone` and has not `exec`ed since, and the death bits are set
//!   exactly for a task that is over.
//!
//! `flags3.c` is the thread reading: a group's leader reports the plain word and
//! a worker thread beside it reports `0x400040`, because a thread is made by
//! `clone` and never calls `exec` - so the fork bit is a fact about how *this*
//! task came to be and not about its group.
//!
//! The two bits that are named here are the ones a program branches on. The
//! measurement also shows a third (`0x8000`) in every corpse and no live task;
//! this kernel reports it, and `docs/notes/` §6.131 records that its Linux name
//! is the part this repository could not check.

use mpos_user_tests::Report;

const PF_FORKNOEXEC: u64 = 0x0000_0040;
const PF_DUMPCORE: u64 = 0x0000_0200;
const PF_SIGNALED: u64 = 0x0000_0400;
const PF_RANDOMIZE: u64 = 0x0040_0000;
/// The bits every corpse carries and no live task does (`flags4.c`'s `0x800c`
/// with the fork bit taken out; `0x4 | 0x8 | 0x8000`).
const PF_DEAD: u64 = 0x0000_800c;

/// Field `n` of a `/proc` `stat` line: the zero-based index `n - 3` of the
/// whitespace split that starts after the parenthesised `comm`, because `comm`
/// is the only field that may contain spaces and field 1/2 are before it.
fn field(text: &str, n: usize) -> Option<i64> {
    let after = text.rsplit_once(')')?.1;
    after.split_whitespace().nth(n - 3)?.parse().ok()
}

fn flags_of(pid: i32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(field(&text, 9)? as u64)
}

/// The `task/<tid>/` spelling, which is a different file for the same task.
fn flags_of_task(pid: i32, tid: i32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    Some(field(&text, 9)? as u64)
}

/// Field 9 of a child that has just died, read **before** anything reaps it.
///
/// This is why the death claims need their own helper: a `waitpid` takes the
/// `/proc` entry away with it, so a word read after the wait describes nothing.
/// The loop waits for the file to say `Z` rather than sleeping a guessed while.
fn zombie_word(pid: i32) -> Option<u64> {
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = text
                .rsplit_once(')')
                .map(|(_, after)| after.trim_start().chars().next());
            if state == Some(Some('Z')) {
                return field(&text, 9).map(|w| w as u64);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    None
}

/// Fork a child, let it announce itself on a pipe, and report what the parent
/// read while the child was in that state.
///
/// The pipe carries one byte the moment the child's state is established, so
/// the parent's read describes that state rather than the instant after it
/// changed - the race every one of these fixtures would otherwise have.
fn child_in_state(body: impl FnOnce(i32)) -> (i32, Option<u64>) {
    let mut pipe = [0i32; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return (-1, None);
    }
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::close(pipe[0]);
            body(pipe[1]);
            libc::_exit(0);
        }
    }
    let mut ready = [0u8; 1];
    unsafe {
        libc::close(pipe[1]);
        libc::read(pipe[0], ready.as_mut_ptr() as *mut libc::c_void, 1);
        libc::close(pipe[0]);
    }
    let reading = flags_of(child);
    (child, reading)
}

fn main() {
    let mut r = Report::new();
    let me = unsafe { libc::getpid() };

    // The defect itself: this program was `exec`ed by the test runner, so its
    // word is not zero on any machine - the layout bit may or may not be there,
    // but a literal `0` is wrong either way.
    let mine = flags_of(me);
    r.check(
        &format!("an execed process's flags is not the literal zero ({mine:?})"),
        matches!(mine, Some(w) if w != 0),
    );

    // And it does not claim to have been forked without an exec: this task was
    // exec'd, so the bit has to be clear. That is the half a kernel which set
    // the bit for every process would fail while still passing the check above.
    r.check(
        &format!("and it does not claim to be forked-and-not-execed ({mine:?})"),
        matches!(mine, Some(w) if w & PF_FORKNOEXEC == 0),
    );

    // `fork` without `exec`: the bit goes on, and the only other change is the
    // machine's layout bit, which both words have or both lack. The child stays
    // alive until the read is done - a child that exits on announcing would be
    // read as a corpse and carry the death bits too.
    let (forked_pid, forked) = child_in_state(|ready| unsafe {
        libc::write(ready, b"\0".as_ptr() as *const libc::c_void, 1);
        loop {
            libc::pause();
        }
    });
    unsafe {
        libc::kill(forked_pid, libc::SIGKILL);
        libc::waitpid(forked_pid, core::ptr::null_mut(), 0);
    }
    r.check(
        &format!("a forked child that has not execed carries PF_FORKNOEXEC (pid {forked_pid}, {forked:?})"),
        matches!((forked, mine), (Some(child), Some(parent)) if child & PF_FORKNOEXEC != 0 && parent & PF_FORKNOEXEC == 0),
    );
    r.check(
        &format!("and the fork bit is the only state bit it added ({forked:?} vs {mine:?})"),
        matches!((forked, mine), (Some(child), Some(parent)) if child & !(PF_FORKNOEXEC | PF_RANDOMIZE) == parent & !PF_RANDOMIZE),
    );

    // A corpse: `fork` + `_exit(0)`, read while it is a zombie. The death bits
    // appear, `PF_FORKNOEXEC` stays (it never execed), and nothing says a
    // signal did it.
    //
    // Not `child_in_state`: that reads the file the moment the child announces,
    // which here would be before the exit. This one waits for the state itself,
    // retrying until the file says `Z`, so the claim cannot race the death.
    let (zombie_pid, zombie) = {
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe { libc::_exit(0) };
        }
        let mut word = None;
        for _ in 0..500 {
            if let Ok(text) = std::fs::read_to_string(format!("/proc/{child}/stat")) {
                let state = text
                    .rsplit_once(')')
                    .map(|(_, after)| after.trim_start().chars().next());
                if state == Some(Some('Z')) {
                    word = field(&text, 9).map(|w| w as u64);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        unsafe { libc::waitpid(child, core::ptr::null_mut(), 0) };
        (child, word)
    };
    r.check(
        &format!("a zombie carries the death bits (pid {zombie_pid}, {zombie:?})"),
        matches!(zombie, Some(w) if w & PF_DEAD == PF_DEAD),
    );
    r.check(
        &format!("and the corpse of a returned child is not signalled ({zombie:?})"),
        matches!(zombie, Some(w) if w & PF_SIGNALED == 0),
    );
    r.check(
        &format!("and a live process does not carry them ({mine:?})"),
        matches!(mine, Some(w) if w & PF_DEAD == 0),
    );

    // Killed by a signal that does not dump: `PF_SIGNALED` on top of the death
    // bits, no `PF_DUMPCORE`. The file is read while the child is a zombie -
    // before the `waitpid` that would take its `/proc` entry away.
    let term = {
        let mut pipe = [0i32; 2];
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            None
        } else {
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe {
                    libc::close(pipe[0]);
                    libc::write(pipe[1], b"\0".as_ptr() as *const libc::c_void, 1);
                    // Wait to be killed, so the death is the signal's.
                    loop {
                        libc::pause();
                    }
                }
            }
            let mut ready = [0u8; 1];
            unsafe {
                libc::close(pipe[1]);
                libc::read(pipe[0], ready.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(pipe[0]);
                libc::kill(child, libc::SIGTERM);
            }
            let word = zombie_word(child);
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, 0) };
            word
        }
    };
    r.check(
        &format!("a child killed by SIGTERM carries PF_SIGNALED ({term:?})"),
        matches!(term, Some(w) if w & PF_SIGNALED == PF_SIGNALED),
    );
    r.check(
        &format!("and not PF_DUMPCORE, because SIGTERM does not dump ({term:?})"),
        matches!(term, Some(w) if w & PF_DUMPCORE == 0),
    );

    // Killed by a signal that dumps: the same word plus `PF_DUMPCORE`. This is
    // the pair that pins the dump bit to the *signal* rather than to the death:
    // both children are dead, and exactly one of them dumped.
    //
    // Two traps, both paid for. The signal is sent from here exactly as
    // `SIGTERM` is above, so the fixtures differ in one thing only: a child
    // that called `raise` on itself reported `0x40804c` - a normal exit -
    // because `raise` returned and the `_exit` behind it ran. And the child
    // lowers `RLIMIT_CORE` to zero before it announces, because a core file
    // this test does not want is a core file written to this host's
    // `core_pattern` - a `systemd-coredump` pipe here, which took longer than
    // this binary's whole budget and read as a hang.
    //
    // `build/scratch/statflags2/nolimit.c` measured the bit does not depend on
    // that limit: `SIGSEGV` with `RLIMIT_CORE` unlimited and with `0` both read
    // `0x40864c` and both report `WCOREDUMP`, so the bit describes the signal's
    // dumpability and not whether a file was written - the same rule field 52
    // already follows (`crate::abi::wait_status`, §6.112).
    //
    // The third trap is the one that read as a hang rather than as a wrong
    // word. `fork` copies signal *dispositions*, and Rust's std runtime
    // installs a `SIGSEGV` handler of its own (the stack-overflow guard), so
    // this child inherited one: the `SIGSEGV` below was caught, the handler
    // returned, and the child went back to `pause()` - alive, so `zombie_word`
    // polled its budget out and the `waitpid` after it blocked forever. A
    // fixture that wants the default death has to ask for it explicitly.
    // `build/scratch/statflags2/inherit.c` reproduces both halves: with the
    // inherited handler the child survives, with `SIG_DFL` restored it dies of
    // signal 11 with `WCOREDUMP`.
    let segv = {
        let mut pipe = [0i32; 2];
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            None
        } else {
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe {
                    libc::close(pipe[0]);
                    libc::signal(libc::SIGSEGV, libc::SIG_DFL);
                    let none = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &none);
                    libc::write(pipe[1], b"\0".as_ptr() as *const libc::c_void, 1);
                    loop {
                        libc::pause();
                    }
                }
            }
            let mut ready = [0u8; 1];
            unsafe {
                libc::close(pipe[1]);
                libc::read(pipe[0], ready.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(pipe[0]);
                libc::kill(child, libc::SIGSEGV);
            }
            let word = zombie_word(child);
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, 0) };
            word
        }
    };
    r.check(
        &format!("a child killed by SIGSEGV carries PF_DUMPCORE ({segv:?})"),
        matches!(segv, Some(w) if w & PF_DUMPCORE == PF_DUMPCORE),
    );
    r.check(
        &format!(
            "and the dump bit is the difference between the two deaths ({segv:?} vs {term:?})"
        ),
        matches!((segv, term), (Some(dumped), Some(plain)) if dumped & !PF_DUMPCORE == plain),
    );

    // A stopped task is not a dead one. This is the claim a kernel that read
    // "not running" as "over" would fail, and `flags5.c` read `0x0`/`0x400000`
    // for exactly this state on the host.
    let stopped = {
        let mut pipe = [0i32; 2];
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            None
        } else {
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe {
                    libc::close(pipe[0]);
                    libc::write(pipe[1], b"\0".as_ptr() as *const libc::c_void, 1);
                    loop {
                        libc::pause();
                    }
                }
            }
            let mut ready = [0u8; 1];
            unsafe {
                libc::close(pipe[1]);
                libc::read(pipe[0], ready.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(pipe[0]);
                libc::kill(child, libc::SIGSTOP);
            }
            // Let the stop land before reading, so the state is the one asked
            // for rather than the one it was about to leave.
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, libc::WUNTRACED) };
            let after = std::fs::read_to_string(format!("/proc/{child}/stat")).ok();
            let word = after.and_then(|text| field(&text, 9).map(|w| w as u64));
            unsafe {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, 0);
            }
            word
        }
    };
    r.check(
        &format!("a SIGSTOPped task does not carry the death bits ({stopped:?})"),
        matches!(stopped, Some(w) if w & PF_DEAD == 0),
    );
    r.check(
        &format!("and it still carries PF_FORKNOEXEC, because a stop is not an exec ({stopped:?})"),
        matches!(stopped, Some(w) if w & PF_FORKNOEXEC == PF_FORKNOEXEC),
    );

    // The thread spelling: a worker thread is made by `clone` and never execs,
    // so it carries the fork bit while its leader does not (`flags3.c`).
    let before = flags_of(me);
    let worker = std::thread::spawn(|| {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let me = unsafe { libc::getpid() };
        (tid, flags_of_task(me, tid), flags_of(me))
    });
    let (worker_tid, worker_word, worker_group) = worker.join().unwrap_or((-1, None, None));
    r.check(
        &format!(
            "a worker thread carries PF_FORKNOEXEC and its group does not (tid {worker_tid}, {worker_word:?} vs {worker_group:?})"
        ),
        matches!((worker_word, worker_group), (Some(own), Some(group)) if own & PF_FORKNOEXEC != 0 && group & PF_FORKNOEXEC == 0),
    );
    r.check(
        &format!(
            "and the leader's own word is unchanged by the thread ({before:?} -> {worker_group:?})"
        ),
        before == worker_group,
    );

    // A reaped child has no file: the word is a property of a task that exists.
    r.check(
        &format!(
            "a reaped child has no flags to report ({:?})",
            flags_of(zombie_pid)
        ),
        flags_of(zombie_pid).is_none(),
    );

    r.finish();
}
