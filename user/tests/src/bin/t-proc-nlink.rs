//! t-proc-nlink - the link count a `/proc` directory answers, and what reads it.
//!
//! `st_nlink` on a directory is the number of names that point at it: its own
//! `.`, its parent's entry for it, and one per subdirectory. For most of a
//! filesystem it is a fact about the tree that nobody asks - but `/proc`'s
//! numbers are read, and one program reads them as a *question*:
//!
//! ```text
//! fstatat(dirfd, "self/task/", &st, 0);
//! if (st.st_nlink <= 2) IMMEDIATE_CRASH();     // "a task directory with no tasks"
//! ```
//!
//! which is Chromium's zygote deciding whether it is in the single-threaded
//! pre-fork state (`build/imgout/chromium.bin`, `0xbc0709c` `cmp $0x2,%rbx` /
//! `0xbc070a0` `jbe` into its own `int3;ud2`). This kernel answered the flat `2`
//! that any *untracked* directory gets, so every zygote died on startup and
//! `--dump-dom` produced no bytes at all (`docs/notes/07-chromium-analysis.md`
//! §4aa).
//!
//! # The rules, measured on a current host (Linux 7.3.4) rather than assumed
//!
//! | path                     | nlink            | what it is                        |
//! |--------------------------|------------------|-----------------------------------|
//! | `/proc/<pid>/task`       | `2 + tasks`      | `3` single-threaded, `n + 3` for `n` extra |
//! | `/proc/<pid>`            | `2 + subdirs`    | `9` = `2 + 7`                     |
//! | `/proc/<pid>/task/<tid>` | `2 + subdirs`    | `7` = `2 + 5`                     |
//! | `/proc/<pid>/fd`, `/ns`  | `2`              | nothing below them                |
//! | `/proc/sys` and below    | **`1`**          | a rule of its own                 |
//! | `/proc/bus`              | `2 + subdirs`    | `4` = `2 + pci`                   |
//! | `/sys`, `/sys/kernel`    | `2 + subdirs`    | `13` = `2 + 11`, `18` = `2 + 16`  |
//!
//! `/proc` itself is **not** any of these: its count follows the process table
//! (`405 -> 408` when three `sleep` children start, back when they are reaped)
//! *and* something else no formula fitted, so this kernel keeps the fallback
//! there and names it a gap instead of inventing a number.
//!
//! # What this program pins
//!
//! The rules as **movements**, because a constant passes for the wrong reason:
//! a fresh process's `task/` is `3`, creating two threads makes it `5`, joining
//! them brings it back to `3` - and the same thread that moved `task/` must
//! *not* move `/proc/<pid>/fd`, which has no subdirectories at all. The
//! `/proc/sys` subtree is claimed the other way: it stays `1` while a
//! neighbour with the same shape follows the ordinary rule.

use mpos_user_tests::Report;
use rustix::fs::{FileType, Mode, OFlags, stat, statat};

/// A `struct stat`'s link count as one type on both machines.
///
/// The cast is load-bearing on one of them and redundant on the other:
/// `st_nlink` is `u64` in x86_64's `struct stat` and `u32` in aarch64's, so a
/// program that names the field without it does not compile on the second
/// machine. Clippy runs on the x86_64 arm alone, where it reads as unnecessary
/// - hence the allow, whose reason is the two layouts.
#[allow(clippy::unnecessary_cast)]
fn links_of(st: &rustix::fs::Stat) -> u64 {
    st.st_nlink as u64
}

/// `stat` a path and answer its link count, or `None` if the call refused.
fn nlink_of(path: &str) -> Option<u64> {
    stat(path).ok().map(|st| links_of(&st))
}

