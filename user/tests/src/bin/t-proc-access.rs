//! Who may **read** another process's `/proc/<pid>` tree.
//!
//! Measured on a current host first (`build/scratch/ptraceacc/p.c`, Linux
//! 7.2.11): the tree answers access by two different rules, and the split is
//! visible the moment a caller is neither the process nor root.
//!
//! * **The reported word is the verdict** for the files and directories under
//!   a pid directory. A `0444` `status`/`stat`/`cmdline` answers anyone, while
//!   `0400` `maps`/`environ` refuses another user with `EACCES` and the `0500`
//!   `fd` directory refuses its own listing. (Linux's `proc_pid_permission`
//!   adds a `ptrace_may_access` test on top of the word for some nodes; for
//!   these the word alone reproduced every measured answer.)
//! * **A magic link asks `ptrace_may_access`**, which is why
//!   `readlink("/proc/<pid>/fd/0")` and `.../ns/net` are `EACCES` to another
//!   user even though the link's own word is `0777`.
//!
//! The pin makes its own reader: a child that drops to `nobody` and probes two
//! trees - a root process's and a `setuid`-dropped one's - then writes what it
//! saw back through a pipe. The parent (root) supplies the second half: root
//! reads everything.
//!
//! Run with `MPOS_ON_HOST=1` it does the same thing on real Linux, which is
//! the reference the claims were written from.

use mpos_user_tests::{Report, host_run};
use rustix::fs::{AtFlags, statat};

/// The ids the reader child drops to - `nobody`, present in both images here.
const NOBODY: u32 = 65_534;

/// The invoking user's ids, for the host run, where the fixture cannot drop.
fn host_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn host_gid() -> u32 {
    unsafe { libc::getegid() }
}

/// A fixture's lifetime is its parent's, not a number of seconds.
///
/// It blocks on the read end of a pipe whose write end only the parent holds,
/// so it ends when the parent closes that end - at the parent's last line, or
/// when the parent dies. A nap was the same idea done with a clock, and it lost
/// the race it was meant to avoid: the fixtures slept 120 s while the program's
/// own budget in `tests.toml` is also 120 s, so a lane slow enough to matter
/// could see a fixture expire mid-matrix, and the claim
/// `the dropped process's entry still names it` then failed on a machine rather
/// than on a rule (once here, and once when the fixture slept 20 s, `docs/notes/`
/// §7.9p). A pipe end cannot expire.
fn wait_while_the_parent_holds_it() {
    let mut byte = 0u8;
    loop {
        let got = unsafe { libc::read(0, (&mut byte as *mut u8).cast(), 1) };
        if got <= 0 {
            return;
        }
    }
}

fn open_answer(path: &str) -> Result<usize, i32> {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            match file.read(&mut buf) {
                Ok(n) => Ok(n),
                Err(e) => Err(e.raw_os_error().unwrap_or(0)),
            }
        }
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

