//! `/proc/<pid>/fdinfo/<n>`: the four lines every descriptor has, and the
//! per-kind lines after them.
//!
//! The file is the only door for most of what it reports: `fcntl(F_GETFL)` and
//! `fcntl(F_GETFD)` need the descriptor *in the asking process*, so a debugger,
//! `lsof`, or a supervisor that has to know where somebody else's descriptor is
//! positioned has no other way to ask. That makes it worth pinning exactly, and
//! it is what this program does - with **relations** rather than constants
//! wherever a constant would be a claim about this kernel's numbering instead
//! of about the ABI:
//!
//! * `pos:` is the position `lseek(SEEK_CUR)` reports and a `read` advances.
//! * `flags:` is `F_GETFL` **or** `O_CLOEXEC` when `FD_CLOEXEC` is set - the
//!   bit `F_GETFL` deliberately does *not* report, measured on Linux 7.2.14
//!   (`build/scratch/fdinfo/cloexec.c`): `open(O_RDONLY|O_CLOEXEC)` answers
//!   `02100000` where `F_GETFL` answers `0100000`, and `F_SETFD(0)` takes it
//!   back out. So the claim is made against `F_GETFL` and the descriptor flag
//!   the program can also read, and a kernel that printed `F_GETFL` alone would
//!   fail it.
//! * `mnt_id:` equals the first column of the `/proc/self/mountinfo` line whose
//!   fields name the file's own mount - the number is a *correlation* id, so
//!   the claim is that the two files agree, not that either is a particular
//!   number.
//! * `ino:` equals `fstat`'s `st_ino` for the same descriptor.
//!
//! Those four relations are what a tool actually computes, so pinning them
//! keeps this kernel honest about the *linkage* rather than about the values.
//!
//! The per-kind half is pinned by the shapes this kernel's node kinds can
//! answer; each claim reads its line only when the kernel reports one, and the
//! test says which kinds it expects to have one (see the per-kind section).
//!
//! Measured on Linux 7.2.14 first (`build/scratch/fdinfo/*.c`), and this
//! program is green on that host as well as in this kernel's guest - the two
//! machines answer the same relations because they are relations.

use mpos_user_tests::Report;

/// The four generic keys, in Linux's order.
const KEYS: [&str; 4] = ["pos:", "flags:", "mnt_id:", "ino:"];

/// `O_CLOEXEC`, the *open* flag; `FD_CLOEXEC` is 1 and is a different number.
const O_CLOEXEC: i64 = 0o200_0000;
const FD_CLOEXEC: i32 = 1;

/// One parsed `fdinfo` body: the four generic numbers and the raw text of every
/// line, so a per-kind claim can look for its own key.
struct Fdinfo {
    pos: u64,
    flags: u32,
    mnt_id: u64,
    ino: u64,
    text: String,
}

impl Fdinfo {
    /// Read one descriptor's `fdinfo`, or `None` if the read failed.
    fn of(pid: u32, fd: i32) -> Option<Fdinfo> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
        let mut info = Fdinfo {
            pos: 0,
            flags: 0,
            mnt_id: 0,
            ino: 0,
            text: text.clone(),
        };
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("pos:\t") {
                info.pos = rest.parse().ok()?;
            } else if let Some(rest) = line.strip_prefix("flags:\t") {
                // Linux writes a literal `0` then the octal digits, which is
                // the same number as the octal spelling of the word.
                info.flags = u32::from_str_radix(rest, 8).ok()?;
            } else if let Some(rest) = line.strip_prefix("mnt_id:\t") {
                info.mnt_id = rest.parse().ok()?;
            } else if let Some(rest) = line.strip_prefix("ino:\t") {
                info.ino = rest.parse().ok()?;
            }
        }
        Some(info)
    }

    /// The text of the first line starting with `key`, or `None`.
    fn line(&self, key: &str) -> Option<&str> {
        self.text.lines().find(|line| line.starts_with(key))
    }
}

