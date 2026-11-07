//! `/proc/<pid>/stat`'s `rsslim` (field 25) - the task's `RLIMIT_RSS` soft
//! limit, printed as the bare `u64` word Linux prints.
//!
//! Measured on a current host (`build/scratch/rsslim/q.c`, Linux 7.2.14):
//! an ordinary process reads `18446744073709551615` (the full `u64::MAX`,
//! *not* `-1`), lowering the soft limit to 4 MiB reads `4194304`, and restoring
//! it reads the infinity word again. A first probe that parsed the token with
//! `atol` answered `-1` for the infinity case - the overflow was the probe's,
//! not Linux's, so this pin parses field 25 as `u64` and never narrows it.
//!
//! The field is a promise about *which* of the sixteen limits is being read,
//! and about *whose* row: it is the task being read, not a global default and
//! not the reader's own row. A literal `0` - which this kernel printed until
//! `docs/notes/` §6.111 - fails every movement below, and a fix that read the
//! startup default instead of the task's stored table would fail the
//! `setrlimit` movement; the child claim is the one that separates "the task's
//! row" from "the reading task's row", which `/proc/self/stat` cannot tell
//! apart because there the two are the same task.

use mpos_user_tests::Report;

/// The field the reader asks for, counted the way `/proc/<pid>/stat` counts:
/// the command name may hold spaces and parentheses, so the fields start after
/// the last `)` and field `N` is the `N - 3`rd token after it. Unlike the
/// `starttime` pin this is parsed as `u64`: `rsslim`'s infinity word does not
/// fit in an `i64`.
fn stat_field(text: &str, field: usize) -> Option<u64> {
    let after = &text[text.rfind(')')? + 1..];
    let mut tokens = after.split_whitespace();
    // The token right after `)` is field 3 (`state`).
    let mut index = 3;
    loop {
        let token = tokens.next()?;
        if index == field {
            return token.parse().ok();
        }
        index += 1;
    }
}

fn rsslim_of(path: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    stat_field(&text, 25)
}

/// The standing `RLIMIT_RSS` pair as `(soft, hard)`.
fn rss_limit() -> Option<(u64, u64)> {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `rl` is a record this process owns.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_RSS, &mut rl) };
    (rc == 0).then_some((rl.rlim_cur as u64, rl.rlim_max as u64))
}

/// Set the soft limit, leaving the hard one where it is.
fn set_rss_soft(cur: u64) -> bool {
    let Some((_, max)) = rss_limit() else {
        return false;
    };
    let rl = libc::rlimit {
        rlim_cur: cur as libc::rlim_t,
        rlim_max: max as libc::rlim_t,
    };
    // SAFETY: `rl` is this process's own word, and `cur <= max`.
    unsafe { libc::setrlimit(libc::RLIMIT_RSS, &rl) == 0 }
}

const LOW: u64 = 4 * 1024 * 1024;
const CHILD_LOW: u64 = 2 * 1024 * 1024;
const INFINITY: u64 = u64::MAX;

/// Fork a child that lowers *its own* ceiling, signals readiness on a pipe,
/// then stays alive long enough to be read. Returns the child's pid and the
/// read end of the pipe. The child's `getpid()` is written as a `u32` so the
/// parent reads the number the child itself would name, even if a pid
/// namespace were ever in play.
fn spawn_lowered_child() -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is two ints this process owns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `fork` in a single-threaded test process; the child touches only
    // async-signal-safe calls before `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // SAFETY: plain `setrlimit`, a write of one word, a sleep, then `_exit`
        // - no destructor, no allocator.
        unsafe {
            libc::close(fds[0]);
            if set_rss_soft(CHILD_LOW) {
                let me = libc::getpid() as u32;
                let bytes = me.to_ne_bytes();
                libc::write(fds[1], bytes.as_ptr().cast(), bytes.len());
            }
            let spec = libc::timespec {
                tv_sec: 5,
                tv_nsec: 0,
            };
            libc::nanosleep(&spec, std::ptr::null_mut());
            libc::_exit(0);
        }
    }
    // SAFETY: the parent closes only the write end it no longer needs.
    unsafe { libc::close(fds[1]) };
    Some((pid, fds[0]))
}

fn main() {
    let mut r = Report::new();

    let own_base = rsslim_of("/proc/self/stat");
    let base_limit = rss_limit().map(|(cur, _)| cur);
    r.check("stat prints field 25 as a u64 word", own_base.is_some());
    r.check(
        &format!(
            "field 25 is this task's RLIMIT_RSS soft limit ({own_base:?} against {base_limit:?})"
        ),
        own_base.is_some() && own_base == base_limit && own_base == Some(INFINITY),
    );

    // Lower the soft limit. Both the field and getrlimit must move together:
    // this is the claim a fix that read the startup default instead of the
    // task's stored table fails.
    let lowered_ok = set_rss_soft(LOW);
    let own_low = rsslim_of("/proc/self/stat");
    let low_limit = rss_limit().map(|(cur, _)| cur);
    r.check(
        &format!("lowering the soft limit to 4 MiB moves field 25 with it ({own_low:?} against {low_limit:?})"),
        lowered_ok && own_low == Some(LOW) && low_limit == Some(LOW),
    );

    // A child's own ceiling belongs in the child's file, and the parent's file
    // keeps the parent's. `/proc/self/stat` cannot tell these apart; two tasks
    // with two different rows can.
    let child = spawn_lowered_child();
    let (child_field, parent_field, child_pid) = if let Some((pid, read_fd)) = child {
        let mut buf = [0u8; 4];
        // SAFETY: one word from the child's readiness pipe, then a bounded
        // read of its stat while it is still alive.
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
        let me = if n == 4 {
            Some(u32::from_ne_bytes(buf) as i32)
        } else {
            None
        };
        unsafe { libc::close(read_fd) };
        let cf = me.and_then(|p| rsslim_of(&format!("/proc/{p}/stat")));
        let pf = rsslim_of("/proc/self/stat");
        // SAFETY: reap the sleeping child so nothing is left behind.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
        (cf, pf, me)
    } else {
        (None, None, None)
    };
    r.check(
        &format!(
            "a child's own ceiling is the field in its file ({child_field:?}, child {child_pid:?})"
        ),
        child_field == Some(CHILD_LOW),
    );
    r.check(
        &format!("and the parent's file keeps the parent's ceiling ({parent_field:?} against {child_field:?})"),
        parent_field == Some(LOW) && parent_field != child_field,
    );

    // Restore: raising the soft limit back to the hard one (infinity) is any
    // process's to do, on Linux and here.
    let restored_ok = set_rss_soft(INFINITY);
    let own_restored = rsslim_of("/proc/self/stat");
    r.check(
        &format!(
            "restoring the ceiling moves field 25 back to the infinity word ({own_restored:?})"
        ),
        restored_ok && own_restored == Some(INFINITY),
    );

    r.finish();
}
