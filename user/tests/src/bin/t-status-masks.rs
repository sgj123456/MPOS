//! `/proc/<pid>/status` prints the signal masks and the switch counters.
//!
//! The long form of the process table is what `top`, `ps` and a shell's job
//! control read. Five of its lines were literals in this kernel while the facts
//! behind them were already in the task table: `SigPnd`/`SigBlk` (this thread's
//! pending and blocked sets), `SigIgn`/`SigCgt` (the group's dispositions, split
//! into ignored and handled), `ShdPnd` (the group's pending set), and
//! `voluntary_ctxt_switches`/`nonvoluntary_ctxt_switches`.
//!
//! Measured on Linux 7.2.14 (`build/scratch/status/s.c`, `t.c`, `u.c`, musl
//! static):
//!
//! ```text
//! after blocking SIGUSR1 (10)        SigPnd=0 SigBlk=0000000000000200 SigIgn=0000000001001000 SigCgt=0
//! after SIG_IGN SIGUSR2 (12)         SigIgn=0000000001001800
//! after a SIGTERM (15) handler       SigCgt=0000000000004000
//! after raising a blocked SIGUSR1    SigPnd=0000000000000200
//! blocked USR1+INT+ILL               SigBlk=000000000000020a   (lowercase hex, 16 digits)
//! 50 sleeps                          voluntary_ctxt_switches: 17 -> 67
//! ```
//!
//! and the switch lines are the **thread's** own (`u.c`): a worker that slept
//! fifty times read `voluntary_ctxt_switches: 50` in its own
//! `/proc/<pid>/task/<tid>/status` while the main thread read `16`, even though
//! `getrusage(RUSAGE_SELF)` answered `106` for the same process - the group's
//! number, which is a different door (§6.105).
//!
//! Every claim below is a *movement* or an agreement between the two doors
//! (`status` and `stat`), never a constant: the host's own baseline is not zero
//! (`SigIgn` carries `SIGPIPE` and more), and this kernel's baseline is not zero
//! either.

use mpos_user_tests::{Report, host_run};
use std::time::Duration;

/// One signal's bit in a mask: signal `n` is `1 << (n - 1)`.
fn bit(signal: i32) -> u64 {
    1u64 << (signal - 1)
}

/// The status lines this test reads.
#[derive(Clone, Copy)]
struct Lines {
    sig_pnd: u64,
    shd_pnd: u64,
    sig_blk: u64,
    sig_ign: u64,
    sig_cgt: u64,
    voluntary: u64,
    nonvoluntary: u64,
}

/// Read one `name:\tvalue` line out of a status file, parsed as a decimal
/// number. `None` when the line is missing or the value is not a number.
fn number(text: &str, name: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(name) {
            return rest.trim_start_matches(':').trim().parse().ok();
        }
    }
    None
}

/// Read one mask line, parsed as sixteen hex digits. The width is the point:
/// a reader's `%016lx` comparison fails on a short word.
fn mask(text: &str, name: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(name) {
            let word = rest.trim_start_matches(':').trim();
            return u64::from_str_radix(word, 16).ok();
        }
    }
    None
}

fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn status_of(path: &str) -> Option<Lines> {
    let text = read_file(path)?;
    Some(Lines {
        sig_pnd: mask(&text, "SigPnd:")?,
        shd_pnd: mask(&text, "ShdPnd:")?,
        sig_blk: mask(&text, "SigBlk:")?,
        sig_ign: mask(&text, "SigIgn:")?,
        sig_cgt: mask(&text, "SigCgt:")?,
        voluntary: number(&text, "voluntary_ctxt_switches:")?,
        nonvoluntary: number(&text, "nonvoluntary_ctxt_switches:")?,
    })
}

fn self_status() -> Option<Lines> {
    status_of("/proc/self/status")
}

/// The `blocked` word `/proc/self/stat` reports (field 32), so the two doors can
/// be compared: `field N = tokens[N - 3]` after the last `)`.
fn stat_blocked() -> Option<u64> {
    let text = read_file("/proc/self/stat")?;
    let tail = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = tail.split_whitespace().collect();
    fields.get(32 - 3)?.parse().ok()
}

fn block(signal: i32) {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, signal);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

extern "C" fn quiet_handler(_signal: libc::c_int) {}

fn sleep_a_millisecond() {
    std::thread::sleep(Duration::from_millis(1));
}

