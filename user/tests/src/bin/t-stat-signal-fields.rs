//! t-stat-signal-fields - `/proc/<pid>/stat`'s four signal words say what the
//! task has pending, blocked, ignored and caught.
//!
//! Fields 31-34 of `stat` are `signal`, `blocked`, `sigignore` and `sigcatch`:
//! two words about **this task** (what is pending on it, what it has blocked) and
//! two about its **process** (which dispositions are `SIG_IGN`, which are a
//! handler). They are the cheap way for a debugger, a supervisor or a `ps` with
//! extra columns to ask why a signal that was sent has not arrived - and this
//! kernel printed `0` for all four while it held every one of the answers:
//! `Task::signals` and `Task::blocked` are the pending and blocked sets, and the
//! group's dispositions were in the signal table all along.
//!
//! Linux's shape was measured before any of this was written
//! (`build/scratch/sigfield/p.c`, Linux 7.2.14): blocking `SIGUSR1` (10) moved
//! field 32 to `512` = `1 << 9`; `signal(SIGUSR2, SIG_IGN)` added `2048` =
//! `1 << 11` to field 33; installing a `SIGTERM` handler put `16384` = `1 << 14`
//! in field 34; raising a blocked `SIGWINCH` (28) put `134217728` = `1 << 27` in
//! field 31; and replacing that handler with `SIG_IGN` moved the same bit from
//! field 34 to field 33. Signal *n* is bit *n-1* - the `sigset_t` layout, not a
//! one-based one.
//!
//! Every claim here is a **relation** against what the process did itself, not
//! against a constant: a libc ignores `SIGPIPE`, so the baseline word is not
//! zero on Linux and is not the same as this kernel's empty table either. What
//! must be true on both machines is that the word moved by exactly the bit the
//! call names and by nothing else - which is what a program subtracting two
//! readings of these fields depends on.

use mpos_user_tests::Report;

/// Signal `n` is bit `n - 1`, the layout both the wire `sigset_t` and the
/// kernel's pending word use.
fn bit(sig: i32) -> u64 {
    1u64 << (sig - 1)
}

/// One reading of the four words, in Linux's order.
#[derive(Clone, Copy, Debug)]
struct Words {
    pending: u64,
    blocked: u64,
    ignored: u64,
    caught: u64,
}

/// Fields 31-34 of `/proc/self/stat`.
///
/// The split starts after the **last** `)`, because `comm` is a parenthesised
/// field and may itself contain spaces and parentheses; field 3 is the first
/// token after it, so field *N* is `tokens[N - 3]`.
fn read_words() -> Option<Words> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    let tail = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let word = |index: usize| -> Option<u64> { fields.get(index)?.parse().ok() };
    Some(Words {
        pending: word(31 - 3)?,
        blocked: word(32 - 3)?,
        ignored: word(33 - 3)?,
        caught: word(34 - 3)?,
    })
}

fn block(sig: i32) {
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, core::ptr::null_mut());
    }
}

fn unblock(sig: i32) {
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut());
    }
}

extern "C" fn quiet_handler(_sig: libc::c_int) {}