/// The thread count this process has, from the directory under test rather
/// than from a second source that could disagree with it.
fn task_entries(pid: u32) -> usize {
    let dir = format!("/proc/{pid}/task");
    std::fs::read_dir(&dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn main() {
    let mut report = Report::new();
    let pid = std::process::id();

    // ---- the directory Chromium reads -------------------------------------
    //
    // The claim is not "3" - a `2 + 1` implementation and a hardcoded `3` both
    // answer that. It is that the number tracks the *task list*, so it moves
    // when threads are made and comes back when they are joined.
    let task_dir = format!("/proc/{pid}/task");
    let base = nlink_of(&task_dir);
    report.check(
        &format!("a single-threaded process's task directory is 3 (got {base:?})"),
        base == Some(3),
    );
    report.check(
        &format!(
            "and it is 2 plus the entries it lists ({base:?} for {} entries)",
            task_entries(pid)
        ),
        base == Some(2 + task_entries(pid) as u64),
    );

    // Two threads that park on a barrier, so both exist while the count is read.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let rx = rx.clone();
        let done_tx = done_tx.clone();
        handles.push(std::thread::spawn(move || {
            // Park until the parent has read the count.
            let _ = rx.lock().map(|guard| guard.recv());
            let _ = done_tx.send(());
        }));
    }
    // A thread that has been spawned is not necessarily in the table yet: give
    // the two a moment to arrive, bounded so a refusal is a failure and not a
    // hang.
    let mut threads_now = 1;
    for _ in 0..200 {
        threads_now = task_entries(pid);
        if threads_now >= 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let with_threads = nlink_of(&task_dir);
    report.check(
        &format!("three threads make it 5 (got {with_threads:?}, {threads_now} entries)"),
        with_threads == Some(5),
    );
    report.check(
        &format!(
            "and it is still 2 plus the entries it lists ({with_threads:?} for {threads_now} entries)"
        ),
        with_threads == Some(2 + threads_now as u64),
    );

    // The same process's `fd` directory has no subdirectories, so the very
    // threads that moved `task/` must leave it alone. A `2 + entries`
    // implementation counts files here and answers tens.
    let fd_dir = format!("/proc/{pid}/fd");
    let fd_links = nlink_of(&fd_dir);
    report.check(
        &format!("a process's fd directory stays 2 whatever it holds (got {fd_links:?})"),
        fd_links == Some(2),
    );

    // Release the threads and put `task/` back, so the claim is about a number
    // that moves in both directions rather than one that only grew.
    drop(tx);
    for handle in handles {
        let _ = handle.join();
    }
    let _ = done_rx.try_recv();
    let mut back_to_one = None;
    for _ in 0..200 {
        if task_entries(pid) == 1 {
            back_to_one = nlink_of(&task_dir);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    report.check(
        &format!("and joined threads bring it back to 3 (got {back_to_one:?})"),
        back_to_one == Some(3),
    );

    // ---- the rule beside it that points the other way ----------------------
    //
    // `/proc/sys` answers `1` however much it holds, while `/proc`'s own
    // ordinary directories answer `2 + subdirs`. Both are claimed, because an
    // implementation that applied one rule everywhere passes either alone: an
    // `2 + subdirs` kernel makes `sys` wrong, and a constant-1 kernel makes
    // `/proc/bus` wrong.
    let sys = nlink_of("/proc/sys");
    report.check(
        &format!("/proc/sys answers 1 whatever it holds (got {sys:?})"),
        sys == Some(1),
    );
    let sys_kernel = nlink_of("/proc/sys/kernel");
    report.check(
        &format!("and so does everything beneath it (got {sys_kernel:?})"),
        sys_kernel == Some(1),
    );
    let pid_dir = nlink_of(&format!("/proc/{pid}"));
    report.check(
        &format!("while a pid directory follows 2 + its subdirectories (got {pid_dir:?})"),
        pid_dir.is_some_and(|n| n > 2),
    );
    let bus = nlink_of("/proc/bus");
    report.check(
        &format!("and /proc/bus is 2 + pci, not 1 (got {bus:?})"),
        bus == Some(3),
    );

    // ---- sysfs uses the same rule -----------------------------------------
    //
    // `/sys` is `2 + its subdirectories` on Linux (`13` = `2 + 11`), and its
    // dirs are directories like any other. Named separately because sysfs is a
    // second filesystem and a fix in one tree is not a fix in the other.
    let sysfs = nlink_of("/sys");
    report.check(
        &format!("/sys follows 2 + its subdirectories (got {sysfs:?})"),
        sysfs.is_some_and(|n| n > 2),
    );

    // ---- the discriminator that a "count every entry" rule gets wrong ----
    //
    // `/proc/bus` above holds one file's worth of entries but exactly one
    // *subdirectory*, so an implementation that counted entries would answer
    // more than 3 there. This is the same question one level down: a pid
    // directory holds dozens of files and four directories, and must answer 6.
    // (A tmpfs directory of this program's own was going to be the control for
    // this, and it is not: `RamDir` answers the trait's flat `0` - i.e. `2` -
    // exactly as procfs did, so the control found a *second* instance of this
    // defect rather than confirming the first. Named in `docs/notes/` §4aa as
    // the next one to fix; claiming it here would make this program about a
    // third filesystem.)
    let pid_path = format!("/proc/{pid}");
    let mut entries_here = 0usize;
    let mut dirs_here = 0u64;
    if let Ok(listed) = std::fs::read_dir(&pid_path) {
        for entry in listed.filter_map(Result::ok) {
            entries_here += 1;
            // `symlink_metadata`, not `metadata`: `/proc/<pid>` holds `exe`,
            // `cwd` and `root` as *links* to directories, and following them
            // would count three subdirectories this directory does not have -
            // which is exactly the mistake this claim's first version made
            // (it read 6 where the answer is 4, and the kernel was right).
            let is_dir = std::fs::symlink_metadata(entry.path())
                .map(|m| m.is_dir())
                .unwrap_or(false);
            dirs_here += u64::from(is_dir);
        }
    }
    report.check(
        &format!(
            "a pid directory counts its {dirs_here} subdirectories, not its {entries_here} entries (got {pid_dir:?})"
        ),
        pid_dir == Some(2 + dirs_here),
    );
    // And the two numbers really are different here, or the claim above would
    // pass for a "count everything" implementation too.
    report.check(
        &format!("({entries_here} entries against {dirs_here} subdirectories)"),
        entries_here as u64 > dirs_here,
    );

    // A `statat` on a directory through a descriptor is the shape Chromium
    // uses (`fstatat(dirfd, "self/task/", ...)`), so it is claimed through that
    // door rather than only through `stat`: an implementation that answered
    // correctly on the path lookup and not under a dirfd would pass everything
    // above.
    let proc_dir = rustix::fs::open(
        "/proc/self",
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    );
    match proc_dir {
        Ok(fd) => {
            let answer = statat(&fd, "task/", rustix::fs::AtFlags::empty())
                .ok()
                .map(|st| links_of(&st));
            report.check(
                &format!("fstatat(dirfd, \"task/\") answers the same 3 (got {answer:?})"),
                answer == Some(3),
            );
            // And the answer arrives with the right kind, since a caller reads
            // the number off a `struct stat` it first checked was a directory.
            report.check(
                "and the entry it named is a directory",
                statat(&fd, "task/", rustix::fs::AtFlags::empty())
                    .ok()
                    .map(|st| FileType::from_raw_mode(st.st_mode))
                    .is_some_and(FileType::is_dir),
            );
        }
        Err(e) => report.check(&format!("opening /proc/self: {e}"), false),
    }

    report.finish();
}
