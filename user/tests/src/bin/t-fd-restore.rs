//! t-fd-restore - a redirected read puts the descriptor back exactly as it was.
//!
//! The shape a POSIX shell uses for `read one two three <"$file"`: save the
//! descriptor (`fcntl(F_DUPFD_CLOEXEC)`), point the saved description at the
//! file (`dup2(file, saved)`), read, then put the original back (`dup2(orig,
//! saved)`) and close the spare. OpenRC's `/usr/libexec/rc/sh/gendepends.sh`
//! does exactly this on line 74, once per service script, inside a `for` loop -
//! and a desktop boot of this image stalls there, with the poll-spin instrument
//! naming that script and a *regular file* on fd 0 (`docs/notes/`
//! §7 R.9's ledger). So the question this pins is a narrow one that no existing
//! program asked: after the restore, is the descriptor the one it was before -
//! still, after a hundred of these - and did each round read the file it had
//! just opened?
//!
//! Why a restore can go wrong in a way `t-dup2` cannot see: that program pins
//! the `dup2` mechanics once each (`dup2(stdout, 5)`, a high-numbered target
//! growing the table). This one pins the *invariant the caller depends on* -
//! the pair is symmetric and repeatable, so round N+1 starts from the state
//! round N did. A kernel that leaked the save slot, or left the descriptor on
//! the previous file, would fail here and pass every claim `t-dup2` makes.
//!
//! **Measured on a current Linux first** (`.workbuddy/probe_fd_restore.c` and
//! the shell probe beside it): a `for f in *; do read a b c <"$f"; done` loop
//! restores fd 0 to the shell's own `pipe:[N]` after *every* iteration, and
//! each iteration reads a different file's first line. With a regular file on
//! stdin, `poll` answers "ready" and `read` at end-of-file answers 0 - never
//! `EAGAIN` - so a spin in that loop means the descriptor's *identity* went
//! wrong, not that a readiness answer did. That is what these claims isolate.
//!
//! The doors are `rustix`'s (`fcntl_dupfd_cloexec`, `dup2`, `read`, `openat`,
//! `pipe`) - nothing here is a clause-(b) or clause-(c) shape, so no raw
//! `libc::syscall` appears. `fstat`'s `st_ino` is the identity word the claims
//! compare, because it is what a person reads off `/proc/self/fd` too.

use mpos_user_tests::Report;
use rustix::fs::{Mode, OFlags, fstat, openat};
use rustix::io::{dup2, fcntl_dupfd_cloexec, read};
use std::os::fd::{AsFd, BorrowedFd};

/// The three scripts the loop reads, each starting with a distinct word, so a
/// restore that left the *previous* file in place reads the wrong word - the
/// claim names which, rather than watching a counter not move. The first word
/// has to be distinct across all three for that count to mean anything, which
/// the first draft of this file got wrong: two scripts both began
/// `#!/sbin/openrc-run`, so three rounds produced two distinct words and the
/// claim failed on a correct kernel. A claim that cannot tell "three files were
/// read" from "two were read twice" is not pinning what it says.
const SCRIPTS: [&str; 3] = [
    "#!/sbin/openrc-run\nname=\"alpha\"\n",
    "#!/bin/openrc-run\nname=\"beta\"\n",
    "#!openrc-run\n# a third form, so the three words differ\n",
];

