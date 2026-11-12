//! t-proc-timers - `/proc/<pid>/timers` is the group's POSIX timers, four
//! lines each, and a timer's number is never handed out twice.
//!
//! The file is a window onto a table this kernel already keeps: `timer_create`
//! makes the record, `timer_settime` arms it, and `timer_gettime` reads it back
//! - but until §6.137 nothing let a program list what it held, so a probe that
//! wanted to know which numbers were live had to remember every call it made.
//!
//! Everything below was measured on a host Linux first (`build/scratch/ptimers/`,
//! 7.2.14), because the shape is not the shape a man page describes:
//!
//! * mode `0444`, `st_size` **0**, and **no per-thread spelling** - the file is
//!   at `/proc/<pid>/timers` only: `/proc/<pid>/task/<tid>/timers` and
//!   `/proc/thread-self/timers` are both `ENOENT` (`u.c`), because Linux hangs
//!   the list off `signal_struct` rather than off a task or an `mm`.
//! * **four lines per timer**, in this order and with this spelling - note the
//!   lower-case `signal:`/`notify:` beside the upper-case `ID:`/`ClockID:`:
//!
//!   ```text
//!   ID: 2
//!   signal: 12/0000000000000000
//!   notify: signal/tid.17626
//!   ClockID: -6
//!   ```
//!
//!   the `signal:` value is the `sigval` as sixteen hex digits (a timer created
//!   with `sival_ptr = 0xdead` prints `000000000000dead`), and `notify:` spells
//!   the subject `pid.<n>` for `SIGEV_SIGNAL`/`SIGEV_NONE` and `tid.<n>` for
//!   `SIGEV_THREAD_ID` - named in the **reader's** pid namespace (`t.c`).
//! * `ClockID:` is `timer->it_clock`: the caller's id for the wall clocks
//!   (`0`, `1`, `7`, `11` measured) and the kernel's own encoding for a
//!   CPU clock, `CLOCK_PROCESS_CPUTIME_ID` printing **-6** - `(~0 << 3) | 2`
//!   truncated to `int` (`t.c`).
//! * the ids are listed **descending**, and a deleted number is **never**
//!   reused: five creates answered `0, 1, 2, 3, 4`, deleting `1` and creating
//!   again answered **5**, so the file printed `5, 4, 3, 2, 0` (`t.c`). That is
//!   the rule this kernel was breaking: its `timer_t` *was* the slot index, so
//!   the next create after a delete handed back the deleted number and the
//!   caller's old handle started naming a different timer.
//! * `itimer`s are not POSIX timers and do not appear: `setitimer` on
//!   `ITIMER_REAL` and `ITIMER_VIRTUAL` left the file byte-for-byte unchanged
//!   (`p.c`).
//! * a process with no timers reads a **zero-byte** file, not a blank line, and
//!   a **zombie keeps the file, empty**: a killed-but-unreaped child read
//!   `0 bytes`, and the same number read `ENOENT` only after the reap
//!   (`z.c`). Linux keeps the `signal_struct` until the task is collected and
//!   tears the timers down at exit, so the zombie's list is empty rather than
//!   its last state.
//!
//! The counterfactual is a `git stash` of the source files, a rebuild of the
//! kernel *and* the UEFI loader that embeds it, and a boot of this same binary:
//! with the table unreachable the file is absent and every claim that reads it
//! is red, and with ids back on slot indices the two id claims are red on a
//! kernel that still has the file.
//!
//! The wire records are spelled here rather than taken from a binding, for the
//! reason `t-timer-create` measured: no libc on Linux binds `struct sigevent`,
//! `struct itimerspec` or `timer_t`, so the calls go through
//! `libc::syscall(libc::SYS_*, ..)` with the numbers from libc's per-machine
//! table and the offsets from the uapi.

use mpos_user_tests::Report;

/// The uapi's `struct sigevent`: the `sigval` at 0, the signal at 8, the
/// notification kind at 12, then a 48-byte union whose first word is the
/// `SIGEV_THREAD_ID` target at 16. Sixty-four bytes, as a libc passes it; this
/// kernel reads the first thirty-two.
#[repr(C)]
struct SigEvent {
    value: u64,
    signo: i32,
    notify: i32,
    un: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<SigEvent>() == 64);

/// `SIGEV_*` as the uapi numbers them.
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD_ID: i32 = 4;

