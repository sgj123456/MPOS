//! t-tid-dir - a **thread**'s own `/proc/<tid>` directory.
//!
//! Linux serves two names for one thread: `/proc/<pid>/task/<tid>`, which is
//! reached from the group, and `/proc/<tid>`, which is reached from the tid
//! itself. This kernel served only the first - its `/proc` directory was a
//! *leader*'s (`pid_exists` asked `snapshot(pid).pid == pid`), so `/proc/<tid>`
//! was `ENOENT` - and item 63's pin is where that was read from the other end:
//! it claimed a thread's `exit_signal` through the `task/` spelling, because it
//! was the only one there was. `docs/uapi/` §5.2 named the gap; this program is
//! its pin.
//!
//! # Both spellings have a program behind them
//!
//! Which of the two names a *program* uses was §5.2's open question, and it is
//! measured rather than guessed, on a current host under `strace`
//! (`.workbuddy/probe_tid_dir.c`): `ps -eLf` walks the group - 306 opens of the
//! *relative* name `task`, beside not one absolute `/proc/<pid>/task/<tid>`
//! path - while `gdb -p <tid>` opens **`/proc/<tid>/status`** outright. So the
//! `task/` spelling is `procps`'s and the tid's own is a debugger's, which is
//! also the shape `/proc/thread-self` hands out: its target text is
//! `<pid>/task/<tid>`, and what it *means* is the tid's directory.
//!
//! # The measurement, whole
//!
//! One probe, a live thread of a live group (`.workbuddy/probe_tid_dir.c`,
//! Linux 7.2.11):
//!
//!   * `/proc/<tid>` is a `0555` directory whose **name set is the leader's
//!     exactly** - 54 entries against 54. The `task/` spelling is the *narrower*
//!     one (47): it carries `children` and lacks `autogroup`,
//!     `coredump_filter`, `map_files`, `mountstats`, `task`, `timens_offsets`,
//!     `timers` and `timerslack_ns`. That split is Linux's own (`base_stuff`
//!     against `tid_base_stuff`), which is why `/proc/<tid>/children` is
//!     `ENOENT` where `/proc/<pid>/task/<tid>/children` answers.
//!   * `stat`'s field 1 is the **tid** (the leader's own record is the pid),
//!     field 38 is **-1**, and field 2 is the task's own `comm`.
//!   * `status` is the pair rather than one number twice: `Tgid 16084 / Pid
//!     16085 / NStgid 16084 / NSpid 16085`.
//!   * `readdir /proc` does **not** list the tid - a listing that carried every
//!     thread would make `ps -e` count one process per thread.
//!   * the *task's own* structures are what the tid's directory reads, which is
//!     the half a group-shaped answer would get wrong: a thread that
//!     `unshare(CLONE_FS)`s and `chdir`s reports its own `cwd` through
//!     `/proc/<tid>/cwd` while its leader's still reports the old one, and a
//!     thread that `unshare(CLONE_FILES)`s and opens a descriptor lists it under
//!     `/proc/<tid>/fd` alone. Both are claimed below from a thread that has
//!     done exactly that.
//!   * and a thread that has **exited** takes its tid with it: it is out of
//!     `/proc/<pid>/task` and `/proc/<tid>` is `ENOENT` - Linux's
//!     `__unhash_process` unhooks a task at exit *unless it leads the group*,
//!     which is why the zombie *process* of item 63's pin keeps its record.
//!
//! Every claim below is made while the thread is **alive** except the two that
//! are about it having gone, so none of them is a reading of a task that was
//! already on its way out.
//!
//! # The `children` file's owner, and the parent a record names (item 65)
//!
//! `children` is the one name the `task/` set carries and the process set does
//! not, which makes it the second half of what this program is about: it is
//! reached through the group, it answers for **one task**, and the records it
//! sits beside name a parent differently from its own list. Measured in the same
//! session on the same fixture shape (`.workbuddy/probe_children_owner.c`,
//! Linux 7.2.11): the leader's child is listed under the leader alone and a
//! **thread's** child under that thread alone; `/proc/<pid>/children` and
//! `/proc/<tid>/children` are both `ENOENT`; the text is `<pid> ` per child and
//! a **zero-byte** file for a task that has forked nobody; a corpse stays on the
//! list until it is reaped; and a child forked by a thread reads `getppid`,
//! `status`'s `PPid` and `stat`'s field 4 as the **group**, not as the tid that
//! forked it.
//!
//! # The link's other end: a thread is its group's child (item 66)
//!
//! Item 65 stopped at the record and named the link: a thread's own parent is
//! its creator's parent on Linux - `kernel_clone` turns `CLONE_THREAD` into
//! `CLONE_PARENT`, so "threads share the parent" - where this kernel's thread
//! carried **no** link at all, so all three doors read `0`. Measured in the
//! same session (`.workbuddy/probe_children_owner.c`, Linux 7.2.11, the leader
//! `87096` whose own parent was `87094`): the worker thread `87099` read
//! `getppid`, `status`'s `PPid` and `stat`'s field 4 as `87094` - its *group's*
//! parent, not the tid that created it.
//!
//! Three behaviours move with that link, and all three are measured:
//!
//!   * a **thread's death notifies nobody** - the same probe blocked `SIGCHLD`
//!     and asked `sigpending`: a `fork` child's death queued a record, a
//!     thread's queued none;
//!   * a dying task **hands its children to an alive task of its group** before
//!     init - the thread's child moved onto the leader's `children` list the
//!     moment the thread exited, and the mirror is measured too
//!     (`.workbuddy/probe_leader_exit.c`: a leader that leaves on `pthread_exit`
//!     while its worker runs gives the *worker* its child, whose `PPid` still
//!     reads the group because that is the worker's `tgid`);
//!   * and **`wait` walks every thread of the caller's group** - with the
//!     forking thread still alive the leader's `waitpid(child, WNOHANG)` was
//!     `0`, not `ECHILD`, and a blocking wait collected it outright.
//!
//! So the fixture below no longer reaps the thread's child itself: the leader
//! does, both while the thread lives and after it has gone. That is the half
//! Linux's `do_wait` (`while (tsk != current->group_leader)`) buys a program.

