//! t-ns-time - the clocks a process reads, which are a namespace's and not the
//! machine's.
//!
//! `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_MONOTONIC_COARSE` and
//! `CLOCK_BOOTTIME` are four *readings* of one uptime, and a time namespace
//! makes those readings depend on who is asking. §6.3's ladder puts it fifth
//! (UTS, MNT, PID, NET, TIME) and this program holds the pair the plan fixed for
//! every rung - **the child's view differs and the parent's does not** - which
//! for this kind is not a figure of speech but the literal mechanism:
//!
//! * `unshare(CLONE_NEWTIME)` replaces the caller's `time_ns_for_children` slot
//!   and **nothing else**, so `/proc/self/ns/time` is the same name it was and
//!   the caller's own readings never move, whatever it writes afterwards;
//! * the *children* it makes from then on are in the new namespace, and their
//!   monotonic clocks read the offsets that were written before they existed;
//! * the offsets are written through `/proc/self/timens_offsets`, which belongs
//!   to the **children's** namespace and is writable only for as long as nothing
//!   is in it - the first task to enter a namespace freezes it, which is why the
//!   order a caller has to use is unshare, write, fork;
//! * the clocks that move are the four uptime readings and no others: REALTIME,
//!   TAI and the two cputime clocks do not, because a certificate's validity
//!   must not shift because a namespace wanted an earlier uptime;
//! * and `setns(fd, CLONE_NEWTIME)` is the *other* door into the same slot - it
//!   takes a namespace somebody else made, and the caller still does not move.
//!
//! # The doors
//!
//! `rustix::thread::unshare_unsafe(UnshareFlags::NEWTIME)` makes the namespace
//! and `rustix::thread::move_into_link_name_space` joins one. Three claims
//! cannot come from a wrapper:
//!
//! * **the six clock ids** are read through `libc::clock_gettime`, one raw door
//!   for all of them, because the *ids* are the subject of the claim and
//!   `rustix`'s `ClockId` has no `MonotonicCoarse` or `Tai` variant - a mixed
//!   set of doors would make the six readings harder to compare than the point
//!   needs them to be.
//! * **`clone(CLONE_NEWTIME)`** is a raw `SYS_clone`, because what the claim is
//!   about is the *flag word*: `CLONE_NEWTIME` is `0x80`, entirely inside the
//!   low byte `clone(2)` reads as the exit signal, so the number a caller can
//!   actually pass is `0x91` and the answer is `EINVAL` from the signal check.
//!   The claim is that this is the errno, not which layer produced it.
//! * **the exact text of the offsets file** is read as bytes, because the claim
//!   includes the shape a real kernel prints (measured, §6.3) rather than only
//!   the two numbers inside it.
//!
//! # What a host run measures
//!
//! Nothing. `CLONE_NEWTIME` needs `CAP_SYS_ADMIN`, which this container's
//! capabilities do not hold, so the program notes that and exits green - the
//! same shape `t-ns-net` and `t-ns-pid` have. Every rule below was measured on a
//! host that *could* make one first (`.workbuddy/probe-timens{,1..10}.c`,
//! `probe-clone-timens.c`, `probe-timens-links.sh`), which is how the shape was
//! known before it was built rather than discovered by building it.

use mpos_user_tests::{Report, host_run};
use rustix::fd::AsFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{Errno, read, write};
use rustix::pipe::pipe;
use rustix::thread::{LinkNameSpaceType, UnshareFlags, move_into_link_name_space, unshare_unsafe};

/// `/proc/self/timens_offsets` - the two clocks the caller's *children* will
/// read, and the only door `unshare(CLONE_NEWTIME)` has for saying by how much.
const OFFSETS: &str = "/proc/self/timens_offsets";

/// The offsets this program writes, in seconds. Chosen so that "did the clock
/// move?" cannot be answered by noise: the uptime of a machine running a whole
/// test sweep is tens of seconds, and a child whose `CLOCK_MONOTONIC` reads
/// forty-two seconds further along than its parent's is unmistakable.
const MONOTONIC_SECS: i64 = 42;
const BOOTTIME_SECS: i64 = 3600;