/// The `clockid`s these claims name, from the uapi rather than a libc macro.
const CLOCK_REALTIME: usize = 0;
const CLOCK_MONOTONIC: usize = 1;
const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
const CLOCK_BOOTTIME: usize = 7;
const CLOCK_TAI: usize = 11;

/// A signal in the range every arm can queue.
const SIG: i32 = 10; // SIGUSR1

/// The payload a `sigval` carries, chosen to be recognisable in the hex word.
const PAYLOAD: u64 = 0xdead;

impl SigEvent {
    fn new(kind: i32, signo: i32, value: u64) -> Self {
        Self {
            value,
            signo,
            notify: kind,
            un: [0; 48],
        }
    }

    /// A `SIGEV_THREAD_ID` timer aimed at `tid`, the union's first word.
    fn at_thread(signo: i32, value: u64, tid: i32) -> Self {
        let mut event = Self::new(SIGEV_THREAD_ID, signo, value);
        event.un[0..4].copy_from_slice(&tid.to_ne_bytes());
        event
    }
}

/// A raw syscall, with the refusal as a plain errno rather than an `Errno`
/// whose variant names differ between a host and a bare-metal libc.
fn raw(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if ret == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as isize)
    }
}

/// `timer_create(clockid, sevp, &id)`.
fn create(clock: usize, event: &SigEvent) -> Result<i32, i32> {
    let mut id: i32 = -1;
    let answer = raw(
        libc::SYS_timer_create,
        clock,
        event as *const SigEvent as usize,
        &mut id as *mut i32 as usize,
        0,
    );
    answer.map(|_| id)
}

/// `timer_delete(id)`, swallowed: the claims only care that the record went.
fn delete(id: i32) {
    let _ = raw(libc::SYS_timer_delete, id as usize, 0, 0, 0);
}

/// The file's bytes as text, or `None` when it is not there.
fn text() -> Option<String> {
    std::fs::read_to_string("/proc/self/timers").ok()
}

/// The four-line blocks the file holds, in the order it lists them.
fn blocks(text: &str) -> Vec<Vec<&str>> {
    let lines: Vec<&str> = text.lines().collect();
    lines.chunks(4).map(|chunk| chunk.to_vec()).collect()
}

/// The value word of the line starting with `name` - `"ID: 4"` -> `"4"`.
fn field<'a>(block: &[&'a str], name: &str) -> Option<&'a str> {
    block
        .iter()
        .find_map(|line| line.strip_prefix(name))
        .map(str::trim)
}

/// The clock word of a block that has exactly one timer in the file.
fn only_clock_word(text: &str) -> Option<String> {
    let parsed = blocks(text);
    if parsed.len() != 1 {
        return None;
    }
    field(&parsed[0], "ClockID:").map(str::to_owned)
}

/// The id of every timer in the file, newest first as printed.
fn ids(text: &str) -> Vec<u32> {
    blocks(text)
        .iter()
        .filter_map(|block| field(block, "ID:")?.parse().ok())
        .collect()
}

/// The state letter of `pid` from `/proc/<pid>/stat`'s field 3, or `None` when
/// the number names nothing. Field `n` is the zero-based index `n - 3` of the
/// tokens after the `)` that closes the command name.
fn state_of(pid: i32) -> Option<char> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1;
    rest.split_whitespace().nth(0)?.chars().next()
}

/// Clear the table: every number the file lists is deleted, so each claim
/// starts from the same empty table.
fn clear() {
    for id in text().map(|text| ids(&text)).unwrap_or_default() {
        delete(id as i32);
    }
}

