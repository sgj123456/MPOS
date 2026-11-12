//! `/proc/<pid>/oom_score`: the OOM killer's badness score for a task.
//!
//! The file is a *view* of two facts this kernel already keeps - a task's
//! resident pages and its one `oom_score_adj` word - so the claims here are
//! relations between doors, not constants two machines cannot agree on:
//!
//! * the body is a bare decimal and one newline, `0444` and read-only;
//! * raising `oom_score_adj` raises the score, and writing it back restores the
//!   score it had;
//! * a larger resident set scores higher;
//! * another process's file is readable and follows that process's adjustment;
//! * a reaped pid is `ENOENT`.
//!
//! **Measured on the host first** (`build/scratch/oomscore/m.c`, Linux 7.2.14),
//! and one line of that measurement is why the claims stop at relations: at a
//! fixed small resident set the host read `800` at `oom_score_adj` 200, `866` at
//! 300, `1000` at 500 and `1333` at 1000 - a step of *two thirds* of the
//! adjustment, and above the scale's top (Linux's `proc_oom_score` clamps at
//! 1000). So the host's absolute number is not `(rss * 1000 / total) + adj`
//! either, and this kernel's own arithmetic on its own pages and frame total
//! cannot be expected to match it. What both obey is the shape asserted below.
//! The same run measured 256 MiB more resident moving the score `800 -> 804` at
//! a fixed adjustment, `0444` with a write-open answering `EACCES`, and a reaped
//! pid answering `ENOENT`.
//!
//! The read-only refusal is the node's *method* rather than a capability -
//! `proc_reg_open` refuses a write-open for root too (`t-proc-words` measured
//! the guest agreeing) - so no claim here branches on the caller's uid.

use mpos_user_tests::Report;

/// `errno` values these files answer with.
const EACCES: i32 = 13;
const ENOENT: i32 = 2;

/// Read `path` as text.
fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Read `path` as a decimal word, trimmed of its one newline.
fn read_num(path: &str) -> Option<i32> {
    read_text(path)?.trim_end_matches('\n').parse::<i32>().ok()
}

/// Write `text` to `path`, answering `0` or the errno (including an errno the
/// *open* produced, which is where the `0444` refusal lands).
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

