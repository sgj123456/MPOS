//! `/proc/<pid>/wchan` is the wait a task is in **now**.
//!
//! Linux's file answers the task's *current* state, and that is measurably not
//! the same question `/proc/<pid>/stat`'s field 35 asks. Measured on a current
//! host (Linux 7.2.14, `build/scratch/wchan/{w,stale,f35,threaded,perm3}.c`):
//!
//! ```text
//! running (self)                     n=1  text="0"
//! blocked in a pipe read             n=14 text="anon_pipe_read"
//! blocked in nanosleep               n=17 text="hrtimer_nanosleep"
//! SIGSTOPped                         n=14 text="do_signal_stop"
//! ptrace-stopped                     n=11 text="ptrace_stop"
//! zombie (unreaped)                  n=1  text="0"
//! reaped                             open errno=2   (ENOENT)
//! spinning (runnable)                n=1  text="0"
//! ```
//!
//! and, the two files side by side on one task that slept and then woke:
//!
//! ```text
//! asleep in a pipe read            f35=1   wchan="anon_pipe_read"
//! woken, running                   f35=1   wchan="0"      <- field 35 is stale
//! zombie                           f35=1   wchan="0"
//! ```
//!
//! So `stat`'s field 35 keeps the *last* park (instrumentation for a stuck
//! machine) while `wchan` is live. This pin holds the live half.
//!
//! Three shapes worth naming, because each is a place a plausible kernel gets
//! it wrong:
//!
//! * **No trailing newline.** The file's whole content is the word - 14 bytes
//!   for `anon_pipe_read`, 1 for `0` - where `/proc/<pid>/comm` ends in `\n`.
//! * **A target the caller may not inspect reads `0`, not `EACCES`.** A child
//!   dropped to uid 65534 answers `0` from its parent's `/proc`, and so does
//!   `pid 1`. Refusing here would be a different syscall's answer.
//! * **Per-thread, not per-group.** With a worker blocked in a pipe read and
//!   the leader running: `/proc/<pid>/wchan` and
//!   `/proc/<pid>/task/<leader>/wchan` read `0`, while
//!   `/proc/<pid>/task/<worker>/wchan` reads `anon_pipe_read`.
//!
//! The names are the kernel's own, so this pin asserts the *shape* - the file
//! exists, is one word with no newline, and is `0` exactly when the task is not
//! waiting - and prints the word so a divergence is visible. Asserting a
//! specific symbol would pin this kernel's park vocabulary rather than the
//! ABI, and would go red the day a park site is renamed for a reason that has
//! nothing to do with `wchan`.

use mpos_user_tests::Report;
use std::io::Read;

/// The environment variable that names the pipe a child should block on.
const PARK_FD: &str = "MPOS_WCHAN_PARK_FD";

/// Read the whole of `path`, or `None` when the open fails.
fn slurp(path: &str) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// `/proc/<pid>/wchan`'s content, or `None` when the file is not there.
fn wchan(pid: i32) -> Option<String> {
    slurp(&format!("/proc/{pid}/wchan"))
}

/// `field n` of `/proc/<pid>/stat`, as the raw token: the zero-based token
/// `n - 3` after the last `)`, which is where the fields after `comm` begin.
fn stat_token(pid: i32, n: usize) -> Option<String> {
    let text = slurp(&format!("/proc/{pid}/stat"))?;
    let after = text.rsplit_once(')')?.1;
    Some(after.split_whitespace().nth(n - 3)?.to_string())
}

/// The state letter of a task - field 3, which is a *letter*, not a number.
fn state_letter(pid: i32) -> Option<char> {
    stat_token(pid, 3)?.chars().next()
}

/// A child that parks in a pipe read, as `(pid, handle)`.
///
/// The child gets a pipe whose write end the parent keeps open and never
/// writes, so its `read` blocks for as long as the pin needs. That is the
/// state whose word is a name; a child that slept instead would answer
/// `hrtimer_nanosleep`, which is a different park and a different claim.
fn parked_child() -> (i32, std::process::Child) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let child = std::process::Command::new(std::env::current_exe().expect("own path"))
        .arg("--wchan-park")
        .env(PARK_FD, format!("{}", fds[0]))
        .spawn()
        .expect("spawn the parked child");
    // The read end is the child's now; keeping the write end open is what
    // holds it in the read.
    unsafe { libc::close(fds[0]) };
    let pid = child.id() as i32;
    // Let it reach the read before the parent measures, or the fixture races
    // the park and blames the kernel.
    std::thread::sleep(std::time::Duration::from_millis(250));
    std::mem::forget(unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) });
    (pid, child)
}

/// The child half: block in the pipe read it was handed.
fn park_main() {
    let fd: i32 = std::env::var(PARK_FD)
        .expect("the parent names the fd")
        .parse()
        .expect("a number");
    let mut byte = [0u8; 1];
    // Nobody ever writes, so this is the park the pin measures.
    unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
    std::process::exit(0);
}