use mpos_user_tests::Report;

/// The tid this thread runs as - `gettid(2)`, which musl reaches through the
/// `SYS_gettid` number and whose libc wrapper names a different call.
fn my_tid() -> i64 {
    unsafe { libc::syscall(libc::SYS_gettid) }
}

/// Field `n` (1-based, as proc(5) numbers them) out of a `stat`-shaped file.
///
/// Field 1 is before the parenthesised `comm`, field 2 is inside it, and the
/// rest are separated by spaces after the **last** `)` - the delimiter `comm`
/// may itself contain.
fn field(path: &str, n: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if n == 1 {
        return text.split_whitespace().next().map(str::to_string);
    }
    let close = text.rfind(')')?;
    if n == 2 {
        return Some(text[text.find('(')? + 1..close].to_string());
    }
    text[close + 1..]
        .split_whitespace()
        .nth(n - 3)
        .map(str::to_string)
}

/// One named `status` line's value, tab-trimmed.
fn status_line(path: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{key}:");
    text.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|rest| rest.trim().to_string())
    })
}

/// The names a directory lists, sorted.
fn names(dir: &str) -> Option<Vec<String>> {
    let mut listed: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    listed.sort();
    Some(listed)
}

/// The whole text of a `readlink`-able name, as `/proc` answers it.
fn link_target(path: &str) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|target| target.to_string_lossy().into_owned())
}

/// A file's mode bits, without following it.
fn mode_of(path: &str) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| m.mode())
}

