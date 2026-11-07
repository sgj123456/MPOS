//! t-status-counts - `/proc/<pid>/status`'s `FDSize` and `SigQ` are counts.
//!
//! Both lines were literals: `FDSize:\t64` and `SigQ:\t0/64`. One of them is a
//! promise about what the descriptor table has *ever* grown to, the other is two
//! independent numbers - the user's queued real-time copies and this task's own
//! `RLIMIT_SIGPENDING` soft limit. A constant answers neither honestly.
//!
//! # What Linux answers (measured before this was written)
//!
//! `build/scratch/fdq/formula.c` holds `/proc/self/status` open on fd 3 and
//! `rewind`s it for each reading, so the probe's own descriptor does not perturb
//! the table it is measuring. `dup(2)` up to each boundary:
//!
//! ```text
//! start                       FDSize=64
//! high fd=63  (slots=64)      FDSize=128
//! high fd=127 (slots=128)     FDSize=256
//! high fd=255 (slots=256)     FDSize=512
//! high fd=511 (slots=512)     FDSize=1024
//! ```
//!
//! `boundary2.c` catches the exact step: `dup` returning 64 (65 slots) moves
//! `64 -> 128`, 128 (129) moves `128 -> 256`, 256 (257) moves `256 -> 512`.
//! `boundary.c` closes all 65 again and the line still reads 128 - the table
//! never shrinks. `hole.c` `dup2`s a single descriptor to 200 and reads 256,
//! then to 1000 and reads 1024, so it is the *table's* high water, not the
//! highest descriptor currently in use. The rule is
//! `slot_count().next_power_of_two().max(64)`, the same one `select_clamp`
//! already uses in `src/vfs/file.rs`.
//!
//! `build/scratch/fdq/sigq.c` and `sigq3.c` answer the other line. With five
//! real-time copies queued, `setrlimit(RLIMIT_SIGPENDING, {3, ...})` turns
//! `SigQ: 6/125239` into `SigQ: 6/3`: the denominator is **this task's** soft
//! limit (it follows `setrlimit`), and the numerator is allowed to be larger
//! than it. `sigq3.c` forks a child that queues two more; the parent - which
//! queued nothing - then reads `8/125239` for itself and `8/9` for the live
//! child, so the numerator is a **per-user** total shared across that uid's
//! tasks while the denominator stays with the task the file describes.
//!
//! Linux's numerator has a baseline of 1 (the kernel counts one record for the
//! process itself) where this kernel's queue starts empty. This pin therefore
//! asserts *movement*, not an absolute baseline, and reads the standing limit
//! with `getrlimit` rather than assuming 16 (this kernel's default) or 125239
//! (the host's).
//!
//! # The counterfactual
//!
//! With the three `src/` files reverted, `FDSize` stays 64 through every
//! boundary, and `SigQ` stays `0/64` no matter how many copies are queued or
//! where the soft limit moves. The growth, step, step-after-close, and both
//! `SigQ` books all go red; only the shape claim (the two lines exist) stays
//! green.

use mpos_user_tests::Report;

/// One real-time number musl lets a caller queue (`t-rt-signal-queue` uses the
/// same range: 32/33/34 are the implementation's own).
const SIG: i32 = 35;

/// How many copies to queue. Small enough for this kernel's default
/// `RLIMIT_SIGPENDING` soft limit of 16, and large enough that lowering the
/// soft limit to 3 leaves the numerator above it.
const COPIES: u64 = 5;

/// The soft limit this test lowers to, leaving the hard one alone. A claim that
/// had to raise a hard limit back would be root's alone and could not run on
/// the host.
const LOW: u64 = 3;

/// Read the two lines from a held descriptor.
///
/// Holding this file open for the whole program is what makes the `FDSize`
/// claims honest: a `read_to_string("/proc/self/status")` of its own opens a
/// descriptor, and at a 64-slot boundary that descriptor is exactly the one
/// that would move the answer. A seek back to zero re-renders the file, so the
/// same descriptor answers every stage. (`formula.c` uses `rewind` for the
/// same reason.)
fn status_text(file: &mut std::fs::File) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// A `Name:\tvalue` line's number, parsed only for the name asked about - the
/// rest of `status` is words, and parsing all of it makes the whole read fail.
fn number(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(name)?
            .trim_start_matches(':')
            .trim()
            .parse::<u64>()
            .ok()
    })
}

/// `SigQ:\tqueued/limit`, the two books kept apart.
fn sigq(text: &str) -> Option<(u64, u64)> {
    let word = text
        .lines()
        .find_map(|line| line.strip_prefix("SigQ:"))?
        .trim();
    let (a, b) = word.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn fd_size(text: &str) -> Option<u64> {
    number(text, "FDSize:")
}

/// Block the one number this program queues, so every copy stays in the
/// pending record instead of being delivered (its default action is to end
/// the process).
fn block(sig: i32) -> bool {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is this process's own word and `sig` is a valid number.
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == 0
    }
}

/// Queue one copy to this process, carrying `value` as its `si_value`.
fn queue(sig: i32, value: u64) -> bool {
    let val = libc::sigval {
        sival_ptr: value as usize as *mut libc::c_void,
    };
    // SAFETY: signalling this process, whose signal is blocked for the whole
    // program, so nothing is delivered out from under the reading.
    unsafe { libc::sigqueue(libc::getpid(), sig, val) == 0 }
}