fn main() {
    let mut r = Report::new();

    // 1. A process with no timers reads an empty file - zero bytes, not a
    //    blank line.
    let empty = text();
    r.check(
        &format!(
            "a process with no timers reads an empty file ({:?})",
            empty.as_ref().map(String::len)
        ),
        empty.as_deref() == Some(""),
    );

    // 2. One timer, and the whole file is Linux's four lines for it.
    let ev = SigEvent::new(SIGEV_SIGNAL, SIG, PAYLOAD);
    let id = match create(CLOCK_MONOTONIC, &ev) {
        Ok(id) => id,
        Err(e) => {
            r.check(&format!("timer_create answered ({e})"), false);
            r.finish();
        }
    };
    let expected = format!(
        "ID: {id}\nsignal: {SIG}/000000000000dead\nnotify: signal/pid.{}\nClockID: 1\n",
        std::process::id()
    );
    let body = text().unwrap_or_default();
    r.check(
        &format!("one timer is Linux's four lines ({body:?})"),
        body == expected,
    );

    // 3. The subject of a process-directed timer is the *process*, and the
    //    signal word carries the sigval the creator asked for.
    let block = blocks(&body).into_iter().next().unwrap_or_default();
    r.check(
        &format!(
            "the subject is this process ({:?})",
            field(&block, "notify:")
        ),
        field(&block, "notify:") == Some(&*format!("signal/pid.{}", std::process::id())),
    );
    r.check(
        &format!(
            "the signal word carries the sigval ({:?})",
            field(&block, "signal:")
        ),
        field(&block, "signal:") == Some(&*format!("{SIG}/000000000000dead")),
    );
    delete(id);

    // 4. ClockID is the clock the timer was built on - the caller's own number
    //    for each wall clock.
    for (clock, want) in [
        (CLOCK_REALTIME, "0"),
        (CLOCK_MONOTONIC, "1"),
        (CLOCK_BOOTTIME, "7"),
        (CLOCK_TAI, "11"),
    ] {
        clear();
        let ev = SigEvent::new(SIGEV_SIGNAL, SIG, 0);
        let Ok(_) = create(clock, &ev) else {
            r.check(&format!("a timer on clock {clock} was created"), false);
            continue;
        };
        let got = text().as_deref().and_then(only_clock_word);
        r.check(
            &format!("clock {clock} prints ClockID {want} (got {got:?})"),
            got.as_deref() == Some(want),
        );
    }

    // 5. A CPU-time timer prints the kernel's encoded word, not the id the
    //    caller passed.
    clear();
    let ev = SigEvent::new(SIGEV_SIGNAL, SIG, 0);
    match create(CLOCK_PROCESS_CPUTIME_ID, &ev) {
        Ok(_) => {
            let got = text().as_deref().and_then(only_clock_word);
            r.check(
                &format!("a CPU-time timer prints ClockID -6 (got {got:?})"),
                got.as_deref() == Some("-6"),
            );
        }
        Err(e) => r.check(
            &format!("a timer on the process CPU clock was created ({e})"),
            false,
        ),
    }

    // 6. A silent timer names the process too, and a thread-directed one names
    //    its thread.
    clear();
    let ev = SigEvent::new(SIGEV_NONE, 0, 0);
    if create(CLOCK_MONOTONIC, &ev).is_ok() {
        let body = text().unwrap_or_default();
        let block = blocks(&body).into_iter().next().unwrap_or_default();
        let want = format!("none/pid.{}", std::process::id());
        r.check(
            &format!(
                "a silent timer still names the process (want {want}, got {:?})",
                field(&block, "notify:")
            ),
            field(&block, "notify:") == Some(&*want),
        );
    } else {
        r.check("a silent timer was created", false);
    }
    clear();
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let ev = SigEvent::at_thread(SIG, 0, tid);
    if create(CLOCK_MONOTONIC, &ev).is_ok() {
        let body = text().unwrap_or_default();
        let block = blocks(&body).into_iter().next().unwrap_or_default();
        let want = format!("signal/tid.{tid}");
        r.check(
            &format!(
                "a thread-directed timer names its thread by tid (want {want}, got {:?})",
                field(&block, "notify:")
            ),
            field(&block, "notify:") == Some(&*want),
        );
    } else {
        r.check("a thread-directed timer was created", false);
    }

    // 7. The ids are the numbers `timer_create` answered with, and they are
    //    listed newest first.
    clear();
    let mut made = Vec::new();
    for _ in 0..3 {
        let ev = SigEvent::new(SIGEV_SIGNAL, SIG, 0);
        match create(CLOCK_MONOTONIC, &ev) {
            Ok(id) => made.push(id),
            Err(e) => r.check(&format!("a third timer was created ({e})"), false),
        }
    }
    let listed = text().map(|text| ids(&text)).unwrap_or_default();
    r.check(
        &format!("the ids are the numbers create answered ({made:?} against {listed:?})"),
        {
            let mut want: Vec<u32> = made.iter().map(|id| *id as u32).collect();
            want.sort_unstable_by(|a, b| b.cmp(a));
            listed == want
        },
    );

    // 8. A deleted number is never handed out again: Linux keeps a per-process
    //    counter that only moves forward, so the create after a delete answers
    //    a number no timer wore before.
    if made.len() == 3 {
        delete(made[1]);
        let ev = SigEvent::new(SIGEV_SIGNAL, SIG, 0);
        match create(CLOCK_MONOTONIC, &ev) {
            Ok(fresh) => r.check(
                &format!(
                    "a deleted id is not reused (deleted {}, fresh {fresh}, all {made:?})",
                    made[1]
                ),
                fresh != made[1] && !made.contains(&fresh),
            ),
            Err(e) => r.check(&format!("the create after a delete answered ({e})"), false),
        }
    } else {
        r.check("three timers existed to delete one of", false);
    }

    // 9. An `itimer` is not a POSIX timer: the file does not change.
    clear();
    let before = text().unwrap_or_default();
    let it = Itimerval::oneshot(60);
    let _ = unsafe {
        libc::syscall(
            libc::SYS_setitimer,
            0, /* ITIMER_REAL */
            &it as *const Itimerval,
            0,
        )
    };
    let after = text().unwrap_or_default();
    r.check(
        &format!(
            "an itimer does not appear ({:?} against {:?})",
            before, after
        ),
        before == after,
    );
    clear();

    // 10. The node's own words: `0444` and no size known before the read.
    match std::fs::metadata("/proc/self/timers") {
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            r.check(
                &format!(
                    "the file is 0444 with no size (mode {:o}, size {})",
                    meta.mode() & 0o7777,
                    meta.len()
                ),
                meta.mode() & 0o7777 == 0o444 && meta.len() == 0,
            );
        }
        Err(e) => r.check(&format!("the timers file has metadata ({e})"), false),
    }

    // 11. There is no per-thread spelling - the list is the process's.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let path = format!("/proc/self/task/{tid}/timers");
    r.check(
        &format!(
            "there is no per-thread spelling ({} -> {:?})",
            path,
            std::fs::metadata(&path).err().map(|e| e.raw_os_error())
        ),
        std::fs::metadata(&path).is_err(),
    );

    // 12. A zombie *keeps* the file, empty; the reap takes it away. Measured on
    //     the host (`build/scratch/ptimers/z.c`): a killed-but-unreaped child
    //     read `0 bytes` and the same number read `ENOENT` after `waitpid`,
    //     because Linux keeps the `signal_struct` until the task is collected
    //     and tears the timers down at exit.
    let mut sync = [0i32; 2];
    if unsafe { libc::pipe(sync.as_mut_ptr()) } == 0 {
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe {
                libc::close(sync[0]);
                // Tell the parent we are about to exit, then do it: the read of
                // the pipe cannot return before the write, so the parent's
                // first look lands on a task that is at least on its way out.
                let byte = [0u8];
                libc::write(sync[1], byte.as_ptr().cast(), 1);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(sync[1]) };
        let mut byte = [0u8];
        unsafe { libc::read(sync[0], byte.as_mut_ptr().cast(), 1) };
        unsafe { libc::close(sync[0]) };
        // Let the exit land: a task that has written but not yet been scheduled
        // out is still alive, and Linux would answer a live (empty) file too -
        // so wait until the child is visibly a zombie before judging.
        let path = format!("/proc/{child}/timers");
        let mut zombie_empty = false;
        for _ in 0..200 {
            if let Some(state) = state_of(child)
                && state == 'Z'
            {
                zombie_empty = std::fs::read_to_string(&path)
                    .map(|t| t.is_empty())
                    .unwrap_or(false);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        let after_reap = std::fs::metadata(&path).is_err();
        r.check(
            &format!("a zombie keeps an empty timers file ({zombie_empty})"),
            zombie_empty,
        );
        r.check(
            &format!("and the reap takes it away ({after_reap})"),
            after_reap,
        );
    } else {
        r.check("a pipe was made for the zombie claim", false);
    }

    r.finish();
}

/// The uapi's `struct itimerval`, for the claim that an `itimer` is not a POSIX
/// timer: one interval and one value, each a `timeval`.
#[repr(C)]
struct Itimerval {
    interval: Timeval,
    value: Timeval,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Timeval {
    tv_sec: libc::c_long,
    tv_usec: libc::c_long,
}

const _: () = assert!(core::mem::size_of::<Itimerval>() == 32);

impl Itimerval {
    fn oneshot(seconds: libc::c_long) -> Self {
        Self {
            interval: Timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            value: Timeval {
                tv_sec: seconds,
                tv_usec: 0,
            },
        }
    }
}