/// The offsets [`join_the_children_slot`] writes, far enough from the ones above
/// that a reading can only be explained by one of them.
const JOINED_SECS: i64 = 111;

/// The clocks this program brackets, as `(id, offset, name)`.
///
/// Seven of the eleven ids a real kernel accepts, chosen because each one is a
/// different *answer* to "does a time namespace move this clock": the four that
/// move, the two wall clocks and one cputime clock that do not. The ids are
/// spelled rather than taken from `libc`, because `libc` names all of them and
/// the numbers are what a reader of the measurement wants to check against.
const CLOCKS: [(i32, i64, &str); 7] = [
    (1, MONOTONIC_SECS, "CLOCK_MONOTONIC"),
    (4, MONOTONIC_SECS, "CLOCK_MONOTONIC_RAW"),
    (6, MONOTONIC_SECS, "CLOCK_MONOTONIC_COARSE"),
    (7, BOOTTIME_SECS, "CLOCK_BOOTTIME"),
    (0, 0, "CLOCK_REALTIME"),
    (11, 0, "CLOCK_TAI"),
    (2, 0, "CLOCK_PROCESS_CPUTIME_ID"),
];

/// One clock, in nanoseconds since its own epoch. Zero is not a sentinel here:
/// a reading of zero is a real reading, and the comparisons below are
/// differences, so nothing is lost.
fn clock_ns(id: i32) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid clock id (measured: this kernel accepts `{0..=9, 11}`) and
    // a `timespec` we own.
    let rc = unsafe { libc::clock_gettime(id, &mut ts) };
    if rc != 0 {
        return i64::MIN;
    }
    (ts.tv_sec as i64) * 1_000_000_000 + ts.tv_nsec as i64
}

fn all_clocks() -> [i64; CLOCKS.len()] {
    let mut out = [0i64; CLOCKS.len()];
    for (slot, (id, _, _)) in CLOCKS.iter().enumerate() {
        out[slot] = clock_ns(*id);
    }
    out
}