/// The standing `RLIMIT_SIGPENDING` pair as `(soft, hard)`.
fn sigpending_limit() -> Option<(u64, u64)> {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `rl` is a record this process owns.
    if unsafe { libc::getrlimit(libc::RLIMIT_SIGPENDING, &mut rl) } != 0 {
        return None;
    }
    Some((rl.rlim_cur as u64, rl.rlim_max as u64))
}

/// Lower only the soft limit, leaving the hard one where it is.
fn set_sigpending_soft(cur: u64) -> bool {
    let Some((_, max)) = sigpending_limit() else {
        return false;
    };
    let rl = libc::rlimit {
        rlim_cur: cur as libc::rlim_t,
        rlim_max: max as libc::rlim_t,
    };
    // SAFETY: `rl` is this process's own word, and `cur <= max`.
    unsafe { libc::setrlimit(libc::RLIMIT_SIGPENDING, &rl) == 0 }
}

/// `dup(0)` until the last descriptor handed out is at least `min_fd`, holding
/// every one of them so the table's slot count really reaches that high water.
/// Returns the descriptors for the caller to close later.
fn grow_to(min_fd: i32, held: &mut Vec<i32>) -> bool {
    loop {
        // SAFETY: fd 0 is open for the whole program; `dup` hands back the
        // lowest free number, which is why the sequence is exactly the slot
        // count the table grows through.
        let fd = unsafe { libc::dup(0) };
        if fd < 0 {
            return false;
        }
        held.push(fd);
        if fd >= min_fd {
            return true;
        }
    }
}

fn main() {
    println!("t-status-counts");
    let mut r = Report::new();

    // One descriptor, held for every reading below.
    let Ok(mut status) = std::fs::File::open("/proc/self/status") else {
        r.check("status opens and prints the two count lines", false);
        r.finish()
    };
    let first = status_text(&mut status);
    r.check(
        "status prints FDSize and SigQ as two counts",
        first.as_deref().and_then(fd_size).is_some() && first.as_deref().and_then(sigq).is_some(),
    );
    let Some(first) = first else { r.finish() };
    let Some(base_fd) = fd_size(&first) else {
        r.finish()
    };

    // --- FDSize: the table's high water, in 64-slot steps -------------------
    let mut held = Vec::new();
    let grew_64 = grow_to(64, &mut held);
    let at_64 = status_text(&mut status).and_then(|t| fd_size(&t));
    r.check(
        &format!(
            "growing the descriptor table past 64 slots moves FDSize ({} -> {:?})",
            base_fd, at_64
        ),
        grew_64 && at_64 == Some(128),
    );

    let grew_128 = grow_to(128, &mut held);
    let at_128 = status_text(&mut status).and_then(|t| fd_size(&t));
    r.check(
        &format!("and the next 64-slot step moves it again ({:?})", at_128),
        grew_128 && at_128 == Some(256),
    );

    let grew_256 = grow_to(256, &mut held);
    let at_256 = status_text(&mut status).and_then(|t| fd_size(&t));
    r.check(
        &format!("and one more step is the same rule ({:?})", at_256),
        grew_256 && at_256 == Some(512),
    );

    // Closing every duplicate does not shrink the line: `slot_count` is the
    // high water, not the number of descriptors still open.
    for fd in held.drain(..) {
        // SAFETY: each is a descriptor this program made with `dup(0)`.
        unsafe { libc::close(fd) };
    }
    let after_close = status_text(&mut status).and_then(|t| fd_size(&t));
    r.check(
        &format!("closing them all never shrinks FDSize ({:?})", after_close),
        after_close == at_256 && after_close == Some(512),
    );

    // --- SigQ: two books, one per-user and one per-task ---------------------
    let limit = sigpending_limit();
    let base_q = sigq(&first);
    r.check(
        &format!(
            "SigQ's denominator is this task's RLIMIT_SIGPENDING soft limit ({:?} against {:?})",
            base_q.map(|(_, d)| d),
            limit.map(|(soft, _)| soft)
        ),
        base_q.is_some()
            && limit.is_some()
            && base_q.map(|(_, d)| d) == limit.map(|(soft, _)| soft),
    );

    let blocked = block(SIG);
    let mut queued = true;
    for value in 0..COPIES {
        queued &= queue(SIG, 0x1000 + value);
    }
    let after_q = status_text(&mut status).and_then(|t| sigq(&t));
    let base_n = base_q.map(|(n, _)| n);
    let grew_n = after_q.map(|(n, _)| n);
    r.check(
        &format!(
            "five queued real-time copies move SigQ's numerator by five ({:?} -> {:?})",
            base_n, grew_n
        ),
        blocked && queued && matches!((base_n, grew_n), (Some(a), Some(b)) if b == a + COPIES),
    );

    let lowered = set_sigpending_soft(LOW);
    let after_low = status_text(&mut status).and_then(|t| sigq(&t));
    r.check(
        &format!(
            "lowering the soft limit moves the denominator with it ({:?} -> {:?})",
            base_q.map(|(_, d)| d),
            after_low.map(|(_, d)| d)
        ),
        lowered && after_low.map(|(_, d)| d) == Some(LOW),
    );
    r.check(
        &format!(
            "and the numerator may be larger than that limit ({:?})",
            after_low
        ),
        matches!(after_low, Some((n, d)) if n > d && d == LOW),
    );

    r.finish()
}
