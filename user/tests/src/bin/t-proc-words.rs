//! The two `/proc/<pid>` words that are *views* of state this kernel already
//! answers elsewhere: `personality` and `oom_adj`.
//!
//! Both are compatibility files - one is a read-only window on `personality(2)`,
//! the other is the **deprecated** fifteen-step scale the OOM killer used before
//! `oom_score_adj` replaced it - and both are easy to serve wrongly in the same
//! way: by keeping a second copy of a fact that already has an owner. The claims
//! here are therefore *relations* between two doors, not constants:
//!
//! * `personality` is `personality(0xffffffff)`'s word, in eight lowercase hex
//!   digits, at `0400` and read-only.
//! * `oom_adj` is `oom_score_adj * 15 / 1000`, with `-17` as the sentinel for
//!   the modern floor; writing it moves the modern word and writing the modern
//!   word moves it back.
//!
//! Measured on Linux 7.2.14 first (`build/scratch/procsyscall/{pers,oomadj}.c`).

use mpos_user_tests::Report;

/// The `personality(2)` query word.
const PERSONALITY_QUERY: usize = 0xffff_ffff;

/// `errno` values these files answer with.
const EACCES: i32 = 13;
const EINVAL: i32 = 22;
const ENOENT: i32 = 2;

/// Read `path` as text, trimmed of its trailing newline.
fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim_end_matches('\n').to_string())
}

/// Write `text` to `path`, answering `0` or the errno.
fn write(path: &str, text: &str) -> i32 {
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(mut file) => match file.write_all(text.as_bytes()) {
            Ok(()) => 0,
            Err(e) => e.raw_os_error().unwrap_or(0),
        },
        Err(e) => e.raw_os_error().unwrap_or(0),
    }
}

/// The file mode of `path`, as the `0777` bits.
fn mode(path: &str) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| meta.mode() & 0o777)
}

/// The effective uid, which decides which refusal `oom_adj -17` owes.
fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

