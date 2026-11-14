//! `/proc/sys/fs/inotify`: the three ceilings `inotify` enforces.
//!
//! Measured on a current host first (`build/scratch/inoctl/r.c`, Linux
//! 7.2.14): the files are `0644` - root writes them, anyone else gets
//! `EACCES`; `abc`, `-1` and `2147483648` are each `EINVAL` with the value
//! untouched; and the numbers are *policy*: with `max_user_watches = 3` the
//! next `inotify_add_watch` answers `ENOSPC`, and with
//! `max_user_instances = 1` the next `inotify_init1` answers `EMFILE`.
//!
//! The machine's payload runs as root, so the claims that *write* a ceiling
//! run only there (on a host run they are named and skipped - an unprivileged
//! writer is refused, which is itself a claim here). The machine's own
//! defaults are Linux's 8192/128/16384; a tuned host answers different
//! numbers, which is why the shared claims only ask that each file parse as
//! a positive decimal.
use mpos_user_tests::Report;
use std::io::Write;

const BASE: &str = "/proc/sys/fs/inotify";

fn read(name: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("{BASE}/{name}")).ok()?;
    text.trim().parse().ok()
}

fn write(name: &str, value: &str) -> i32 {
    match std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{BASE}/{name}"))
    {
        Ok(mut file) => match file.write_all(value.as_bytes()) {
            Ok(()) => 0,
            Err(e) => e.raw_os_error().unwrap_or(-1),
        },
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

fn watches(low: u64) -> (u64, u64, i32) {
    // Add watches until the cap answers, counting how many were taken and
    // what the refusal was; `low` says how many the caller expects to fail
    // on, so the fixture directories are made fresh per run.
    let dir = std::env::temp_dir().join(format!("t-inotify-limits-{low}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok();
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
    let mut taken = 0u64;
    let mut errno = 0i32;
    for i in 0..8u64 {
        let sub = dir.join(format!("s{i}"));
        std::fs::create_dir_all(&sub).ok();
        let path = std::ffi::CString::new(sub.to_string_lossy().as_bytes()).unwrap();
        let wd = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_CREATE) };
        if wd < 0 {
            errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            break;
        }
        taken += 1;
    }
    unsafe { libc::close(fd) };
    let _ = std::fs::remove_dir_all(&dir);
    (taken, 8, errno)
}

fn main() {
    println!("t-inotify-limits");
    let mut report = Report::new();

    // The tree exists and answers a number - both machines.
    for name in [
        "max_queued_events",
        "max_user_instances",
        "max_user_watches",
    ] {
        report.check(
            &format!("/proc/sys/fs/inotify/{name} answers a count"),
            read(name).is_some_and(|n| n > 0),
        );
    }

    if mpos_user_tests::host_run() {
        // On a host run the payload is unprivileged, and the *open* is where
        // Linux refuses it - before any count is parsed, so even a good
        // number answers EACCES. That ordering is the claim worth making
        // here; the refusals a privileged writer sees are the machine's.
        for text in ["4096\n", "abc\n", "-1\n"] {
            report.check(
                &format!("an unprivileged write of {text:?} is refused with EACCES"),
                write("max_user_watches", text) == libc::EACCES,
            );
        }
        report.finish();
    }

    // Root sees the counts judged: a bad one is refused and changes nothing.
    let before = read("max_user_watches");
    let code = write("max_user_watches", "abc\n");
    report.check(
        "a non-numeric write is EINVAL",
        code == libc::EINVAL && read("max_user_watches") == before,
    );
    let code = write("max_user_watches", "-1\n");
    report.check(
        "a negative write is EINVAL",
        code == libc::EINVAL && read("max_user_watches") == before,
    );

    // The machine runs as root, so the ceilings can be moved - and the
    // numbers are enforced, which is what makes them policy.
    let original = read("max_user_watches").unwrap_or(0);
    report.check(
        "the machine's watch ceiling is Linux's default",
        original == 8192,
    );

    let code = write("max_user_watches", "2\n");
    report.check(
        "root can lower the watch ceiling",
        code == 0 && read("max_user_watches") == Some(2),
    );
    let (taken, _, errno) = watches(2);
    report.check(
        "the lowered ceiling bites: two watches taken, the third is ENOSPC",
        taken == 2 && errno == libc::ENOSPC,
    );

    let code = write("max_user_watches", &format!("{original}\n"));
    report.check(
        "raising it again is accepted and the file says so",
        code == 0 && read("max_user_watches") == Some(original),
    );
    let (taken, _, errno) = watches(8);
    report.check(
        "with the default back, eight watches are taken and nothing refuses",
        taken == 8 && errno == 0,
    );

    let instances = read("max_user_instances").unwrap_or(0);
    let code = write("max_user_instances", "1\n");
    // One instance is already held by this program's own descriptor below,
    // so the next one is the one that must be refused.
    let held = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
    let second = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
    report.check(
        "the lowered instance ceiling bites: the second descriptor is EMFILE",
        code == 0 && held >= 0 && second < 0 && errno == libc::EMFILE,
    );
    if held >= 0 {
        unsafe { libc::close(held) };
    }
    report.check(
        "the instance ceiling can be put back",
        write("max_user_instances", &format!("{instances}\n")) == 0,
    );

    // The queue's ceiling is per descriptor and only shows under overflow,
    // which `t-inotify` already fills 16384 times; here the claim is that
    // its file is the number the code reads.
    let queued = read("max_queued_events").unwrap_or(0);
    report.check(
        "the machine's queue ceiling is Linux's default",
        queued == 16384,
    );
    report.check(
        "the queue ceiling can be moved and put back",
        write("max_queued_events", "32\n") == 0
            && read("max_queued_events") == Some(32)
            && write("max_queued_events", &format!("{queued}\n")) == 0
            && read("max_queued_events") == Some(queued),
    );

    report.finish();
}