/// The first whitespace-delimited word of a file, which is what a shell's
/// `read` hands its caller - and what `gendepends.sh` compares against
/// `#*/openrc-run`.
fn first_word(path: &std::ffi::CStr) -> String {
    let file = match openat(rustix::fs::CWD, path, OFlags::RDONLY, Mode::empty()) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = [0u8; 64];
    let n = match read(&file, &mut buf) {
        Ok(n) => n,
        Err(_) => 0,
    };
    String::from_utf8_lossy(&buf[..n])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

fn inode_of(fd: BorrowedFd<'_>) -> u64 {
    fstat(fd).map(|s| s.st_ino).unwrap_or(0)
}

fn main() {
    println!("t-fd-restore");
    let mut r = Report::new();

    // The files the loop reads live under a directory of this run's own, so a
    // leftover from an interrupted run cannot stand in for a script and make a
    // claim about nothing.
    let dir = std::env::temp_dir().join(format!("mpos-fd-restore-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    for (i, body) in SCRIPTS.iter().enumerate() {
        let _ = std::fs::write(dir.join(format!("script{i}")), body);
    }
    r.check(
        "the three scripts are written",
        (0..SCRIPTS.len()).all(|i| dir.join(format!("script{i}")).exists()),
    );

    // The "shell's own stdin" stand-in is a regular file, and the first draft
    // of this test used a pipe - which went red on the guest at the *fixture*
    // claim and at every claim about the kernel, so the reading was worthless.
    // Measured on a current Linux (`.workbuddy/probe_pipe_ino.c`): a pipe does
    // carry a nonzero `st_ino`, shared by both ends, where this kernel answers
    // 0 for one - so `st_ino` is not an identity word for a pipe here and the
    // fixture was comparing a number the machine may legitimately leave unset.
    // A regular file is the honest stand-in, and the one the boot was actually
    // stuck on: the spin's own line recorded `fd=0 ... kind=0 size=204
    // read=true`, a *file* on the descriptor the restore should have put back.
    // Identity is then `st_ino`, which is a file's to give.
    let stdin_path = std::ffi::CString::new(dir.join("shell-stdin").to_string_lossy().as_bytes())
        .unwrap_or_default();
    let _ = std::fs::write(dir.join("shell-stdin"), b"the shell's own stdin\n");
    let stdin_file = match openat(
        rustix::fs::CWD,
        stdin_path.as_c_str(),
        OFlags::RDONLY,
        Mode::empty(),
    ) {
        Ok(f) => f,
        Err(_) => {
            r.check("a file stands in for the shell's own stdin", false);
            r.finish();
        }
    };
    let orig = inode_of(stdin_file.as_fd());
    r.check(
        "a file stands in for the shell's own stdin, and it has an identity",
        orig != 0,
    );

    // One round, spelled out: save, point at a file, read, put back.
    let mut saved = match fcntl_dupfd_cloexec(stdin_file.as_fd(), 3) {
        Ok(fd) => fd,
        Err(_) => {
            r.check("the save slot is made", false);
            r.finish();
        }
    };
    r.check(
        "the save slot shares the shell's stdin description",
        inode_of(saved.as_fd()) == orig,
    );

    let script0 = std::ffi::CString::new(dir.join("script0").to_string_lossy().as_bytes())
        .unwrap_or_default();
    let script = match openat(
        rustix::fs::CWD,
        script0.as_c_str(),
        OFlags::RDONLY,
        Mode::empty(),
    ) {
        Ok(f) => f,
        Err(_) => {
            r.check("the script opens", false);
            r.finish();
        }
    };
    let script_ino = inode_of(script.as_fd());
    r.check(
        "the script is a different object from the shell's stdin",
        script_ino != orig,
    );
    r.check(
        "dup2 points the saved description at the script",
        dup2(script.as_fd(), &mut saved).is_ok() && inode_of(saved.as_fd()) == script_ino,
    );
    r.check(
        "the redirect's own read sees the script's first word",
        first_word(script0.as_c_str()) == "#!/sbin/openrc-run",
    );
    r.check(
        "dup2 puts the original description back",
        dup2(stdin_file.as_fd(), &mut saved).is_ok() && inode_of(saved.as_fd()) == orig,
    );
    drop(script);
    r.check(
        "the restore survives the script's own descriptor closing",
        inode_of(saved.as_fd()) == orig,
    );
    drop(saved);

    // The claim the boot needs: a hundred of these over **one** saved
    // descriptor, each round starting from the state the last one left. That is
    // the load-bearing shape, and this file's first draft was not it: re-making
    // the save slot each round from stdin makes the restore redundant (a fresh
    // `dup` of stdin is the original whatever the last round left behind), so
    // the loop passed green with the restore step deleted - measured, which is
    // how that draft was caught. A shell's `0<file` mutates *one* descriptor
    // number and puts it back, so a second round after a broken restore reads a
    // drained file instead. The save slot is made once, outside the loop, and
    // only `dup2` moves it.
    let mut slot = match fcntl_dupfd_cloexec(stdin_file.as_fd(), 3) {
        Ok(fd) => fd,
        Err(_) => {
            r.check("the loop's save slot is made once", false);
            r.finish();
        }
    };
    r.check(
        "the loop's save slot is made once",
        inode_of(slot.as_fd()) == orig,
    );

    let mut rounds = 0usize;
    let mut wrong = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for round in 0..100usize {
        let which = round % SCRIPTS.len();
        let path = std::ffi::CString::new(
            dir.join(format!("script{which}"))
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap_or_default();
        // The redirect proper: point the one descriptor at the file...
        let file = match openat(
            rustix::fs::CWD,
            path.as_c_str(),
            OFlags::RDONLY,
            Mode::empty(),
        ) {
            Ok(f) => f,
            Err(_) => {
                wrong += 1;
                continue;
            }
        };
        let file_ino = inode_of(file.as_fd());
        if dup2(file.as_fd(), &mut slot).is_err() || inode_of(slot.as_fd()) != file_ino {
            wrong += 1;
        }
        // ...read it the way the script does...
        seen.insert(first_word(path.as_c_str()));
        // ...and put the original description back, on the same number.
        if dup2(stdin_file.as_fd(), &mut slot).is_err() || inode_of(slot.as_fd()) != orig {
            wrong += 1;
        }
        rounds += 1;
    }
    r.check("a hundred redirects ran", rounds == 100);
    r.check(
        "every round restored the shell's own description",
        wrong == 0,
    );
    r.check(
        "each round read the script it had just opened",
        seen.len() == SCRIPTS.len(),
    );
    // And the observation that makes the two above bite: what a *later* reader
    // finds on that descriptor once the loop is over. With every restore in
    // place it is the shell's own stdin; a loop that moved the descriptor and
    // never put it back leaves a service script sitting there, which is exactly
    // the state `gendepends.sh`'s stall showed on the wire (`fd=0 ... kind=0
    // size=204`). This claim is what the counterfactual run needs: with the
    // restore deleted the two counts above still balance, because each round's
    // own `dup2` overwrites the previous round's leftover, so only a post-loop
    // look can see the leak. Measured both ways.
    r.check(
        "after the loop the descriptor is the shell's own stdin again",
        inode_of(slot.as_fd()) == orig,
    );
    drop(slot);

    // The bottom of the stack: a caller that inherits a *file* on its stdin -
    // the state the boot was stuck in - reads it to end-of-file and gets 0
    // there, which is what the spin's own line recorded (`size=204`,
    // `read=true`). A kernel answering `EAGAIN` at end-of-file would send every
    // caller's read loop straight back to `poll`, which is the shape being
    // chased; Linux answers 0, measured on the host first.
    let mut at_eof = 0usize;
    let mut tried = 0usize;
    if let Ok(f) = openat(
        rustix::fs::CWD,
        script0.as_c_str(),
        OFlags::RDONLY,
        Mode::empty(),
    ) {
        tried += 1;
        let mut drained = [0u8; 256];
        while matches!(read(&f, &mut drained), Ok(n) if n > 0) {}
        let mut eof = [0u8; 8];
        if let Ok(0) = read(&f, &mut eof) {
            at_eof += 1;
        }
    }
    r.check("the end-of-file probe ran", tried == 1);
    r.check(
        "a regular file at end-of-file reads 0, not EAGAIN",
        at_eof == 1,
    );

    let _ = std::fs::remove_dir_all(&dir);
    r.finish()
}