/// The mount id `/proc/self/mountinfo` gives the mount `path` is really on:
/// the **longest** mountpoint that prefixes it, and among equally long ones the
/// **last** in the file - which is the top of a stack.
///
/// Both halves are load-bearing and both were measured on the host this pin was
/// written against, which had two mounts on the one mountpoint `/tmp`: the first
/// match (`700`, a read-only `tmpfs`) is not the filesystem a file created there
/// lands on, and the top of the stack (`711`) is. So "the first line that says
/// `/tmp`" is not the answer to "which mount is this file on"; the answer is the
/// most specific mountpoint above it, and the newest among equals.
///
/// The comparison is by mountpoint rather than by device because that is the
/// correlation the number is *for*: a tool holding a descriptor reads this file
/// to learn which mount it belongs to, and the two files agreeing on that is the
/// whole claim.
fn mountinfo_id_of(path: &str) -> Option<u64> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, u64)> = None;
    for line in info.lines() {
        // `<id> <parent> <maj:min> <root> <mountpoint> <options> ...`
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        let mountpoint = fields[4];
        // A mountpoint of `/` covers everything; anything else must be the path
        // itself or a directory prefix of it.
        let covers = mountpoint == "/"
            || path == mountpoint
            || path
                .strip_prefix(mountpoint)
                .is_some_and(|rest| rest.starts_with('/'));
        if !covers {
            continue;
        }
        let Ok(id) = fields[0].parse::<u64>() else {
            continue;
        };
        // `>=` so an equally specific later mount - the top of a stack - wins.
        if best.is_none_or(|(len, _)| mountpoint.len() >= len) {
            best = Some((mountpoint.len(), id));
        }
    }
    best.map(|(_, id)| id)
}