fn list_answer(path: &str) -> Result<usize, i32> {
    match std::fs::read_dir(path) {
        Ok(entries) => Ok(entries.count()),
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

fn link_answer(path: &str) -> Result<usize, i32> {
    match std::fs::read_link(path) {
        Ok(_) => Ok(0),
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

/// The matrix the reader reports, one `key=value` per line.
fn reader_matrix(roots: &str, dropped: &str) -> String {
    let mut out = String::new();
    let cases: [(&str, String); 8] = [
        ("status_root", format!("{roots}/status")),
        ("maps_root", format!("{roots}/maps")),
        ("environ_root", format!("{roots}/environ")),
        ("fd_root", format!("{roots}/fd")),
        ("fd_link_root", format!("{roots}/fd/0")),
        ("status_dropped", format!("{dropped}/status")),
        ("maps_dropped", format!("{dropped}/maps")),
        ("fd_link_dropped", format!("{dropped}/fd/0")),
    ];
    for (key, path) in &cases {
        let answer = match *key {
            "fd_root" => list_answer(path),
            k if k.starts_with("fd_link") => link_answer(path),
            _ => open_answer(path),
        };
        match answer {
            Ok(n) => out.push_str(&format!("{key}=ok:{n}\n")),
            Err(e) => out.push_str(&format!("{key}=errno:{e}\n")),
        }
    }
    // The reader's own tree is its own: both the file and the descriptor's
    // link answer, whatever the two rules say about anyone else's.
    for (key, path) in [
        ("own_status", "/proc/self/status"),
        ("own_fd_link", "/proc/self/fd/0"),
    ] {
        let answer = if key == "own_fd_link" {
            link_answer(path)
        } else {
            open_answer(path)
        };
        match answer {
            Ok(n) => out.push_str(&format!("{key}=ok:{n}\n")),
            Err(e) => out.push_str(&format!("{key}=errno:{e}\n")),
        }
    }
    out
}

fn main() {
    if let Some(mode) = std::env::args().nth(1) {
        if mode == "--reader" {
            let roots = std::env::args().nth(2).unwrap_or_default();
            let dropped = std::env::args().nth(3).unwrap_or_default();
            unsafe {
                libc::setgroups(0, core::ptr::null());
                libc::setgid(NOBODY);
                libc::setuid(NOBODY);
            }
            print!("{}", reader_matrix(&roots, &dropped));
            return;
        }
        if mode == "--sleep" {
            wait_while_the_parent_holds_it();
            return;
        }
        if mode == "--dropped" {
            unsafe {
                libc::setgroups(0, core::ptr::null());
                libc::setgid(NOBODY);
                libc::setuid(NOBODY);
            }
            wait_while_the_parent_holds_it();
            return;
        }
    }

    let mut r = Report::new();
    let exe = std::env::current_exe().expect("the pin's own path");
    // Each fixture's stdin is a pipe only this parent holds, so a fixture ends
    // when this process drops its end - at the last claim, or when it dies. A
    // nap was the same idea done with a clock, and it lost the race it was
    // meant to avoid: the fixtures slept 120 s while this program's own budget
    // in `tests.toml` is also 120 s, so a lane slow enough to matter could see
    // one expire mid-matrix and `the dropped process's entry still names it`
    // then failed on a machine rather than on a rule (once here, and once when
    // the fixture slept 20 s: `docs/notes/` §7.9p). A pipe end cannot expire.
    let mut roots = std::process::Command::new(&exe)
        .arg("--sleep")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("a root process to read");
    let mut dropped = std::process::Command::new(&exe)
        .arg("--dropped")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("a dropped process to read");
    let root_pid = roots.id().to_string();
    let dropped_pid = dropped.id().to_string();

    let reader = std::process::Command::new(&exe)
        .arg("--reader")
        .arg(format!("/proc/{root_pid}"))
        .arg(format!("/proc/{dropped_pid}"))
        .output()
        .expect("the reader to run");
    let said = String::from_utf8_lossy(&reader.stdout);
    let field = |key: &str| -> Option<String> {
        said.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")).map(String::from))
    };
    println!("     reader: {}", said.trim().replace('\n', " "));

    // The word is the verdict: what a world-readable file answers, and what
    // the two owner-only ones refuse.
    r.check(
        "another user reads a root process's world-readable status",
        field("status_root").map(|s| s.starts_with("ok:") && s != "ok:0") == Some(true),
    );
    r.check(
        "but root's maps is the owner's alone (EACCES 13)",
        field("maps_root") == Some(String::from("errno:13")),
    );
    r.check(
        "and so is root's environ (EACCES 13)",
        field("environ_root") == Some(String::from("errno:13")),
    );
    r.check(
        "the fd directory cannot be listed by another user (EACCES 13)",
        field("fd_root") == Some(String::from("errno:13")),
    );
    // A dropped process answers the same words: the two rules are about the
    // node, not about who the process is.
    r.check(
        "a dropped process's status is world-readable too",
        field("status_dropped").map(|s| s.starts_with("ok:") && s != "ok:0") == Some(true),
    );
    r.check(
        "its maps is still the owner's alone",
        field("maps_dropped") == Some(String::from("errno:13")),
    );
    // And the magic links, which are `0777` and still refuse: the access check
    // is about the process, not the word.
    r.check(
        "a foreign process's descriptor link is not readable (EACCES 13)",
        field("fd_link_root") == Some(String::from("errno:13")),
    );
    r.check(
        "neither is a dropped process's",
        field("fd_link_dropped") == Some(String::from("errno:13")),
    );

    // The reader's own tree is its own - the rule must not become "nobody may
    // read /proc".
    r.check(
        "the reader reads its own status",
        field("own_status").map(|s| s.starts_with("ok:")) == Some(true),
    );
    r.check(
        "and its own descriptor's link",
        field("own_fd_link") == Some(String::from("ok:0")),
    );

    // Root reads everything: the access rules are for callers that are not.
    r.check(
        "root reads a dropped process's maps",
        open_answer(&format!("/proc/{dropped_pid}/maps")).map(|n| n > 0) == Ok(true),
    );
    r.check(
        "and follows its descriptor's link",
        link_answer(&format!("/proc/{dropped_pid}/fd/0")) == Ok(0),
    );
    // The ownership the tree already carries is not affected by any of this.
    let owner = statat(
        rustix::fs::CWD,
        &format!("/proc/{dropped_pid}"),
        AtFlags::empty(),
    )
    .map(|st| (st.st_uid, st.st_gid))
    .map_err(|e| e.raw_os_error());
    // On a host run this program is not root, so its fixture never drops to
    // `nobody` and the ids there are the invoking user's: what is claimable is
    // that the entry exists and names *its* owner, which is the rule this
    // claim is about. On the machine the fixture does drop, and the ids are
    // `nobody`'s.
    let named_its_owner = if host_run() {
        let looked = rustix::fs::stat(format!("/proc/{dropped_pid}"));
        looked.map(|st| (st.st_uid, st.st_gid)) == Ok((host_uid(), host_gid()))
    } else {
        owner == Ok((NOBODY, NOBODY))
    };
    r.check(
        &format!("and the dropped process's entry still names it ({owner:?})"),
        named_its_owner,
    );

    // Dropping the write ends ends the fixtures politely; the kill is the one
    // that cannot be missed (a fixture that never reached its read would
    // otherwise sit there for the rest of the sweep).
    drop(roots.stdin.take());
    drop(dropped.stdin.take());
    let _ = roots.kill();
    let _ = dropped.kill();
    let _ = roots.wait();
    let _ = dropped.wait();
    r.finish();
}