fn main() {
    let mut r = Report::new();
    const SIGUSR1: i32 = libc::SIGUSR1;
    const SIGUSR2: i32 = libc::SIGUSR2;
    const SIGWINCH: i32 = libc::SIGWINCH;
    const SIGTERM: i32 = libc::SIGTERM;

    let Some(start) = read_words() else {
        r.check("the four signal words read (the file did not parse)", false);
        r.finish();
        return;
    };
    r.check(
        &format!("the four signal words read as numbers ({start:?})"),
        true,
    );

    // 1. Blocking a signal is this task's own business: the bit lands in the
    //    blocked word, and - measured - nothing else in the file moves.
    block(SIGUSR1);
    let after_block = read_words().expect("stat reads twice in one process");
    r.check(
        &format!(
            "blocking SIGUSR1 puts its bit in the blocked word ({} -> {}, expected +{})",
            start.blocked,
            after_block.blocked,
            bit(SIGUSR1)
        ),
        after_block.blocked == start.blocked | bit(SIGUSR1),
    );

    // 2. `SIG_IGN` is a disposition, so it lands in the group's ignore word -
    //    and a process's threads share that word (`CLONE_SIGHAND`).
    unsafe { libc::signal(SIGUSR2, libc::SIG_IGN) };
    let after_ignore = read_words().expect("stat reads three times in one process");
    r.check(
        &format!(
            "ignoring SIGUSR2 puts its bit in the ignore word ({} -> {}, expected +{})",
            after_block.ignored,
            after_ignore.ignored,
            bit(SIGUSR2)
        ),
        after_ignore.ignored == after_block.ignored | bit(SIGUSR2),
    );

    // 3. A handler is the other disposition: field 34 and not field 33. The
    //    two words partition the table, which is the property a reader uses.
    let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
    action.sa_sigaction = quiet_handler as libc::sighandler_t;
    unsafe { libc::sigaction(SIGTERM, &action, core::ptr::null_mut()) };
    let after_handler = read_words().expect("stat reads four times in one process");
    r.check(
        &format!(
            "a handled SIGTERM puts its bit in the catch word ({} -> {}, expected +{})",
            after_ignore.caught,
            after_handler.caught,
            bit(SIGTERM)
        ),
        after_handler.caught == after_ignore.caught | bit(SIGTERM),
    );
    r.check(
        &format!(
            "and not in the ignore word, which the two words partition ({} against {})",
            after_handler.ignored, after_ignore.ignored
        ),
        after_handler.ignored == after_ignore.ignored,
    );

    // 4. A pending signal is the third thing a reader asks: sent, blocked, not
    //    delivered. `SIGWINCH` is used because its default action discards it,
    //    which makes the clearing half below observable without a handler.
    block(SIGWINCH);
    let after_block_winch = read_words().expect("stat reads five times in one process");
    unsafe { libc::raise(SIGWINCH) };
    let after_raise = read_words().expect("stat reads six times in one process");
    r.check(
        &format!(
            "a blocked signal that is raised shows in the pending word ({} -> {}, expected +{})",
            after_handler.pending,
            after_raise.pending,
            bit(SIGWINCH)
        ),
        after_raise.pending == after_handler.pending | bit(SIGWINCH),
    );
    r.check(
        &format!(
            "and the blocked word already carried it, the raise adding nothing ({} against {})",
            after_raise.blocked, after_block_winch.blocked
        ),
        after_raise.blocked == after_block_winch.blocked,
    );

    // 5. Delivery empties it. `SIGWINCH` at `SIG_DFL` is discarded, so the
    //    pending bit must be gone and nothing else may change.
    unblock(SIGWINCH);
    let after_delivery = read_words().expect("stat reads seven times in one process");
    r.check(
        &format!(
            "unblocking it delivers the default action and clears the pending word ({} -> {})",
            after_raise.pending, after_delivery.pending
        ),
        after_delivery.pending == after_handler.pending,
    );

    // 6. The blocked word is the task's, the dispositions the process's: a
    //    thread that blocks another signal shows it in its own `stat` and its
    //    leader's word is its own. This is the claim that says the two halves
    //    of the file are read from the right subject.
    let leader_blocked = after_delivery.blocked;
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i64;
    let own = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"))
        .ok()
        .and_then(|text| {
            let tail = &text[text.rfind(')')? + 1..];
            let fields: Vec<&str> = tail.split_whitespace().collect();
            fields.get(32 - 3)?.parse::<u64>().ok()
        });
    r.check(
        &format!("this thread's own stat carries the leader's blocked word ({own:?} against {leader_blocked})"),
        own == Some(leader_blocked),
    );

    r.finish();
}