/// The generic four lines: present, in Linux's order, and each one linked to
/// the door that reports the same fact.
fn generic_block(r: &mut Report) {
    let fd = unsafe {
        libc::open(
            c"/tmp/fdinfo-pin.txt".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    if fd < 0 {
        r.check("a scratch file opens", false);
        return;
    }
    // Give the file a length and seek somewhere only a decimal print reaches.
    let position = (1u64 << 32) + 17;
    unsafe {
        libc::ftruncate(fd, (position + 4096) as libc::off_t);
        libc::lseek(fd, position as libc::off_t, libc::SEEK_SET);
    }

    let pid = std::process::id();
    let info = match Fdinfo::of(pid, fd) {
        Some(info) => info,
        None => {
            r.check(&format!("fdinfo/<fd> of {pid} is readable"), false);
            unsafe { libc::close(fd) };
            return;
        }
    };

    r.check("fdinfo is readable for the caller's own descriptor", true);
    r.check("the four generic lines come first, in Linux's order", {
        let mut seen = 0;
        for line in info.text.lines() {
            if line.is_empty() {
                break;
            }
            let key = KEYS.iter().find(|key| line.starts_with(**key));
            match key {
                Some(key) if key == &KEYS[seen] => seen += 1,
                Some(_) => {}
                None => break,
            }
        }
        seen == KEYS.len()
    });
    r.check(
        &format!(
            "pos: is the position lseek reports ({}, got {})",
            position, info.pos
        ),
        info.pos == position,
    );

    // The flags relation, first with no cloexec: the line is exactly F_GETFL.
    let getfl = unsafe { libc::fcntl(fd, libc::F_GETFL) } as u32;
    r.check(
        &format!(
            "flags: is F_GETFL when FD_CLOEXEC is clear (F_GETFL 0{:o}, fdinfo 0{:o})",
            getfl, info.flags
        ),
        info.flags == getfl,
    );

    // Now set FD_CLOEXEC and require the bit to appear - the half F_GETFL does
    // not report, and the one a kernel printing F_GETFL alone would lose.
    unsafe { libc::fcntl(fd, libc::F_SETFD, FD_CLOEXEC) };
    let with_cloexec = Fdinfo::of(pid, fd).map(|info| info.flags).unwrap_or(0);
    let getfl_after = unsafe { libc::fcntl(fd, libc::F_GETFL) } as u32;
    r.check(
        &format!(
            "flags: carries O_CLOEXEC from FD_CLOEXEC (F_GETFL 0{:o}, fdinfo 0{:o})",
            getfl_after, with_cloexec
        ),
        with_cloexec == getfl_after | O_CLOEXEC as u32,
    );
    r.check(
        "F_GETFL itself does not report O_CLOEXEC",
        getfl_after & O_CLOEXEC as u32 == 0,
    );
    // And clearing it takes the bit back out, so the line follows the flag's
    // current value rather than the open's record of it.
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    let cleared = Fdinfo::of(pid, fd).map(|info| info.flags).unwrap_or(!0);
    r.check(
        "clearing FD_CLOEXEC takes the bit back out",
        cleared == getfl_after,
    );

    // `ino:` against `fstat`.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    let fstat_ok = unsafe { libc::fstat(fd, &mut st) } == 0;
    r.check(
        &format!("ino: is fstat's st_ino ({} vs {})", st.st_ino, info.ino),
        fstat_ok && info.ino == st.st_ino,
    );

    // `mnt_id:` against mountinfo - the correlation the number exists for.
    match mountinfo_id_of("/tmp/fdinfo-pin.txt") {
        Some(id) => r.check(
            &format!(
                "mnt_id: is the mountinfo id of the file's own mount ({id}, got {})",
                info.mnt_id
            ),
            info.mnt_id == id,
        ),
        None => r.check("mountinfo names the mount the scratch file is on", false),
    }

    unsafe { libc::close(fd) };

    // A descriptor that does not exist names no file here, and neither does a
    // name that is not the descriptor's decimal spelling. Measured on Linux
    // 7.2.14 (`build/scratch/fdinfo/edge.c`): `abc`, `-1`, `00`, ` 3`, `3x`,
    // `0x10` and `4294967296` are all `ENOENT` from this directory - `00` in
    // particular parses as 0 and Linux still refuses it.
    let absent: Vec<&str> = vec!["999", "abc", "-1", "00", "3x", "0x10"];
    let mut all_refused = true;
    for name in absent {
        if std::fs::metadata(format!("/proc/{pid}/fdinfo/{name}")).is_ok() {
            all_refused = false;
            r.check(&format!("fdinfo/{name} is ENOENT"), false);
        }
    }
    r.check(
        "a name that is not the descriptor's decimal spelling is ENOENT",
        all_refused,
    );

    // The directory's own words, and the file's: `0555` and `0444` - unlike
    // `/proc/<pid>/fd`, which is `0500` with a symlink per entry.
    use std::os::unix::fs::MetadataExt;
    let dir_meta = std::fs::metadata(format!("/proc/{pid}/fdinfo")).ok();
    r.check(
        "the fdinfo directory is 0555",
        dir_meta.as_ref().map(|meta| meta.mode() & 0o777) == Some(0o555),
    );
    let file_meta = std::fs::metadata(format!("/proc/{pid}/fdinfo/0")).ok();
    r.check(
        "an fdinfo file is 0444",
        file_meta.as_ref().map(|meta| meta.mode() & 0o777) == Some(0o444),
    );
    r.check(
        "an fdinfo entry is a regular file, not a link",
        file_meta.map(|meta| meta.file_type().is_file()) == Some(true),
    );
}

/// A descriptor read through `/proc/<pid>/fdinfo` reports the **description's**
/// position, so a `dup` shares it and a second open does not.
fn position_is_the_descriptions(r: &mut Report) {
    let pid = std::process::id();
    let path = c"/tmp/fdinfo-pin-pos.txt";
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    if fd < 0 {
        r.check("the position fixture opens", false);
        return;
    }
    unsafe {
        libc::write(fd, b"0123456789".as_ptr().cast(), 10);
        libc::lseek(fd, 4, libc::SEEK_SET);
    }
    let dup = unsafe { libc::dup(fd) };
    let kept = Fdinfo::of(pid, fd).map(|info| info.pos);
    let copied = Fdinfo::of(pid, dup).map(|info| info.pos);
    r.check(
        &format!("a dup reports the same position as its original ({kept:?} vs {copied:?})"),
        kept == copied && kept == Some(4),
    );
    // Reading moves it, and the duplicate sees the move - one description.
    let mut byte = 0u8;
    unsafe { libc::read(fd, (&raw mut byte).cast(), 1) };
    let after = Fdinfo::of(pid, dup).map(|info| info.pos);
    r.check(
        &format!("a read moves the position both names report (got {after:?})"),
        after == Some(5),
    );
    // A second open of the same file is a different description: its own
    // position, starting at zero.
    let other = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    let fresh = Fdinfo::of(pid, other).map(|info| info.pos);
    r.check(
        &format!("a second open has its own position (got {fresh:?})"),
        fresh == Some(0),
    );
    unsafe {
        libc::close(other);
        libc::close(dup);
        libc::close(fd);
    }
}

/// The per-kind lines this kernel serves, each read through the key Linux uses.
///
/// Only the kinds whose state this kernel holds are claimed; a kernel that
/// reports the block must report the *right* values, and each claim is against
/// the operation that set the state rather than against a constant.
/// A `(sec, nsec)` pair as one nanosecond count - `it_value`'s field, which
/// both doors spell as a decimal `timespec` in parentheses.
fn parse_timespec(text: &str) -> Option<u64> {
    let inner = text.strip_prefix('(')?.strip_suffix(')')?;
    let (sec, nsec) = inner.split_once(',')?;
    Some(sec.trim().parse::<u64>().ok()? * 1_000_000_000 + nsec.trim().parse::<u64>().ok()?)
}

fn per_kind(r: &mut Report) {
    let pid = std::process::id();

    // eventfd: the counter is the value that was written, in hex.
    let efd = unsafe { libc::syscall(libc::SYS_eventfd2, 0u32, 0) } as i32;
    if efd >= 0 {
        let value: u64 = 255;
        unsafe {
            let ptr = &value as *const u64;
            libc::write(efd, ptr.cast(), 8);
        }
        if let Some(info) = Fdinfo::of(pid, efd) {
            if let Some(line) = info.line("eventfd-count:") {
                let field = line.trim_start_matches("eventfd-count:").trim();
                // Linux prints this one in hex (`ff` for 255) - measured
                // `build/scratch/fdinfo/radix2.c`. A kernel printing decimal
                // would answer `255` and fail here.
                r.check(
                    &format!("eventfd-count is the counter in hex (got {field})"),
                    u64::from_str_radix(field, 16).is_ok_and(|n| n == value),
                );
                r.check(
                    "eventfd-semaphore is 0 for a non-semaphore eventfd",
                    info.line("eventfd-semaphore:").and_then(|line| {
                        line.trim_start_matches("eventfd-semaphore:")
                            .trim()
                            .parse::<u64>()
                            .ok()
                    }) == Some(0),
                );
            } else {
                r.check("an eventfd's fdinfo reports eventfd-count", false);
            }
        }
        unsafe { libc::close(efd) };
    } else {
        r.check("eventfd2 is served", false);
    }

    // signalfd: `sigmask` is the wire sigset - sixteen hex digits, signal *n* in
    // bit *n-1*. The two signals below put bits 9 and 14 up, so the line is
    // `0000000000004200`; a kernel rendering the kernel's own numbering, or in
    // decimal, would fail.
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigaddset(&mut set, libc::SIGTERM);
    }
    let sfd = unsafe { libc::signalfd(-1, &set, 0) };
    if sfd >= 0 {
        if let Some(info) = Fdinfo::of(pid, sfd) {
            r.check(
                &format!(
                    "a signalfd's sigmask is the wire sigset (got {:?})",
                    info.line("sigmask:")
                ),
                info.line("sigmask:") == Some("sigmask:\t0000000000004200"),
            );
        }
        unsafe { libc::close(sfd) };
    } else {
        r.check("signalfd is served", false);
    }

    // timerfd: `it_value` is what `timerfd_gettime` reports *now* - the time
    // remaining, not the stored deadline - so the claim is against that sibling
    // door and not against a constant. `settime flags` echoes the word the arm
    // was asked with.
    let tfd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, 0) };
    if tfd >= 0 {
        let mut arm: libc::itimerspec = unsafe { core::mem::zeroed() };
        arm.it_value.tv_sec = 100;
        arm.it_interval.tv_sec = 5;
        arm.it_interval.tv_nsec = 500_000_000;
        let armed = unsafe { libc::timerfd_settime(tfd, 0, &arm, core::ptr::null_mut()) } == 0;
        // The two doors are read at two instants of a running clock, so their
        // answers are *not* equal and must not be pinned equal: an arm set for
        // 100 s has less than that left by the time anything reads it, and the
        // two reads here are microseconds apart. The relation that does hold is
        // monotonicity - the fdinfo line, read first, reports no less than the
        // `timerfd_gettime` that follows it - so that is the claim, with the
        // span's own bound: both must still be inside the arm's 100 seconds.
        let info_first = Fdinfo::of(pid, tfd);
        let mut got: libc::itimerspec = unsafe { core::mem::zeroed() };
        let read_back = armed && unsafe { libc::timerfd_gettime(tfd, &mut got) } == 0;
        if let Some(info) = info_first {
            let field = info
                .line("it_value:")
                .and_then(|line| parse_timespec(line.trim_start_matches("it_value:").trim()));
            let later = (got.it_value.tv_sec as u64) * 1_000_000_000 + got.it_value.tv_nsec as u64;
            r.check(
                &format!(
                    "a timerfd's it_value is the remaining span timerfd_gettime reports (fdinfo {field:?} vs later read {later} ns)"
                ),
                read_back && field.is_some_and(|first| first >= later && first <= 100 * 1_000_000_000),
            );
            r.check(
                "a timerfd's settime flags is the word it was armed with (00 for a relative arm)",
                info.line("settime flags:") == Some("settime flags: 00"),
            );
            r.check(
                "a timerfd's clockid is the clock it was made on (1 = CLOCK_MONOTONIC)",
                info.line("clockid:") == Some("clockid: 1"),
            );
        }
        unsafe { libc::close(tfd) };
    } else {
        r.check("timerfd_create is served", false);
    }

    // A pipe end reports pipefs's pseudo id and a socket sockfs's - the two
    // numbers measured on Linux (`build/scratch/fdinfo/rel.c`), the ones a
    // reader comparing against mountinfo has to find nowhere.
    let mut pp = [0i32; 2];
    if unsafe { libc::pipe(pp.as_mut_ptr()) } == 0 {
        if let Some(info) = Fdinfo::of(pid, pp[0]) {
            r.check(
                "a pipe end's mnt_id is pipefs's pseudo id (18)",
                info.mnt_id == 18,
            );
        }
        unsafe {
            libc::close(pp[0]);
            libc::close(pp[1]);
        }
    }
    let mut pair = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } == 0 {
        if let Some(info) = Fdinfo::of(pid, pair[0]) {
            r.check(
                "a socket's mnt_id is sockfs's pseudo id (12)",
                info.mnt_id == 12,
            );
        }
        unsafe {
            libc::close(pair[0]);
            libc::close(pair[1]);
        }
    }
}

fn main() {
    let mut r = Report::new();
    generic_block(&mut r);
    position_is_the_descriptions(&mut r);
    per_kind(&mut r);
    r.finish();
}