fn main() {
    let mut r = Report::new();
    let me = std::process::id();

    // ---- personality -----------------------------------------------------
    let pers_path = format!("/proc/{me}/personality");
    let word = read(&pers_path);
    r.check(
        &format!("personality is eight lowercase hex digits (got {word:?})"),
        word.as_deref().is_some_and(|text| {
            text.len() == 8
                && text
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        }),
    );
    // The relation: the file and the syscall are one fact. The query answers the
    // word in force, and the file must print that same number.
    let queried = unsafe { libc::syscall(libc::SYS_personality, PERSONALITY_QUERY) } as i64;
    let from_syscall = format!("{:08x}", queried as u32);
    r.check(
        &format!(
            "personality's word is personality(2)'s query answer ({from_syscall:?} vs {word:?})"
        ),
        word.as_deref() == Some(from_syscall.as_str()),
    );
    r.check("personality is 0400", mode(&pers_path) == Some(0o400));
    r.check(
        &format!(
            "personality is read-only (write answered {})",
            write(&pers_path, "0\n")
        ),
        write(&pers_path, "0\n") == EACCES,
    );
    // Another process's word is readable: this is not a ptrace-gated file.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::pause() };
        std::process::exit(0);
    }
    if child > 0 {
        let other = read(&format!("/proc/{child}/personality"));
        r.check(
            &format!("another process's personality is readable (got {other:?})"),
            other.as_deref() == Some(from_syscall.as_str()),
        );
        unsafe {
            libc::kill(child, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(child, &mut status, 0);
        }
    }

    // ---- oom_adj ---------------------------------------------------------
    let adj_path = format!("/proc/{me}/oom_adj");
    let score_path = format!("/proc/{me}/oom_score_adj");
    r.check("oom_adj is 0644", mode(&adj_path) == Some(0o644));

    // A function of the two files, both directions: writing the modern word must
    // show through the deprecated one, which is the whole point of it being a
    // view rather than a second store.
    let score = |value: i32| -> Option<(i32, i32)> {
        write(&score_path, &format!("{value}\n"));
        let modern = read(&score_path)?.parse::<i32>().ok()?;
        let old = read(&adj_path)?.parse::<i32>().ok()?;
        Some((modern, old))
    };
    // **From the floor upward**, and both halves of that are the measurement.
    // `oom_score_adj_min` is the most protective value ever written - inherited
    // from the process's parent too - and nothing may go below it without
    // `CAP_SYS_RESOURCE`. So the values this program may write are the ones at
    // or above the word it *starts* with, in ascending order: writing `1000`
    // first would raise the floor and make every later step a refusal that says
    // nothing about the conversion.
    let start = read(&score_path)
        .and_then(|text| text.parse::<i32>().ok())
        .unwrap_or(-1000);
    r.check(
        &format!("oom_score_adj starts at or above the floor it was given ({start})"),
        start >= -1000,
    );
    for value in [-1000, -500, 0, 15, 200, 1000]
        .into_iter()
        .filter(|value| *value >= start)
    {
        let got = score(value);
        // Linux's conversion, done here so the claim is about the *relation*:
        // `oom_adj` is `oom_score_adj * 15 / 1000`, with `-1000` reading as the
        // `-17` sentinel rather than through the multiplication.
        let expect = if value == -1000 {
            -17
        } else {
            value * 15 / 1000
        };
        r.check(
            &format!(
                "oom_adj is oom_score_adj*15/1000 (score {value} -> want {expect}, got {got:?})"
            ),
            got == Some((value, expect)),
        );
    }

    // Writing the deprecated scale moves the modern one: 15 is the top of the old
    // scale and 1000 the top of the new, so the two ends must meet exactly.
    let wrote = write(&adj_path, "15\n");
    let pair = (read(&score_path), read(&adj_path));
    r.check(
        &format!("writing oom_adj 15 sets oom_score_adj to 1000 (wrote {wrote}, got {pair:?})"),
        wrote == 0 && pair.0.as_deref() == Some("1000") && pair.1.as_deref() == Some("15"),
    );
    // And the round trip back through the modern file.
    write(&score_path, "200\n");
    r.check(
        &format!(
            "writing oom_score_adj 200 reads back as oom_adj 3 (got {:?})",
            read(&adj_path)
        ),
        read(&adj_path).as_deref() == Some("3"),
    );

    // Three refusals, measured on the host: out of range and non-numeric are
    // `EINVAL`; the disable sentinel is `EACCES` for a caller without
    // `CAP_SYS_RESOURCE` - and the guest's payload *is* root, so the same
    // program has to expect whichever answer the caller's uid decides.
    for bad in ["16", "-18", "abc"] {
        let got = write(&adj_path, &format!("{bad}\n"));
        r.check(
            &format!("oom_adj {bad:?} is EINVAL (got {got})"),
            got == EINVAL,
        );
    }
    let sentinel = write(&adj_path, "-17\n");
    if euid() != 0 {
        r.check(
            &format!("oom_adj -17 without the capability is EACCES (got {sentinel})"),
            sentinel == EACCES,
        );
    } else {
        // Root may take the sentinel; what is pinned is that it lands on the
        // modern floor rather than on `-1133`, a number the scale does not have.
        r.check(
            &format!(
                "oom_adj -17 lands on the modern floor (write answered {sentinel}, oom_score_adj {:?})",
                read(&score_path)
            ),
            sentinel == 0 && read(&score_path).as_deref() == Some("-1000"),
        );
    }

    // Not claimed here: the modern file's own floor refusal. `oom_score_adj_min`
    // is a *separate* number from the current value - the lowest ever written,
    // which starts at the scale's floor - so a program cannot discover it from
    // either file, and `start - 1` would be a guess about a number this test
    // never read. The `-17` claim above is the floor rule it *can* see: the
    // disable sentinel needs `CAP_SYS_RESOURCE` whatever the floor is.

    // A pid that does not exist names neither file.
    let absent_personality = read("/proc/999999/personality");
    let absent_adj = read("/proc/999999/oom_adj");
    r.check(
        "a pid that does not exist has neither file",
        absent_personality.is_none() && absent_adj.is_none(),
    );
    // The file is still there and still readable after every refusal above -
    // a refusal is the file's answer, not a vanished one.
    r.check(
        "the file still answers after the refusals",
        read(&adj_path).is_some() && read(&score_path).is_some(),
    );
    let _ = ENOENT;

    r.finish();
}