fn main() {
    let mut r = Report::new();
    let me = std::process::id();
    let score_path = format!("/proc/{me}/oom_score");
    let adj_path = format!("/proc/{me}/oom_score_adj");

    // ---- the body --------------------------------------------------------
    let body = read_text(&score_path);
    // A bare decimal: digits, exactly one trailing newline, no sign, no padding
    // (the only word that may start with `0` is `0` itself). Measured `800\n`.
    let shape = body.as_deref().is_some_and(|text| {
        let digits = &text[..text.len().saturating_sub(1)];
        text.len() >= 2
            && text.ends_with('\n')
            && !digits.contains('\n')
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'))
    });
    r.check(
        &format!("oom_score is a bare decimal word and one newline (got {body:?})"),
        shape,
    );
    let start = body
        .as_deref()
        .and_then(|text| text.trim_end_matches('\n').parse::<i32>().ok());
    r.check(
        &format!("oom_score parses as a decimal integer (got {start:?})"),
        start.is_some(),
    );
    r.check(
        &format!("oom_score is not negative (got {start:?})"),
        start.is_some_and(|value| value >= 0),
    );
    r.check(
        &format!("oom_score is 0444 (got {:?})", mode(&score_path)),
        mode(&score_path) == Some(0o444),
    );
    // Read-only: the refusal is the node's missing write *method*, so it is
    // `EACCES` for root as well as for an ordinary user.
    let refused = write(&score_path, "0\n");
    r.check(
        &format!("oom_score refuses a write-open with EACCES (got {refused})"),
        refused == EACCES,
    );

    // ---- the adjustment moves the score ---------------------------------
    // The claim is a *movement*, not a number: two machines with different
    // totals cannot agree on the absolute value, but both must move upward.
    let adjust = read_num(&adj_path).unwrap_or(-1000);
    let room = adjust <= 800;
    r.check(
        &format!("the starting oom_score_adj leaves room to raise it (got {adjust})"),
        room,
    );
    let raised = if room { adjust + 200 } else { 1000 };
    let _ = write(&adj_path, &format!("{raised}\n"));
    let after_raise = read_num(&score_path);
    r.check(
        &format!("raising oom_score_adj by 200 raises the score ({start:?} -> {after_raise:?})"),
        after_raise
            .zip(start)
            .is_some_and(|(after, before)| after > before),
    );
    // Writing the adjustment back puts the score back, which is what shows the
    // file is *computed from* the stored word rather than a cached number.
    let _ = write(&adj_path, &format!("{adjust}\n"));
    let restored = read_num(&score_path);
    r.check(
        &format!("writing oom_score_adj back restores the score ({start:?} vs {restored:?})"),
        restored == start,
    );

    // ---- another process, and a pid that has gone -----------------------
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::pause() };
        std::process::exit(0);
    }
    if child > 0 {
        // A live child exists and is not a zombie the moment `fork` returns;
        // the small sleep only gives the parent's `/proc` lookup a settled
        // subject, it is not a claim about scheduling.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let child_score = format!("/proc/{child}/oom_score");
        let child_adj = format!("/proc/{child}/oom_score_adj");
        let before = read_num(&child_score);
        r.check(
            &format!("another process's oom_score is readable (got {before:?})"),
            before.is_some(),
        );
        // Raise the *child's* adjustment through its own file, as root in the
        // guest and as its same-uid parent on the host; `0444` is the score's
        // word and not a ptrace gate, so the parent may read it either way.
        let target = read_num(&child_adj)
            .map(|value| (value + 200).min(1000))
            .unwrap_or(1000);
        let wrote = write(&child_adj, &format!("{target}\n"));
        let after = read_num(&child_score);
        r.check(
            &format!(
                "another process's score follows the adjustment written for it \
                 ({before:?} -> {after:?}, wrote {wrote})"
            ),
            wrote == 0
                && after
                    .zip(before)
                    .is_some_and(|(after, before)| after > before),
        );
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, core::ptr::null_mut(), 0);
        }
        let gone = std::fs::read_to_string(&child_score)
            .err()
            .and_then(|error| error.raw_os_error());
        r.check(
            &format!("a reaped process's oom_score is ENOENT (got {gone:?})"),
            gone == Some(ENOENT),
        );
    }

    // `0444` is everybody's word, so pid 1's file is readable too - the check
    // that this is not one of the ptrace-gated files beside it.
    r.check(
        "pid 1's oom_score is readable",
        read_num("/proc/1/oom_score").is_some(),
    );

    // ---- a larger resident set scores higher ----------------------------
    let before = read_num(&score_path);
    // 256 MiB, because the movement has to be visible on *both* machines: this
    // host's score moved `800 -> 804` for exactly this much (measured), and the
    // guest's resident proportion is far larger.
    //
    // The pages are **volatile**-touched through a raw mapping rather than
    // written in a loop over a `Vec` that is dropped afterwards: that version
    // passed on the host and read `0 -> 0` in the guest, because stores into
    // memory nothing ever reads are dead and the allocator's pages were never
    // faulted in - the measurement was of the compiler, not of the kernel. A
    // volatile write is an observable side effect, so residency is real.
    const BYTES: usize = 256 << 20;
    let mapping = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped = mapping != libc::MAP_FAILED;
    if mapped {
        for page in (0..BYTES).step_by(4096) {
            unsafe { core::ptr::write_volatile((mapping as *mut u8).add(page), 1) };
        }
    }
    let after = read_num(&score_path);
    if mapped {
        unsafe { libc::munmap(mapping, BYTES) };
    }
    r.check(
        &format!("a larger resident set scores higher ({before:?} -> {after:?})"),
        mapped
            && after
                .zip(before)
                .is_some_and(|(after, before)| after > before),
    );

    r.finish();
}