/// Run `body` in a forked child and read its verdict from the exit status.
///
/// The two descriptor claims below need it, and for a reason that is measured
/// rather than tidy: `read_dir` of a process's **own** `/proc/<pid>/fd` holds a
/// descriptor open while it lists, so the number it adds to the listing is a
/// number that process has - the first run of this program failed exactly there,
/// reading the leader's own `/proc/<pid>/fd` from the leader. A child's
/// descriptor lives in the child, so the parent's listing is the parent's.
fn in_child(body: impl FnOnce() -> bool) -> bool {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let ok = body();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    reaped == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// Whether `dir` lists `number` as an entry name.
fn lists(dir: &str, number: i32) -> bool {
    names(dir).is_some_and(|list| list.iter().any(|name| name == &number.to_string()))
}

/// Collect `pid` within about two seconds, and answer whether it went.
///
/// Bounded rather than a blocking `waitpid`, because this program's fixtures
/// are processes that leave when a pipe closes, and *this* program must not be
/// the thing that hangs the sweep: a claim that never gets printed reads as a
/// hang, which is worse than a claim that fails. A `-1` answers at once - that
/// is `ECHILD`, which is what a wait for a task that is not this thread's child
/// answers.
fn reap_within(pid: i32) -> bool {
    wait_within(pid).is_some()
}

/// [`reap_within`] that keeps the status word, for the claim that the leader
/// collected the thread's child *with the code the child exited by*.
fn wait_within(pid: i32) -> Option<i32> {
    for _ in 0..400 {
        let mut status = 0;
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if got == pid {
            return Some(status);
        }
        if got == -1 {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
}

fn main() {
    println!("t-tid-dir");
    let mut r = Report::new();

    let pid = std::process::id();
    // A known cwd, so the leader's `/proc/<pid>/cwd` is a fact this program
    // chose rather than whatever the runner happened to start it in - the
    // thread below moves its *own* copy of the context somewhere else.
    std::env::set_current_dir("/").expect("a cwd to start from");

    // ---- the thread the claims are about ----------------------------------
    //
    // It splits its own filesystem context and its own descriptor table off the
    // group's **before** it reports, because those are the two structures whose
    // owner the claims below discriminate: a directory built from the leader
    // would answer the group's for both. Then it forks a child of its own and
    // parks, so every reading is taken from a live thread - and that child is
    // what tells a *task's* children from its group's.
    let (report_tx, report_rx) = std::sync::mpsc::channel::<(i64, i32, i32, i32, i32)>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

    // The pipe the two children park on, and the one the thread's child writes
    // its own `getppid` through. Both are made *before* the fork sites, because
    // a forked child inherits whatever is open at the moment it is forked.
    let mut park = [0i32; 2];
    let mut b_report = [0i32; 2];
    if unsafe { libc::pipe(park.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(b_report.as_mut_ptr()) } != 0
    {
        r.check("a pipe to park the children on", false);
        r.finish()
    }
    let (park_read, park_write) = (park[0], park[1]);
    let child_a = unsafe { libc::fork() };
    if child_a == 0 {
        // The leader's own child: it reports nothing, holds the read end, and
        // leaves when the parent closes the write end. Closing *its own* copy
        // of that end first is what makes the release reach it at all.
        unsafe {
            libc::close(park_write);
            libc::close(b_report[0]);
            libc::close(b_report[1]);
            let mut byte = [0u8; 1];
            let _ = libc::read(park_read, byte.as_mut_ptr().cast(), 1);
            libc::_exit(0);
        }
    }

    let worker = std::thread::spawn(move || {
        unsafe { libc::syscall(libc::SYS_unshare, libc::CLONE_FS as libc::c_long) };
        let _ = std::env::set_current_dir("/tmp");
        unsafe { libc::syscall(libc::SYS_unshare, libc::CLONE_FILES as libc::c_long) };
        let own = std::fs::File::open("/dev/null").ok();
        let fd = own
            .as_ref()
            .map(std::os::fd::AsRawFd::as_raw_fd)
            .unwrap_or(-1);
        // The thread's own child - forked *by the thread*, which is the shape
        // that tells the two `children` files apart.
        let child_b = unsafe { libc::fork() };
        if child_b == 0 {
            unsafe {
                libc::close(park_write);
                libc::close(b_report[0]);
                let ppid = libc::getppid();
                let _ = libc::write(b_report[1], std::ptr::from_ref(&ppid).cast(), 4);
                libc::close(b_report[1]);
                let mut byte = [0u8; 1];
                let _ = libc::read(park_read, byte.as_mut_ptr().cast(), 1);
                // A code the leader's `wait` reads back: the claim that a
                // group-wide wait collects the *record*, not just a pid.
                libc::_exit(7);
            }
        }
        // Its own copy of the write end has to go, or the children never see
        // the release: `unshare(CLONE_FILES)` above gave this thread a
        // descriptor table of its own, and a reader holding a writer waits for
        // ever. The same goes for the report pipe - a child that never reached
        // its `write` would leave the group's read parked on *this* copy.
        unsafe { libc::close(park_write) };
        unsafe { libc::close(b_report[1]) };
        let thread_ppid = unsafe { libc::getppid() };
        let _ = report_tx.send((my_tid(), thread_ppid, fd, child_b, b_report[0]));
        let _ = release_rx.recv();
        // The thread does **not** reap its child: `do_wait` walks every thread
        // of the caller's group, so the leader collects it - while the thread
        // lives, and after it has gone (a dying task hands its children to an
        // alive task of its group). Doing it here would leave the leader's
        // claims nothing to collect.
        //
        // Held until the group has read `/proc/<tid>/fd`: a dropped descriptor
        // would take the number out of the listing the claim is about.
        drop(own);
    });
    let (tid, thread_ppid, fd, child_b, b_read) = match report_rx.recv() {
        Ok(working) => working,
        Err(_) => {
            r.check("the thread under test reported", false);
            r.finish()
        }
    };
    r.check(
        "the thread under test started",
        tid > 0 && pid as i64 != tid,
    );

    // ---- a thread's own parent is its group's (item 66) -------------------
    //
    // `CLONE_THREAD` implies `CLONE_PARENT` on Linux, so a thread's parent is
    // its creator's parent - the same number the leader reads - and the three
    // doors are one rule. Measured: the worker `87099` read `87094` through all
    // three while the leader `87096`'s own parent was `87094`
    // (`.workbuddy/probe_children_owner.c`).
    let leader_ppid = status_line(&format!("/proc/{pid}/status"), "PPid")
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(-1);
    r.check(
        "a thread's own getppid(2) is its group's parent",
        leader_ppid > 0 && thread_ppid == leader_ppid,
    );
    r.check(
        "and the tid's status PPid is the same number",
        status_line(&format!("/proc/{tid}/status"), "PPid").as_deref()
            == Some(leader_ppid.to_string().as_str()),
    );
    r.check(
        "and its stat field 4 agrees with the other two doors",
        field(&format!("/proc/{tid}/stat"), 4).as_deref() == Some(leader_ppid.to_string().as_str()),
    );
    // The read end of the thread's child's report is the parent's now, and its
    // write end is the child's alone - closed here so a child that never
    // reported answers an empty read rather than parking this program.
    unsafe { libc::close(b_report[1]) };

    let tid_dir = format!("/proc/{tid}");
    let pid_dir = format!("/proc/{pid}");
    let task_dir = format!("/proc/{pid}/task/{tid}");

    // ---- the directory, and what it names --------------------------------
    r.check(
        "a thread's tid names a directory, not ENOENT",
        mode_of(&tid_dir).is_some_and(|m| m & libc::S_IFMT == libc::S_IFDIR),
    );
    r.check(
        "and its own word is the 0555 every proc directory carries",
        mode_of(&tid_dir).is_some_and(|m| m & 0o7777 == 0o555),
    );
    // The set, which is the shape §5.2 called "the whole file set": the *process*
    // set rather than the narrower one the `task/` spelling answers with - the
    // same either way for the names this kernel implements, and the claim that
    // the tid's directory is built as a process's.
    r.check(
        "the tid's name set is the leader's exactly, not the narrower task set",
        names(&tid_dir).is_some() && names(&tid_dir) == names(&pid_dir),
    );
    // The two halves of the listing question, and they are deliberately
    // different questions: `/proc` names processes, and a tid is looked *up*.
    let procs = names("/proc").unwrap_or_default();
    r.check(
        "readdir /proc holds the leader",
        procs.iter().any(|name| name == &pid.to_string()),
    );
    r.check(
        "and does not hold the thread - a listing carries processes",
        !procs.iter().any(|name| name == &tid.to_string()),
    );

    // ---- stat: the two fields that are the task's own ---------------------
    let tid_stat = format!("{tid_dir}/stat");
    let pid_stat = format!("{pid_dir}/stat");
    let task_stat = format!("{task_dir}/stat");
    r.check(
        "the tid's stat field 1 is the tid",
        field(&tid_stat, 1).as_deref() == Some(tid.to_string().as_str()),
    );
    r.check(
        "where the leader's own record still names the process",
        field(&pid_stat, 1).as_deref() == Some(pid.to_string().as_str()),
    );
    r.check(
        "the tid's stat field 38 is -1, the number no signal byte can carry",
        field(&tid_stat, 38).as_deref() == Some("-1"),
    );
    r.check(
        "and the leader's is not -1: it is a process, and its byte stands",
        field(&pid_stat, 38).is_some_and(|value| value != "-1"),
    );
    // The same task's record through the group's own spelling, field for field
    // over the ones this kernel accounts: the two names are one record, which is
    // why a program may read either.
    r.check(
        "the task spelling's own stat agrees about the id and the byte",
        field(&task_stat, 1) == field(&tid_stat, 1)
            && field(&task_stat, 38) == field(&tid_stat, 38),
    );
    r.check(
        "and its field 2 is the tid directory's comm",
        field(&tid_stat, 2)
            == std::fs::read_to_string(format!("{tid_dir}/comm"))
                .ok()
                .map(|text| text.trim_end().to_string()),
    );

    // ---- status: `Pid` and `Tgid` are a pair, not one number twice --------
    let tid_status = format!("{tid_dir}/status");
    r.check(
        "the tid's status names the thread in Pid",
        status_line(&tid_status, "Pid").as_deref() == Some(tid.to_string().as_str()),
    );
    r.check(
        "and the group in Tgid",
        status_line(&tid_status, "Tgid").as_deref() == Some(pid.to_string().as_str()),
    );
    r.check(
        "with the NS* chains running the same way: NSpid the thread's",
        status_line(&tid_status, "NSpid").as_deref() == Some(tid.to_string().as_str()),
    );
    r.check(
        "and NStgid the group's",
        status_line(&tid_status, "NStgid").as_deref() == Some(pid.to_string().as_str()),
    );

    // ---- the group-shaped names, reached through the tid ------------------
    r.check(
        "the tid's task listing is the group's, both threads",
        names(&format!("{tid_dir}/task")) == names(&format!("{pid_dir}/task")),
    );
    r.check(
        "the tid's ns/pid reads back what the group's does",
        link_target(&format!("{tid_dir}/ns/pid")).is_some()
            && link_target(&format!("{tid_dir}/ns/pid"))
                == link_target(&format!("{pid_dir}/ns/pid")),
    );
    r.check(
        "the tid's exe reads back the image the group's does",
        link_target(&format!("{tid_dir}/exe")).is_some()
            && link_target(&format!("{tid_dir}/exe")) == link_target(&format!("{pid_dir}/exe")),
    );
    r.check(
        "and the tid's maps lists records",
        std::fs::read_to_string(format!("{tid_dir}/maps"))
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false),
    );

    // ---- the task's *own* structures, which is what the tid is for --------
    //
    // These two are the discriminating pair: `/proc/<tid>` reads the *task's*
    // `fs_struct` and file table, so a thread that split them off reports its own
    // through its own name - while the leader's directory goes on reporting the
    // group's. A directory built from the leader would answer both the same way.
    r.check(
        "the tid reads the thread's own cwd after CLONE_FS was unshared",
        link_target(&format!("{tid_dir}/cwd")).as_deref() == Some("/tmp"),
    );
    r.check(
        "while the leader's still reads the one the group kept",
        link_target(&format!("{pid_dir}/cwd")).as_deref() == Some("/"),
    );
    let tid_fds = format!("{tid_dir}/fd");
    let pid_fds = format!("{pid_dir}/fd");
    r.check(
        "the tid lists the descriptor the thread opened under its own CLONE_FILES",
        fd >= 0 && in_child(|| lists(&tid_fds, fd)),
    );
    r.check(
        "and the leader's does not - the tables are two",
        fd >= 0 && in_child(|| !lists(&pid_fds, fd)),
    );

    // ---- children: the list is the task's, and the parent is the group -----
    //
    // The two fixtures above are the shape §5.1 item 65 was measured on: the
    // leader forked `child_a`, the worker thread forked `child_b`, and both are
    // alive. `children` is the one name the `task/` set carries and the process
    // set does not, so it is reached through the group and answers about *one
    // task* - while the records beside it name that task's parent as its
    // **group**.
    let leader_children = format!("{pid_dir}/task/{pid}/children");
    let thread_children = format!("{task_dir}/children");
    let text = |path: &str| std::fs::read_to_string(path).ok();

    r.check(
        "the process's own directory carries no children file",
        text(&format!("{pid_dir}/children")).is_none(),
    );
    r.check(
        "nor does the tid's - the file is the task set's alone",
        text(&format!("{tid_dir}/children")).is_none(),
    );
    // The bytes are the claim, not just the set: Linux writes `<pid> ` per
    // child, and the length that follows is the part a "who is in there"
    // reading would miss.
    r.check(
        "the leader's children file is the child the leader forked",
        text(&leader_children).as_deref() == Some(format!("{child_a} ").as_str()),
    );
    r.check(
        "and the thread's is the child the thread forked, not the leader's",
        text(&thread_children).as_deref() == Some(format!("{child_b} ").as_str()),
    );
    r.check(
        "a task that has forked nobody has a zero-byte children file",
        text(&format!("/proc/{child_b}/task/{child_b}/children")).as_deref() == Some(""),
    );

    // The parent a record *names* is the fork-parent's group, which is what
    // makes a thread's child answer the process rather than the thread. All
    // three doors are asked, because they are three readers of one rule.
    let group = pid.to_string();
    r.check(
        "a child forked by a thread names the group in stat's field 4",
        field(&format!("/proc/{child_b}/stat"), 4).as_deref() == Some(group.as_str()),
    );
    r.check(
        "and the same group in status's PPid",
        status_line(&format!("/proc/{child_b}/status"), "PPid").as_deref() == Some(group.as_str()),
    );
    let mut raw = [0u8; 4];
    let got = unsafe { libc::read(b_read, raw.as_mut_ptr().cast(), 4) };
    unsafe { libc::close(b_read) };
    r.check(
        "and its own getppid(2) answers the group too",
        got == 4 && i32::from_ne_bytes(raw) == pid as i32,
    );
    // The control, and it is one: a child the *leader* forked names the leader,
    // which is that same group - so this claim holds before and after item 65
    // and is here to keep the thread's own answer from being read as the
    // general one.
    r.check(
        "while the child the leader forked names the leader, the same number",
        field(&format!("/proc/{child_a}/stat"), 4).as_deref() == Some(group.as_str()),
    );

    // A wait is the group's, not the forking thread's: `do_wait` walks every
    // thread of the caller's group, so the leader sees a child its worker
    // forked - and `WNOHANG` answers "not yet" (`0`), not `ECHILD`, because
    // the child exists. Measured: `.workbuddy/probe_children_owner.c` read the
    // leader's `waitpid(child B, WNOHANG)` as `0`.
    let mut probe_status = 0;
    let probe = unsafe { libc::waitpid(child_b, &mut probe_status, libc::WNOHANG) };
    r.check(
        "the leader may wait for a child its thread forked: WNOHANG answers 0, not ECHILD",
        probe == 0,
    );

    // A corpse is still a child until it is reaped, which is the reap and not
    // the exit that takes it off the list.
    let dead = unsafe { libc::fork() };
    if dead == 0 {
        unsafe { libc::_exit(0) };
    }
    let listed = |pid: i32| {
        std::fs::read_to_string(&leader_children)
            .map(|text| text.split_whitespace().any(|id| id == pid.to_string()))
            .unwrap_or(false)
    };
    // Polled rather than slept on: the exit and the listing are two events, and
    // the corpse is only interesting once `stat` says `Z` - a live child is on
    // the list either way, so a reading that did not wait for the letter would
    // claim nothing.
    let mut corpse = false;
    for _ in 0..200 {
        if field(&format!("/proc/{dead}/stat"), 3).as_deref() == Some("Z") && listed(dead) {
            corpse = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    r.check("an unreaped corpse stays on its parent's list", corpse);
    let mut dead_status = 0;
    unsafe { libc::waitpid(dead, &mut dead_status, 0) };
    r.check("and the reap is what takes it off", !listed(dead));

    // ---- the link's other end: the group adopts what the thread left ------
    //
    // The worker leaves while child B is still parked, and Linux's
    // `forget_original_parent` hands a dying task's children to an alive task
    // of its group before init - measured: the thread's child moved onto the
    // leader's `children` list (`probe_children_owner.c`). The mirror is
    // measured too (`.workbuddy/probe_leader_exit.c`): a leader that leaves on
    // `pthread_exit` under a live worker gives the *worker* its child.
    //
    // A dead thread also takes its tid with it: Linux unhooks a task from the
    // namespace it was numbered in at exit *unless it leads its group* - so a
    // dead thread's tid names nothing while a zombie process keeps its
    // `/proc/<pid>` (which item 63's pin reads a byte out of). This kernel keeps
    // a dead thread's *slot* until it is reused, so the clause is the tree's to
    // apply; both readings below are Linux's answer. The first cannot be fooled
    // by a slot being handed to somebody else: a reused slot belongs to another
    // group, so it would not appear in *this* process's `task` listing either
    // way.
    let _ = release_tx.send(());
    let _ = worker.join();
    r.check(
        "a thread that has exited is out of its group's task listing",
        names(&format!("{pid_dir}/task"))
            .is_some_and(|list| !list.iter().any(|name| name == &tid.to_string())),
    );
    r.check(
        "and its tid names no directory at all",
        mode_of(&tid_dir).is_none(),
    );
    r.check(
        "so the child it forked is on the leader's children file now",
        text(&leader_children).as_deref() == Some(format!("{child_a} {child_b} ").as_str()),
    );

    // Release both children: closing the write end is what their reads wait
    // for, and each held its own copy until then. Child A is the leader's own;
    // child B is the one it adopted, and the status word must be *that* death's
    // - the code child B exits by is 7.
    unsafe { libc::close(park_write) };
    reap_within(child_a);
    let b_status = wait_within(child_b);
    r.check(
        "and the leader's wait collects it, with the code it exited by",
        b_status.is_some_and(|status| libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 7),
    );
    r.check(
        "which takes it off the leader's children file",
        !listed(child_b),
    );

    // ---- a thread's death notifies nobody ---------------------------------
    //
    // The link's other consequence: only a group leader's death is the
    // process's, so a worker's exit queues no `SIGCHLD` for the group's parent
    // (`do_notify_parent`'s `thread_group_leader` clause). Measured with the
    // signal *blocked*, which is the only way a record that was never queued
    // can be told from one that was delivered: a `fork` child's death queues
    // one, a thread's queues nothing (`.workbuddy/probe_children_owner.c`).
    //
    // The observer must be the group's **parent**, not the group: a thread's
    // `parent` link is the parent of its process, so the record a missing
    // leader gate would queue lands one level up. The fixture is therefore two
    // forks deep: this `in_child` process is the observer, it blocks `SIGCHLD`,
    // then forks the group; the group starts and joins a thread and reports
    // that the thread has died while it stays alive, so the only `SIGCHLD` the
    // observer could be holding is the thread's.
    //
    // Blocked is the only way to tell "never queued" from "delivered": the
    // probe measured a `fork` child's death as one queued record and a thread's
    // as none (`.workbuddy/probe_children_owner.c`), and it had to do the same
    // scenario in a child of its own for the same reason.
    let quiet = in_child(|| {
        let mut chld: libc::sigset_t = unsafe { core::mem::zeroed() };
        let mut old: libc::sigset_t = unsafe { core::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut chld);
            libc::sigaddset(&mut chld, libc::SIGCHLD);
            libc::sigprocmask(libc::SIG_BLOCK, &chld, &mut old);
        }
        let mut ready = [0i32; 2];
        let mut go = [0i32; 2];
        if unsafe { libc::pipe(ready.as_mut_ptr()) } != 0
            || unsafe { libc::pipe(go.as_mut_ptr()) } != 0
        {
            return false;
        }
        let group = unsafe { libc::fork() };
        if group == 0 {
            unsafe {
                libc::close(ready[0]);
                libc::close(go[1]);
            }
            let thread = std::thread::spawn(|| {});
            let _ = thread.join();
            unsafe {
                let byte = [1u8; 1];
                let _ = libc::write(ready[1], byte.as_ptr().cast(), 1);
                let mut release = [0u8; 1];
                let _ = libc::read(go[0], release.as_mut_ptr().cast(), 1);
                libc::_exit(0);
            }
        }
        unsafe {
            libc::close(ready[1]);
            libc::close(go[0]);
            let mut seen = [0u8; 1];
            let _ = libc::read(ready[0], seen.as_mut_ptr().cast(), 1);
        }
        // The death and the queueing are two events; poll rather than sleep
        // once, so a slow machine cannot pass the claim by being early.
        let mut queued = false;
        for _ in 0..40 {
            let mut pending: libc::sigset_t = unsafe { core::mem::zeroed() };
            unsafe { libc::sigpending(&mut pending) };
            if unsafe { libc::sigismember(&pending, libc::SIGCHLD) } == 1 {
                queued = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        unsafe {
            let byte = [1u8; 1];
            let _ = libc::write(go[1], byte.as_ptr().cast(), 1);
            let mut status = 0;
            libc::waitpid(group, &mut status, 0);
        }
        !queued
    });
    r.check(
        "a thread's death queues no SIGCHLD for its group's parent",
        quiet,
    );

    r.finish()
}