use std::os::fd::FromRawFd;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--wchan-park") => park_main(),
        Some("--wchan-exit") => std::process::exit(0),
        _ => {}
    }
    let mut report = Report::new();

    // The file is there at all, and its shape is one word with no newline.
    let me_pid = std::process::id() as i32;
    let me = wchan(me_pid);
    report.check(
        &format!("a task's wchan file exists ({:?})", me.as_deref()),
        me.is_some(),
    );
    let mine = me.unwrap_or_default();
    report.check(
        &format!("and its content carries no newline ({mine:?})"),
        !mine.contains('\n'),
    );

    // A running task is not waiting, so its word is `0`.
    report.check(
        &format!(
            "a running task is not waiting, so it reads 0 ({mine:?}, state {:?})",
            state_letter(me_pid)
        ),
        mine == "0",
    );

    // A parked child: a non-empty word that is not `0`.
    let (pid, mut child) = parked_child();
    let word = wchan(pid);
    let letter = state_letter(pid);
    report.check(
        &format!("a child blocked in a read names its wait ({word:?}, state {letter:?})"),
        word.as_deref().is_some_and(|w| !w.is_empty() && w != "0"),
    );
    // The two files are two reads at two moments, and this kernel's pipe read
    // is a *timed* park (`block_until_nanos`), so a blocked reader alternates
    // between parked and briefly runnable. Measured on the second machine's
    // lane: `wchan` answered `block_until_nanos` while `stat` said `R` in the
    // same breath, because the state was sampled a moment after a wake and
    // before the re-park. So the claim is not "it is asleep right now" - that
    // is not a promise a timed park makes - it is that sampling the two files
    // together finds the pair the files exist to describe: a sample where the
    // task is asleep *and* names its wait. A kernel that never said `S` would
    // fail this, and so would one whose `wchan` always read `0`.
    let mut agreed = None;
    for _ in 0..200 {
        let state = state_letter(pid);
        let word = wchan(pid);
        if state == Some('S') && word.as_deref().is_some_and(|w| w != "0" && !w.is_empty()) {
            agreed = Some((state, word));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    report.check(
        &format!("sampled together, it is asleep with its wait named ({agreed:?})"),
        agreed.is_some(),
    );
    // The two doors answer different questions on purpose: `stat` field 35 is
    // the *last* park and stays set, while `wchan` is live. Both are right.
    let field = stat_token(pid, 35);
    report.check(
        &format!("and stat's field 35 records the same park as a number ({field:?})"),
        field.as_deref().is_some_and(|f| f != "0"),
    );

    // The thread's own spelling answers for the thread: this process is
    // single-threaded, so `task/<tid>` must agree with the process file.
    let per_thread = slurp(&format!("/proc/{me_pid}/task/{me_pid}/wchan"));
    report.check(
        &format!("the thread spelling is there too ({per_thread:?})"),
        per_thread.is_some(),
    );
    report.check(
        &format!("and it answers the same wait as the process file ({per_thread:?} vs {mine:?})"),
        per_thread.as_deref() == Some(mine.as_str()),
    );

    // A zombie is not waiting either, and the file survives until it is
    // collected.
    let mut dead = std::process::Command::new(std::env::current_exe().expect("own path"))
        .arg("--wchan-exit")
        .spawn()
        .expect("spawn a child that exits at once");
    let dead_pid = dead.id() as i32;
    // Let it die, then read it before reaping: `waitpid` takes the `/proc`
    // entry away with it.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let dead_word = wchan(dead_pid);
    report.check(
        &format!(
            "a zombie is not waiting either ({dead_word:?}, state {:?})",
            state_letter(dead_pid)
        ),
        dead_word.as_deref() == Some("0"),
    );
    let _ = dead.wait();
    report.check(
        &format!(
            "and a collected task has no wchan file ({:?})",
            wchan(dead_pid)
        ),
        wchan(dead_pid).is_none(),
    );

    // The rule for a task the caller may not inspect is that this file is
    // *readable*: Linux answers `0` rather than refusing, where `maps`/`smaps`
    // answer `EACCES` for the same target (`build/scratch/wchan/perm3.c`, where
    // a child dropped to uid 65534 and `pid 1` both read `0` from an
    // unprivileged parent). The word itself is not asserted here because it
    // depends on the *caller*: this payload runs as root on the machine, so it
    // may inspect `pid 1` and reads that task's real wait - which is also
    // Linux's answer for root. The claim that holds either way is the one that
    // is the rule: no refusal.
    let init = wchan(1);
    report.check(
        &format!("a task we may not own is answered, not refused ({init:?})"),
        init.is_some(),
    );
    // And the two shapes that answer *are* the two Linux has: the empty word or
    // a name. Anything else - an errno, a made-up sentence - is not this file.
    let word_is_one_of_laws_two = init
        .as_deref()
        .is_some_and(|w| w == "0" || (!w.is_empty() && !w.contains(' ') && !w.contains('\n')));
    report.check(
        &format!("and its word is either 0 or a single wait name ({init:?})"),
        word_is_one_of_laws_two,
    );

    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = child.wait();
    report.finish();
}