fn main() {
    let mut r = Report::new();

    let start = self_status();
    r.check(
        &format!(
            "status prints the mask and switch lines ({})",
            start.is_some()
        ),
        start.is_some(),
    );
    let start = start.unwrap_or(Lines {
        sig_pnd: 0,
        shd_pnd: 0,
        sig_blk: 0,
        sig_ign: 0,
        sig_cgt: 0,
        voluntary: 0,
        nonvoluntary: 0,
    });

    // The blocked word: the same fact `stat` carries as its field 32, so the two
    // doors are compared rather than trusted one at a time.
    block(libc::SIGUSR1);
    let blocked = self_status().unwrap_or(Lines { ..start });
    r.check(
        &format!(
            "blocking SIGUSR1 puts its bit in SigBlk ({:016x} -> {:016x})",
            start.sig_blk, blocked.sig_blk
        ),
        blocked.sig_blk & bit(libc::SIGUSR1) != 0
            && blocked.sig_blk & bit(libc::SIGUSR1) != start.sig_blk & bit(libc::SIGUSR1),
    );
    r.check(
        &format!(
            "and stat's blocked word says the same ({:?})",
            stat_blocked()
        ),
        stat_blocked() == Some(blocked.sig_blk),
    );

    // The two disposition words: one is what a task ignores, the other what it
    // has a handler for, and a signal belongs to exactly one of them.
    unsafe { libc::signal(libc::SIGUSR2, libc::SIG_IGN) };
    unsafe { libc::signal(libc::SIGTERM, quiet_handler as libc::sighandler_t) };
    let disposed = self_status().unwrap_or(Lines { ..start });
    r.check(
        &format!(
            "ignoring SIGUSR2 puts its bit in SigIgn ({:016x} -> {:016x})",
            blocked.sig_ign, disposed.sig_ign
        ),
        disposed.sig_ign & bit(libc::SIGUSR2) != 0,
    );
    r.check(
        &format!(
            "a handled SIGTERM puts its bit in SigCgt ({:016x} -> {:016x})",
            blocked.sig_cgt, disposed.sig_cgt
        ),
        disposed.sig_cgt & bit(libc::SIGTERM) != 0,
    );
    r.check(
        &format!(
            "and the two words never share a bit ({:016x} & {:016x})",
            disposed.sig_ign, disposed.sig_cgt
        ),
        disposed.sig_ign & disposed.sig_cgt == 0,
    );

    // A blocked signal that is raised stays pending: the bit appears in SigPnd.
    unsafe { libc::raise(libc::SIGUSR1) };
    let pending = self_status().unwrap_or(Lines { ..start });
    r.check(
        &format!(
            "a raised blocked signal shows in SigPnd ({:016x})",
            pending.sig_pnd
        ),
        pending.sig_pnd & bit(libc::SIGUSR1) != 0,
    );

    // The switch lines are this thread's own, and a sleep is one voluntary
    // switch (measured on Linux: 50 sleeps move the line from 17 to 67).
    let slept = {
        for _ in 0..50 {
            sleep_a_millisecond();
        }
        self_status().unwrap_or(pending)
    };
    r.check(
        &format!(
            "fifty sleeps move voluntary_ctxt_switches by fifty ({} -> {})",
            pending.voluntary, slept.voluntary
        ),
        slept.voluntary >= pending.voluntary + 50,
    );
    if !host_run() {
        // On this kernel a sleep parks the task, which is one voluntary switch
        // and nothing involuntary; the host adds its own libc waits in between,
        // which is why this half is the machine's.
        r.check(
            &format!(
                "and none of them is an involuntary one ({} -> {})",
                pending.nonvoluntary, slept.nonvoluntary
            ),
            slept.nonvoluntary <= pending.nonvoluntary + 10,
        );
    }

    // A sibling's sleeps belong to that sibling's own lines, not to the main
    // thread's - measured on Linux, where the worker read 50 and the main
    // thread 16 for the same fifty sleeps.
    let main_before = slept.voluntary;
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for _ in 0..50 {
            sleep_a_millisecond();
        }
        let own = status_of(&format!("/proc/self/task/{}/status", unsafe {
            libc::gettid()
        }))
        .map(|lines| lines.voluntary);
        let _ = sender.send(own);
    });
    let own = receiver.recv().ok().flatten();
    let _ = worker.join();
    let main_after = self_status()
        .map(|lines| lines.voluntary)
        .unwrap_or(main_before);
    r.check(
        &format!("a sibling's sleeps are that sibling's lines ({own:?})"),
        own.is_some_and(|value| value >= 40),
    );
    r.check(
        &format!("and not this thread's ({main_before} -> {main_after})"),
        main_after < main_before + 40,
    );

    r.finish();
}