/// Read a whole file, or `None` if it cannot be opened or read.
fn read_file(path: &str) -> Option<String> {
    let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty()).ok()?;
    let mut buf = vec![0u8; 512];
    let mut len = 0;
    loop {
        match read(&fd, &mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if len == buf.len() {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

/// Write a whole file, answering the errno (`0` for success) - the errno *is*
/// the claim for two of the writes below, so it cannot be swallowed into a
/// `Result`.
fn write_file(path: &str, text: &str) -> isize {
    let fd = match openat(CWD, path, OFlags::WRONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => return -(e.raw_os_error() as isize),
    };
    match write(&fd, text.as_bytes()) {
        Ok(_) => 0,
        Err(e) => -(e.raw_os_error() as isize),
    }
}

fn readlink(path: &str) -> Option<String> {
    // `Vec::new()` is the reuse buffer the call wants back; a link's target is
    // 4096 bytes at most and the read grows it, so there is nothing to size
    // here.
    let target = rustix::fs::readlinkat(CWD, path, Vec::new()).ok()?;
    target.into_string().ok()
}

/// The `kind:[<id>]` text split into its two halves, so two links can be
/// compared by the *namespace* they name rather than by the whole string.
fn split_link(text: &str) -> Option<(&str, &str)> {
    let (kind, rest) = text.split_once(':')?;
    let id = rest.strip_prefix('[')?.strip_suffix(']')?;
    Some((kind, id))
}

/// A reading of one of the four moving clocks, with the bracket it has to fall
/// inside.
///
/// The child's reading and the parent's two are three samples of the *same*
/// monotone scale, one offset apart, so `child - after <= offset <= child -
/// before` is exact rather than approximate: no slack is needed for the time
/// either process spent running, because the parent's second reading is taken
/// after the child has already spoken and its first before the child existed.
fn bracketed(ok: bool, child: i64, before: i64, after: i64, offset: i64) -> bool {
    ok && child != i64::MIN && child - after <= offset && offset <= child - before
}

fn main() {
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: a host cannot create a time namespace (CLONE_NEWTIME needs CAP_SYS_ADMIN), so no claim is made here"
        );
        r.finish()
    }

    // ---- Before anything: the machine's namespace is frozen and its offsets
    //      are zero ----
    //
    // A machine whose init never unshared is *in* a time namespace, and it is
    // the frozen one: `init` is in it, so there is no moment at which it could
    // have been written. Measured on a real kernel (§6.3, `probe-timens7.c`):
    // the file reads two zero lines and a root write to it is `EACCES`. That is
    // the same `EACCES` a namespace gives once something has entered it, which
    // is the whole rule this program leans on below.
    let machine_offsets = read_file(OFFSETS);
    r.check(
        "the machine's offsets file reads two zero lines",
        machine_offsets.as_deref()
            == Some("monotonic           0         0\nboottime            0         0\n"),
    );
    r.check(
        "and refuses a write with EACCES, because init is in it",
        write_file(OFFSETS, "monotonic 1 0\n") == -(libc::EACCES as isize),
    );

    // ---- The door, and which slot it moves ----
    //
    // This process's own readings are taken *here*, before the call, because the
    // claim they carry is about the call: `unshare` moves the children's slot,
    // so neither it nor the offsets written after it may move the caller's own
    // clocks by anything at all.
    let initial = all_clocks();
    let own_slot_before = readlink("/proc/self/ns/time");
    let children_before = readlink("/proc/self/ns/time_for_children");
    r.check(
        "the two slots start out naming one namespace",
        own_slot_before.is_some() && own_slot_before == children_before,
    );

    let made = unsafe { unshare_unsafe(UnshareFlags::NEWTIME) };
    r.check("unshare(CLONE_NEWTIME) succeeds", made.is_ok());
    if let Err(e) = made {
        println!("note: {e} - the rest of this program needs the namespace");
        r.finish()
    }

    let own_after = readlink("/proc/self/ns/time");
    let children_after = readlink("/proc/self/ns/time_for_children");
    r.check(
        "the caller's own slot does not move",
        own_after.is_some() && own_after == own_slot_before,
    );
    r.check(
        "the children's slot moves to a namespace of its own",
        children_after.is_some() && children_after != children_before,
    );
    r.check(
        "...and both are still `time:[<id>]`",
        children_after
            .as_deref()
            .and_then(split_link)
            .is_some_and(|(kind, id)| kind == "time" && !id.is_empty()),
    );

    // ---- The offsets file belongs to the children's namespace ----
    r.check(
        "the offsets of a fresh namespace are zero too",
        read_file(OFFSETS).as_deref()
            == Some("monotonic           0         0\nboottime            0         0\n"),
    );
    r.check(
        "and the write is accepted, because nothing is in it yet",
        write_file(
            OFFSETS,
            &format!("monotonic {MONOTONIC_SECS} 0\nboottime {BOOTTIME_SECS} 0\n"),
        ) == 0,
    );
    // The claim is the *shape* a real kernel prints and not only the two
    // numbers, so the text is checked as text: two 31-byte lines, each a name
    // and two right-aligned fields.
    let written = read_file(OFFSETS).unwrap_or_default();
    let lines: Vec<&str> = written.lines().collect();
    r.check(
        "and reads back as two lines of the shape a real kernel prints",
        lines.len() == 2 && lines.iter().all(|l| l.len() == 31),
    );
    r.check(
        "...naming the two clocks with the numbers that were written",
        lines
            .first()
            .is_some_and(|l| l.split_whitespace().eq(["monotonic", "42", "0"]))
            && lines
                .get(1)
                .is_some_and(|l| l.split_whitespace().eq(["boottime", "3600", "0"])),
    );
    // A grammar the kernel agrees with is part of the door being real: text a
    // real kernel refuses must be refused here, and the senders of this file do
    // misspell it (`probe-timens9.c` is nine wrong writes).
    r.check(
        "a name that is not one of the two clocks is EINVAL",
        write_file(OFFSETS, "wallclock 1 0\n") == -(libc::EINVAL as isize),
    );
    r.check(
        "and a nanosecond field outside 0..1e9 is EINVAL",
        write_file(OFFSETS, "monotonic 0 1000000000\n") == -(libc::EINVAL as isize),
    );
    r.check(
        "and a refused write changed nothing",
        read_file(OFFSETS).unwrap_or_default() == written,
    );

    // ...and none of it moved the caller. Measured across the unshare *and* the
    // write rather than around the child's run, because that is the difference
    // between the two slots being one and being two: a kernel that put the
    // caller into the namespace it had just written would show the offsets here
    // and nowhere else. The seven deltas are printed with the claim, so a red
    // one says which clock and by how much; the bound is five seconds, which is
    // four times below the smaller offset and far above anything a syscall pair
    // on a serial one-vCPU guest can take.
    let own_now = all_clocks();
    let moved: Vec<i64> = own_now
        .iter()
        .zip(initial.iter())
        .map(|(now, then)| now - then)
        .collect();
    r.check(
        &format!("and the unshare and the write moved none of this process's own clocks (deltas {moved:?})"),
        moved.iter().all(|d| (0..5_000_000_000).contains(d)),
    );

    // ---- The pair: the child's clocks move, the caller's do not ----
    //
    // Read in one section, with one handshake, because the property is exactly
    // that the two readings *disagree by the offset*: an assertion spread over
    // two unsynchronised reads could pass on a machine where both moved, or
    // where neither did and the offset happened to be zero.
    let before = all_clocks();
    let report = pipe();
    r.check("the child's report pipe is made", report.is_ok());
    let Ok((report_r, report_w)) = report else {
        r.finish()
    };
    let child = unsafe { libc::fork() };
    if child == 0 {
        drop(report_r);
        let mine = all_clocks();
        let mut line = String::new();
        for v in mine {
            line.push_str(&format!("{v} "));
        }
        let _ = write(&report_w, line.as_bytes());
        drop(report_w);
        // The child must not run the rest of this program: `Report` would be
        // printed twice and the exit status would be the child's.
        unsafe { libc::_exit(0) }
    }
    drop(report_w);
    let mut text = String::new();
    {
        let mut buf = vec![0u8; 256];
        if let Ok(n) = read(&report_r, &mut buf) {
            text = String::from_utf8_lossy(&buf[..n]).into_owned();
        }
    }
    drop(report_r);
    let after = all_clocks();
    let parsed: Vec<i64> = text
        .split_whitespace()
        .filter_map(|w| w.parse::<i64>().ok())
        .collect();
    r.check(
        "the child reports every clock it was asked for",
        parsed.len() == CLOCKS.len(),
    );

    let in_ns = readlink(&format!("/proc/{child}/ns/time"));
    r.check(
        "...and the child is in the new namespace, its own slot",
        in_ns.is_some() && in_ns == children_after,
    );
    for (slot, (_, offset, name)) in CLOCKS.iter().enumerate() {
        let got = parsed.get(slot).copied().unwrap_or(i64::MIN);
        r.check(
            &format!(
                "{name} is {} by {offset} s in the child and unmoved here",
                if *offset == 0 { "unmoved" } else { "moved" }
            ),
            bracketed(true, got, before[slot], after[slot], offset * 1_000_000_000),
        );
    }
    // The caller's own readings: the *same* clocks, bracketed around the child's
    // run, with the offsets they would have had if `unshare` had moved the
    // caller. This is the half a "namespace" that is really a shared global
    // fails, and it is why the four claims above are not enough on their own.
    for (slot, (_, _, name)) in CLOCKS.iter().enumerate() {
        r.check(
            &format!("...and this process's own {name} never saw the offset"),
            bracketed(true, after[slot], before[slot], after[slot], 0),
        );
    }
    // The interlock that makes the seven claims above load-bearing rather than
    // vacuous. On their own they would also pass on a kernel with **one** slot
    // for both, because a namespace whose offsets are zero moves nothing - what
    // they say is "these readings did not move", not "this process is in a
    // different namespace from the one those numbers belong to". Reading the
    // offset back *here*, after it has moved the child, is the other half: the
    // numbers are this process's to read and the child's to be moved by, which
    // is only true while the two slots are two.
    r.check(
        "...while the very offsets that moved it are still readable here, because the file is the children's slot",
        read_file(OFFSETS).unwrap_or_default() == written,
    );

    // ---- The freeze: the first task in a namespace fixes its offsets ----
    r.check(
        "a write after a task has entered the namespace is EACCES",
        write_file(OFFSETS, "monotonic 1 0\n") == -(libc::EACCES as isize),
    );

    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the child exited on its own status",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // ---- `clone` is not a second door ----
    //
    // `CLONE_NEWTIME` is `0x80`, inside the low byte `clone(2)` reads as the
    // exit signal, so `CLONE_NEWTIME | SIGCHLD` reaches the kernel as `0x91` and
    // the number 145 is not a signal. The errno is therefore `EINVAL` and not
    // `EOPNOTSUPP`: the request is not "unsupported", it is unreadable. Measured
    // both ways on a host (§6.3): `clone(0x91)` is `-22` while
    // `clone(SIGCHLD|CLONE_NEWNET)` succeeds, and `clone3` - which carries the
    // exit signal in its own field - *creates* a task for the flag, so what is
    // refused is the spelling and not the namespace.
    //
    // The call is made with the child's stack slot zeroed and the guard below
    // exits any child that appears, so a kernel that wrongly accepted the word
    // would fail the claim rather than fork a second copy of this program.
    let cloned =
        unsafe { libc::syscall(libc::SYS_clone, 0x91usize, 0usize, 0usize, 0usize, 0usize) };
    if cloned == 0 {
        unsafe { libc::_exit(0) }
    }
    let errno = unsafe { *libc::__errno_location() };
    r.check(
        "clone(CLONE_NEWTIME|SIGCHLD) is EINVAL - the flag is in the signal byte",
        cloned == -1 && Errno::from_raw_os_error(errno) == Errno::INVAL,
    );

    // ---- And `setns` is the other door into the same slot ----
    //
    // A join, not a copy, and against this kind it replaces the *children's*
    // slot exactly as `unshare` does - the caller still does not move. Measured
    // (§6.3, `probe-timens8.c`): joining a namespace that already has a process
    // in it answers `0`, because the freeze governs the offsets file and not who
    // may come in.
    join_the_children_slot(&mut r);

    r.finish()
}

/// Make a namespace in a child, join it from here, and check both halves: the
/// joining process takes the *children's* slot and keeps its own, and the child
/// it makes afterwards reads the joined namespace's offsets.
///
/// The namespace is made by a keeper, and what the joiner names is the keeper's
/// own child - the task actually **in** the namespace. That indirection is not
/// decoration: `unshare` does not move its caller, so the keeper's
/// `/proc/<keeper>/ns/time` names the namespace the keeper *inherited* rather
/// than the one it made, and a joiner naming that would be joining the
/// namespace it was already in and proving nothing. Naming the resident is this
/// file's first half seen from the other side.
///
/// Nothing outlives the section: the keeper waits for the resident, the joiner
/// kills the resident when it is done, and the keeper's `waitpid` returns so it
/// leaves. A fixture that leaves a task paused forever would be a machine later
/// tests inherit.
fn join_the_children_slot(r: &mut Report) {
    let handshake = pipe();
    r.check("the namespace keeper's pipe is made", handshake.is_ok());
    let Ok((ready_r, ready_w)) = handshake else {
        return;
    };
    let keeper = unsafe { libc::fork() };
    if keeper == 0 {
        drop(ready_r);
        // A namespace of its own, then the offsets *before* anything is in it -
        // the order this whole file is about.
        let made = unsafe { unshare_unsafe(UnshareFlags::NEWTIME) };
        let wrote = write_file(
            OFFSETS,
            &format!("monotonic {JOINED_SECS} 0\nboottime 0 0\n"),
        );
        // ...and only now does the first task enter it, which is what freezes
        // the offsets and what makes the namespace one a *task* is in - the
        // thing a `/proc/<pid>/ns/time` link can name.
        let resident = unsafe { libc::fork() };
        if resident == 0 {
            loop {
                unsafe { libc::pause() };
            }
        }
        let _ = write(&ready_w, format!("{made:?} {wrote} {resident}").as_bytes());
        drop(ready_w);
        // The keeper stays for as long as the resident does: the joiner's kill
        // ends this `waitpid` and the keeper leaves with it.
        let mut status = 0;
        unsafe { libc::waitpid(resident, &mut status, 0) };
        unsafe { libc::_exit(0) }
    }
    drop(ready_w);
    let mut buf = [0u8; 128];
    let n = read(&ready_r, &mut buf).unwrap_or(0);
    let ready = String::from_utf8_lossy(&buf[..n]).into_owned();
    drop(ready_r);
    let resident: libc::pid_t = ready
        .split_whitespace()
        .nth(2)
        .and_then(|f| f.parse().ok())
        .unwrap_or(-1);
    r.check(
        "the keeper made a namespace, set its offsets, and put a task in it",
        ready.starts_with("Ok(()) 0") && resident > 0,
    );
    if resident <= 0 {
        let mut status = 0;
        unsafe { libc::waitpid(keeper, &mut status, 0) };
        return;
    }

    let resident_link = format!("/proc/{resident}/ns/time");
    let resident_ns = readlink(&resident_link);
    let own_ns = readlink("/proc/self/ns/time");
    r.check(
        "the resident is in the namespace the keeper made, not this process's",
        resident_ns.is_some() && resident_ns != own_ns,
    );

    let joined = openat(CWD, &resident_link, OFlags::RDONLY, Mode::empty())
        .and_then(|fd| move_into_link_name_space(fd.as_fd(), Some(LinkNameSpaceType::Time)));
    r.check("setns(fd, CLONE_NEWTIME) succeeds", joined.is_ok());

    let children_now = readlink("/proc/self/ns/time_for_children");
    r.check(
        "...and it took the children's slot",
        children_now.is_some() && children_now == resident_ns,
    );
    r.check(
        "...while this process's own slot still reads the one it had",
        readlink("/proc/self/ns/time").as_deref() == own_ns.as_deref(),
    );

    // The proof that the join is real rather than a name: a child made now reads
    // the *joined* namespace's offset, which is neither zero nor the one this
    // process wrote earlier.
    let before = clock_ns(1);
    if let Ok((report_r, report_w)) = pipe() {
        let child = unsafe { libc::fork() };
        if child == 0 {
            drop(report_r);
            let _ = write(&report_w, format!("{}", clock_ns(1)).as_bytes());
            drop(report_w);
            unsafe { libc::_exit(0) }
        }
        drop(report_w);
        let mut b = [0u8; 64];
        let n = read(&report_r, &mut b).unwrap_or(0);
        drop(report_r);
        let mine = clock_ns(1);
        let got = String::from_utf8_lossy(&b[..n])
            .trim()
            .parse::<i64>()
            .unwrap_or(i64::MIN);
        r.check(
            "a child born after the join reads the joined namespace's offset",
            bracketed(true, got, before, mine, JOINED_SECS * 1_000_000_000),
        );
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        r.check(
            "that child exited on its own status",
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        );
    }

    // The resident goes, and the keeper with it.
    unsafe { libc::kill(resident, libc::SIGKILL) };
    let mut status = 0;
    unsafe { libc::waitpid(keeper, &mut status, 0) };
    r.check(
        "the keeper and its resident are gone, leaving nothing paused",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
}
