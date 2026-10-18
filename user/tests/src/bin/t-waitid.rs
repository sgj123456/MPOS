//! t-waitid - a child's fate, described as a `siginfo_t`.
//!
//! Pinned here, per waitid(2): `idtype` and `id` name the subject (`P_ALL`,
//! `P_PID` and `P_PGID`, and nothing else), `WEXITED` alone reports an exit as
//! `SIGCHLD`/`CLD_EXITED` with the child's pid in `si_pid` and its code in
//! `si_status`, `WSTOPPED` and `WCONTINUED` report a stop and a resume as their
//! own `CLD_*` codes, `WNOHANG` answers 0 with the record's six words zeroed when
//! nothing is ready, `WNOWAIT` reports without collecting, and a call that asks
//! for no event class at all - or carries a bit Linux's own options word does
//! not define - is refused, with those same six words written as zeros.
//!
//! Every one of those is Linux's answer measured on a current host before it was
//! written down (`.workbuddy/probe_waitid.c`), and this program carries the
//! claims that tell the shapes apart rather than the ones that agree by
//! accident. Three of them are worth naming, because each was a divergence this
//! kernel had until 2026-10-16 (`docs/uapi/` §5.2 carried all three):
//!
//! * **`id` names the child.** `P_PID` used to collect any ready child, so a
//!   caller naming one child could be handed another's record. The claim below
//!   exists for exactly that: a zombie sibling is present while the named child
//!   is still running, and the answer must be "none yet" (0 with `si_pid`
//!   cleared) rather than the sibling.
//! * **`P_PGID` selects a group.** It used to be `EINVAL`; it now resolves the
//!   group's leader the way `setpgid`/`getpgid` do, with `id = 0` meaning the
//!   caller's own group. The pair below is what makes the selector visible: a
//!   zombie in *our* group is invisible to `P_PGID, <the other group>` and
//!   visible to `P_PGID, 0`.
//! * **A call that asks for no event is `EINVAL`.** It used to answer 0 with a
//!   cleared record, which is the one answer a caller cannot tell from "no event
//!   yet". Linux writes the record *and* refuses, and the measurement says the
//!   write happens before the refusal for every refusal this door has - which is
//!   why each of the three below reads the record back, and what the last section
//!   pins the reach and the order of (`docs/uapi/` §5.1 item 51).
//!
//! One divergence stays, and it is named rather than hidden:
//!
//! * **`P_PIDFD` is `EINVAL`.** Linux answers `EBADF` for a descriptor that is
//!   not a pidfd (measured: fd 9999, errno 9) and serves a real one. This kernel
//!   has no pidfd family - `pidfd_open`, `clone3(CLONE_PIDFD)` and
//!   `pidfd_send_signal` are all absent - so no descriptor can name a task here,
//!   and a refusal is the honest answer (`docs/uapi/` §5.3).
//!
//! **`si_uid` is the child's real uid, and it is the record's now** (`docs/uapi/`
//! §5.1 item 23): the kernel carries it on `Retired`, because a death outlives
//! the task's slot, and asks the live identity for a stop or a resume, which are
//! reported while the child is still in the table. Measured on a current host
//! before the claims were written (`.workbuddy/probe_waitid.c`): `CLD_EXITED`,
//! `CLD_TRACED`, `CLD_STOPPED` and `CLD_CONTINUED` all report the child's uid,
//! while a call that faces no event at all is a cleared record instead.
//!
//! **The fifth argument, `struct rusage`, is filled** (`docs/uapi/` §5.1 item
//! 34): Linux copies the child's `RUSAGE_BOTH` CPU into it for every event the
//! call *reports*, and leaves the caller's bytes alone when it reports nothing.
//! This door used to ignore the pointer entirely - the two claims below are what
//! tells a report from the zero a polling parent gets, and `t-wait4` carries the
//! CPU value and the bad-pointer orderings for the shape both doors share.
//!
//! The last section adds the claims that tell that uid from the zero the bug
//! wrote, and it needs a *privilege* to make them: it moves the program's own
//! real uid (to `MOVED`) while keeping its effective one, so the children it
//! forks from there on have two different ids for the record to choose between -
//! a record that reports the real uid answers `MOVED` where one that reports the
//! effective id, or never filled the field, answers 0. Moving an id needs
//! `CAP_SETUID`, which the payload has (it runs as root) and a hand host run
//! often does not (measured: this sandbox's shell has an empty effective
//! capability set, and its uid is already `MOVED`, so nothing there would move
//! anyway); that section is therefore made where it can be and named where it
//! cannot, in `t-posix`'s shape rather than hidden behind the flag. Everything
//! before it runs under the identity the runner handed over.
//!
//! A wait with no matching child at all is `ECHILD`, and that is pinned here: a
//! second wait for a child already collected must answer it rather than park.
//! The probe runs in a child with a deadline of its own, so a kernel that
//! regressed to parking shows up as a failed check rather than a hung suite -
//! the deadline is what a `waitid` asleep inside the probe cannot blow past.
//!
//! The `EFAULT` shapes need the record's *address* rather than a reference, so
//! the claims about them go through a raw door of their own (`waitid_raw`) and
//! hand it a `PROT_NONE` page - the wrappers above cannot, and a null record is
//! not one of those shapes at all: Linux accepts it.
//!
//! # Where the two writes go
//!
//! The last section pins what a success path cannot see, because it is visible
//! only to a caller that hands the kernel a pointer it cannot write to: the wait
//! runs first (so a refusal costs the caller its `EFAULT` *after* the child is
//! collected), the `struct rusage` is copied only when an event was reported, and
//! the record is written last - six words, not a cleared buffer - with its own
//! `EFAULT` outranking whatever the wait said. A null record is legal, and a
//! record whose tail is unmapped is still served when the six words land inside
//! the mapped part. Measured on a current host first
//! (`.workbuddy/probe_waitid_record.c`, Linux 7.2.9: scenarios P-Z and AA-AH).
//!
//! # The migration
//!
//! The call is the raw `SYS_waitid` (247 here, 95 on the second machine, both
//! taken from libc's per-machine table), because the record the kernel writes
//! *is* the claim and `rustix::process::waitid` owns its buffer: the 0xFF seed
//! that proves a refused or `WNOHANG`-empty call writes zeros into the six words
//! it owns (and only those), and the fields
//! read back out of it, have nowhere to go through a wrapper that hands over its
//! own.
//!
//! What the raw door must not carry is this repository's memory of the record's
//! *layout*, which is what the no-std version did - `si_pid` at byte 16 and
//! `si_status` at 24, read through a helper that took a raw offset. The
//! migrated program reads the machine's own `libc::siginfo_t` instead:
//! `si_signo`/`si_code` are fields, `si_pid()`/`si_status()` are libc's
//! accessors into the union, and the record's size is pinned against the ABI's
//! own `SI_MAX_SIZE` (128) rather than against a number written here. Measured
//! before the change was written, by dumping a real record from this host's
//! kernel through musl: `si_signo` 17 at byte 0, `si_code` 1 at byte 8, the
//! child's pid at 16, its uid at 20, the exit code at 24 - the asm-generic
//! `{signo, errno, code, union}` with the union padded to its 8-byte alignment,
//! which is the shape of *both* machines here (x86_64 includes asm-generic's
//! header; arm64 is asm-generic). The offsets the no-std version wrote down were
//! therefore right on both machines and are now unnecessary on both.
//!
//! The polling that watches the ECHILD probe and the children that sleep are
//! `rustix`'s `waitpid` (`wait4` with a null `rusage`, and `Ok(None)` for
//! `WNOHANG` finding nobody ready) and `libc::nanosleep`; every child leaves
//! through the raw `SYS_exit_group`, because a forked std child holds a copy of
//! the parent's buffered stdout and only a non-flushing exit leaves it behind.

use mpos_user_tests::{Report, Rusage, burn_cpu_ms};
use rustix::process::{Pid, WaitOptions, waitpid};
use std::mem::size_of;

/// `waitid(idtype, id, infop, options, rusage)` through the raw door, with the
/// caller's own record. Returns the answer, or the negative errno the no-std
/// harness reported.
///
/// `options` is an `i32` here for the reason Linux declares it one: `__WCLONE` is
/// bit 31, so a C caller passes a *negative* `int`, the register arrives
/// sign-extended, and the kernel is owed the low 32 bits and nothing else. The
/// `__WCLONE` claims below are what caught this kernel reading the whole
/// register - the call was `EINVAL`, because the upper half looked like an
/// undefined option bit.
fn waitid(idtype: u32, id: i32, info: &mut libc::siginfo_t, options: i32) -> isize {
    waitid_rusage(idtype, id, info, options, core::ptr::null_mut())
}

/// [`waitid`] with the fifth argument spelled: the `struct rusage` the call
/// fills whenever it *reports* an event, which is what the claims about the
/// record's CPU read - and, with a seed in place, what tells a report from the
/// "nothing yet" zero.
fn waitid_rusage(
    idtype: u32,
    id: i32,
    info: &mut libc::siginfo_t,
    options: i32,
    rusage: *mut Rusage,
) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_waitid,
            idtype as libc::c_long,
            id as libc::c_long,
            info as *mut libc::siginfo_t as libc::c_long,
            options as libc::c_long,
            rusage as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// A record seeded with 0xFF, so a field the kernel zeroes is a field the caller
/// can see cleared.
fn poisoned() -> libc::siginfo_t {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        std::ptr::write_bytes(
            (&raw mut info).cast::<u8>(),
            0xFF,
            size_of::<libc::siginfo_t>(),
        );
    }
    info
}

/// Sleep out a whole number of milliseconds through the raw door, so a forked
/// child never touches the allocator.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    unsafe {
        libc::nanosleep(&req, core::ptr::null_mut());
    }
}

/// Leave through `exit_group`: the code is the claim, and the child's inherited
/// copy of the parent's stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        // The door does not return; `_exit` is the maintained fallback.
        libc::_exit(code)
    }
}

/// A child that exits with `code` at once, and its pid.
fn child_that_exits(code: i32) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        leave(code);
    }
    kid
}

/// A child that burns `ms` of CPU and then exits 0 - the fixture a claim about
/// the CPU *in* the record needs, since a child that merely exits has used
/// almost none and "the CPU arrived" would be indistinguishable from "the record
/// was zeroed".
fn child_that_burns(ms: i64) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        burn_cpu_ms(ms);
        leave(0);
    }
    kid
}

/// A child that is still running while the claim beside it is made, and leaves
/// on its own after `ms` - the fixture for "a call that reports nothing writes
/// nothing".
fn child_that_sleeps(ms: i64) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        sleep_ms(ms);
        leave(0);
    }
    kid
}

/// The same, with one of its uids moved first - the child that tells `si_uid`'s
/// *real* id from its effective one. `None` leaves that id alone, which is what
/// `setresuid`'s `-1` says, and the child leaves with `2` when the move is
/// refused so a claim that reads `si_status` cannot pass vacuously on a child
/// that never moved anything.
fn child_with_ids(real: Option<u32>, effective: Option<u32>) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        let none = -1i32 as libc::uid_t;
        let stored = |id: Option<u32>| id.map_or(none, |uid| uid as libc::uid_t);
        let moved = unsafe { libc::setresuid(stored(real), stored(effective), none) };
        leave(if moved == 0 { 0 } else { 2 });
    }
    kid
}

/// The same with a `SIGSTOP` after the move, so the *live* path - the record a
/// stop writes while the child is still in the task table - reports the moved
/// id too.
fn child_with_ids_stopping(real: Option<u32>) -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        let none = -1i32 as libc::uid_t;
        let stored = |id: Option<u32>| id.map_or(none, |uid| uid as libc::uid_t);
        let moved = unsafe { libc::setresuid(stored(real), none, none) };
        if moved != 0 {
            leave(2);
        }
        unsafe {
            libc::raise(libc::SIGSTOP);
        }
        sleep_ms(1500);
        leave(0);
    }
    kid
}

/// Poll `P_PID` on `kid` until an event of the asked-for kinds is ready, with a
/// deadline: `None` when the deadline passes first.
///
/// A stop and a resume are events of the *child's* making, so the parent has to
/// wait for them to happen - on the second machine the whole sweep runs under
/// TCG, where a child's first instruction is milliseconds away.
fn await_event(kid: libc::pid_t, options: i32) -> Option<libc::siginfo_t> {
    for _ in 0..400 {
        let mut info = poisoned();
        if waitid(libc::P_PID, kid, &mut info, options | libc::WNOHANG) == 0
            && unsafe { info.si_pid() } == kid
        {
            return Some(info);
        }
        sleep_ms(5);
    }
    None
}

fn main() {
    println!("t-waitid");
    let mut r = Report::new();

    // The record's size is the ABI's, not this file's: `SI_MAX_SIZE` is 128 on
    // both machines, and the fields below are read out of a struct of it.
    r.check(
        "the siginfo record is the size the ABI names (128)",
        size_of::<libc::siginfo_t>() == 128,
    );

    // The idtypes. `P_ALL`/`P_PID`/`P_PGID` are Linux's first three; the fourth,
    // `P_PIDFD`, needs a pidfd family this kernel does not have, and an idtype
    // outside the four is a request Linux refuses too. Measured on a current
    // host: a caller with no children at all asking for its own group
    // (`P_PGID`, id 0) is answered `ECHILD` (errno 10), and fd 9999 as a
    // `P_PIDFD` is `EBADF` (errno 9) - which is the answer this kernel cannot
    // give and names instead (`docs/uapi/` §5.3).
    let mut info = poisoned();
    let answered = waitid(libc::P_PGID, 0, &mut info, libc::WEXITED | libc::WNOHANG);
    r.check(
        "P_PGID on the caller's own group is served",
        answered == -(libc::ECHILD as isize),
    );
    let mut info = poisoned();
    r.check(
        "P_PGID naming a group with no children is ECHILD",
        waitid(libc::P_PGID, 4242, &mut info, libc::WEXITED | libc::WNOHANG)
            == -(libc::ECHILD as isize),
    );
    let mut info = poisoned();
    r.check(
        "a negative P_PGID id is EINVAL",
        waitid(libc::P_PGID, -1, &mut info, libc::WEXITED | libc::WNOHANG)
            == -(libc::EINVAL as isize),
    );
    let mut info = poisoned();
    r.check(
        "an idtype outside Linux's four is EINVAL",
        waitid(4, 0, &mut info, libc::WEXITED | libc::WNOHANG) == -(libc::EINVAL as isize),
    );
    // `P_PIDFD` is the idtype this kernel cannot answer, and the reason is the
    // one above: no pidfd family exists here, so no descriptor can name a task
    // and the honest answer is "this idtype is not served". Linux *does* serve
    // it, and answers a descriptor that is not a pidfd with `EBADF` - measured,
    // fd 9999, errno 9 - so the host run names the answer this kernel owes a
    // prerequisite for rather than pretending to share it (`docs/uapi/` §5.3).
    let mut info = poisoned();
    let answered = waitid(
        libc::P_PIDFD,
        9999,
        &mut info,
        libc::WEXITED | libc::WNOHANG,
    );
    r.check(
        "P_PIDFD is EINVAL here (no pidfd family to resolve a descriptor through)",
        answered
            == if mpos_user_tests::host_run() {
                -(libc::EBADF as isize)
            } else {
                -(libc::EINVAL as isize)
            },
    );

    // A call that asks for no event class, and one carrying a bit Linux's own
    // options word does not define: both are refused, and both clear the record
    // *before* the refusal - Linux's order, measured, and the half a caller can
    // read back. An unknown bit is `0x10000000`, which is none of
    // `WNOHANG`/`WNOWAIT`/`WEXITED`/`WSTOPPED`/`WCONTINUED`/`__WNOTHREAD`/
    // `__WALL`/`__WCLONE`.
    let mut info = poisoned();
    r.check(
        "a call asking for no event is EINVAL",
        waitid(libc::P_ALL, 0, &mut info, 0) == -(libc::EINVAL as isize),
    );
    r.check(
        "and it cleared the record first (si_pid cleared)",
        unsafe { info.si_pid() } == 0,
    );
    let mut info = poisoned();
    r.check(
        "an options bit Linux does not define is EINVAL",
        waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | 0x1000_0000) == -(libc::EINVAL as isize),
    );
    r.check(
        "and that refusal cleared the record too",
        unsafe { info.si_pid() } == 0,
    );

    // A child that exits 3: the info record carries signo, code, pid, status.
    let kid = child_that_exits(3);
    r.check("a child that exits 3 was forked", kid > 0);
    if kid > 0 {
        let mut info = poisoned();
        r.check(
            "waitid(P_PID, child, WEXITED) succeeds",
            waitid(libc::P_PID, kid, &mut info, libc::WEXITED) == 0,
        );
        r.check("si_signo is SIGCHLD", info.si_signo == libc::SIGCHLD);
        r.check("si_code is CLD_EXITED", info.si_code == libc::CLD_EXITED);
        r.check("si_pid is the child", unsafe { info.si_pid() } == kid);
        r.check(
            "si_status is the exit code 3",
            unsafe { info.si_status() } == 3,
        );
        r.check(
            "si_uid is the uid the child ran as",
            unsafe { info.si_uid() } == unsafe { libc::getuid() },
        );

        // A second wait for a child already collected: no child is left, so
        // Linux answers ECHILD instead of blocking. The probe runs in a child
        // with a deadline of its own, so a kernel that parked here again would
        // show up as the probe never exiting rather than as a hung suite.
        let probe = unsafe { libc::fork() };
        if probe == 0 {
            let mut probe_info = poisoned();
            let answered = waitid(libc::P_PID, kid, &mut probe_info, libc::WEXITED);
            leave(if answered == -(libc::ECHILD as isize) {
                0
            } else {
                1
            });
        }
        r.check("the ECHILD probe was forked", probe > 0);
        if probe > 0 {
            let want = Pid::from_raw(probe);
            let mut status = None;
            let mut tries = 0;
            while status.is_none() && tries < 200 {
                status = waitpid(want, WaitOptions::NOHANG)
                    .ok()
                    .flatten()
                    .map(|(_, status)| status);
                if status.is_none() {
                    sleep_ms(1);
                }
                tries += 1;
            }
            r.check(
                "a second wait for a collected child answers ECHILD, not a park",
                status.is_some_and(|s| s.exited() && s.exit_status() == Some(0)),
            );
            if status.is_none() {
                // Still parked: end the probe so the rest of the suite runs.
                unsafe {
                    libc::kill(probe, libc::SIGKILL);
                }
                let _ = waitpid(want, WaitOptions::empty());
            }
        }
    }

    // `id` names the child. A zombie sibling is present while the named child is
    // still running: Linux reports nothing for a child that has not finished,
    // and hands the *sibling's* record to nobody but the caller that names it.
    // A kernel that ignores its `id` answers with the sibling here - which is
    // exactly the divergence this claim was written for.
    let alive = unsafe { libc::fork() };
    if alive == 0 {
        sleep_ms(1500);
        leave(0);
    }
    let zombie = child_that_exits(4);
    r.check(
        "the running child and its zombie sibling were forked",
        alive > 0 && zombie > 0,
    );
    if alive > 0 && zombie > 0 {
        // Wait for the sibling to be a zombie, so the only thing that can keep
        // the next answer honest is the `id`. `WNOWAIT` is load-bearing here:
        // this probe must *look* at the record without collecting it, or the
        // claim two below has nothing left to find - which is what a host run
        // of the first draft measured.
        let mut tries = 0;
        let mut ready = false;
        while !ready && tries < 400 {
            let mut probe = poisoned();
            ready = waitid(
                libc::P_PID,
                zombie,
                &mut probe,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) == 0
                && unsafe { probe.si_pid() } == zombie;
            if !ready {
                sleep_ms(5);
            }
            tries += 1;
        }
        r.check("the sibling became a zombie", ready);

        let mut info = poisoned();
        let answered = waitid(libc::P_PID, alive, &mut info, libc::WEXITED | libc::WNOHANG);
        r.check(
            "a zombie sibling is not the named child's event",
            answered == 0 && unsafe { info.si_pid() } == 0,
        );

        // And the sibling is still there to be collected, by name.
        let mut info = poisoned();
        r.check(
            "so the sibling's own record is still waiting",
            waitid(libc::P_PID, zombie, &mut info, libc::WEXITED) == 0
                && unsafe { info.si_status() } == 4,
        );

        // `P_ALL` takes whichever child is ready - the running one has not
        // finished, so this is the same "collect by any means" door `wait4`
        // offers.
        let any = child_that_exits(5);
        r.check("a child that exits 5 was forked", any > 0);
        if any > 0 {
            let mut info = poisoned();
            r.check(
                "waitid(P_ALL) collects a child",
                waitid(libc::P_ALL, 0, &mut info, libc::WEXITED) == 0,
            );
            r.check(
                "P_ALL reports the child's pid",
                unsafe { info.si_pid() } == any,
            );
            r.check("P_ALL reports its code", unsafe { info.si_status() } == 5);
        }

        // WNOHANG while the only child left is still running: 0 with the info
        // cleared, not an error and not a blocking wait.
        let mut info = poisoned();
        let answered = waitid(libc::P_PID, alive, &mut info, libc::WEXITED | libc::WNOHANG);
        r.check(
            "WNOHANG with nothing ready answers 0, not an error",
            answered == 0,
        );
        r.check("and clears si_pid to 0", unsafe { info.si_pid() } == 0);

        // Collect it, so it cannot be mistaken for a later check's child.
        let mut info = poisoned();
        r.check(
            "the blocking wait then collects the sleeper",
            waitid(libc::P_PID, alive, &mut info, libc::WEXITED) == 0,
        );
    }

    // `P_PGID` selects a group. A child that puts itself in a group of its own
    // is the discriminating case: with a zombie in *our* group, a wait on the
    // child's group must report nothing, and a wait on our own group must
    // report the zombie. A kernel that ignores the selector answers the zombie
    // to both, which is why the pair is here rather than one claim.
    let grouped = unsafe { libc::fork() };
    if grouped == 0 {
        sleep_ms(2500);
        leave(8);
    }
    // The group is set from *both* sides, which is the idiom every shell uses
    // and the only race-free one: whichever of the two runs first wins, and by
    // the time the parent looks, the child is in a group of its own rather than
    // in ours - the single fact the pair of claims below is about.
    if grouped > 0 {
        unsafe {
            libc::setpgid(grouped, grouped);
        }
    }
    let ours = child_that_exits(6);
    r.check(
        "a child in its own group and one in ours were forked",
        grouped > 0 && ours > 0,
    );
    if grouped > 0 && ours > 0 {
        let mut tries = 0;
        let mut ready = false;
        while !ready && tries < 400 {
            let mut probe = poisoned();
            ready = waitid(
                libc::P_PGID,
                0,
                &mut probe,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) == 0
                && unsafe { probe.si_pid() } == ours;
            if !ready {
                sleep_ms(5);
            }
            tries += 1;
        }
        r.check("the sibling in our own group became collectable", ready);

        // The other group has nothing ready, and its member is not ours to
        // mistake: `P_PGID, <that child's pid>` is that group, whose leader is
        // the child itself.
        let mut info = poisoned();
        let answered = waitid(
            libc::P_PGID,
            grouped,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
        );
        r.check(
            "a wait on the other group reports nothing of ours",
            answered == 0 && unsafe { info.si_pid() } == 0,
        );

        let mut info = poisoned();
        let answered = waitid(libc::P_PGID, 0, &mut info, libc::WEXITED);
        r.check(
            "and our own group collects our child",
            answered == 0 && unsafe { info.si_pid() } == ours,
        );
        r.check("with the sibling's code", unsafe { info.si_status() } == 6);

        let mut info = poisoned();
        r.check(
            "the grouped child is collected by pid as usual",
            waitid(libc::P_PID, grouped, &mut info, libc::WEXITED) == 0
                && unsafe { info.si_status() } == 8,
        );
    }

    // A stop and a resume, as their own records: `WSTOPPED` makes a child that
    // `SIGSTOP`s itself an event reportable to a parent that asked to hear
    // about it, and `WCONTINUED` reports the `SIGCONT` that ends it. Both were
    // a zero-filled 0 before 2026-10-16 - the same answer as "nothing yet",
    // which is why a job-control parent could never see either.
    let stopper = unsafe { libc::fork() };
    if stopper == 0 {
        unsafe {
            libc::raise(libc::SIGSTOP);
        }
        sleep_ms(1500);
        leave(7);
    }
    r.check("a child that stops itself was forked", stopper > 0);
    if stopper > 0 {
        match await_event(stopper, libc::WSTOPPED) {
            Some(info) => {
                r.check(
                    "a stopped child is reported by WSTOPPED",
                    info.si_signo == libc::SIGCHLD,
                );
                r.check("si_code says a stop", info.si_code == libc::CLD_STOPPED);
                r.check(
                    "si_status is the stopping signal",
                    unsafe { info.si_status() } == libc::SIGSTOP,
                );
                // A stop is reported while the child is still in the task
                // table, so its uid comes from the live identity rather than
                // from a record - and Linux fills it here too (measured).
                r.check(
                    "si_uid is the stopped child's uid",
                    unsafe { info.si_uid() } == unsafe { libc::getuid() },
                );
            }
            None => {
                r.check("a stopped child is reported by WSTOPPED", false);
                r.check("si_code says a stop", false);
                r.check("si_status is the stopping signal", false);
                r.check("si_uid is the stopped child's uid", false);
            }
        }

        // The resume: `SIGCONT` ends the group stop, and the parent hears it
        // only because it asked with `WCONTINUED`.
        unsafe {
            libc::kill(stopper, libc::SIGCONT);
        }
        match await_event(stopper, libc::WCONTINUED) {
            Some(info) => {
                r.check(
                    "a resumed child is reported by WCONTINUED",
                    info.si_code == libc::CLD_CONTINUED,
                );
                r.check(
                    "with the continuing signal as its status",
                    unsafe { info.si_status() } == libc::SIGCONT,
                );
                r.check(
                    "si_uid is the resumed child's uid",
                    unsafe { info.si_uid() } == unsafe { libc::getuid() },
                );
            }
            None => {
                r.check("a resumed child is reported by WCONTINUED", false);
                r.check("with the continuing signal as its status", false);
                r.check("si_uid is the resumed child's uid", false);
            }
        }

        let mut info = poisoned();
        r.check(
            "and it still exits with its own code",
            waitid(libc::P_PID, stopper, &mut info, libc::WEXITED) == 0
                && unsafe { info.si_status() } == 7,
        );
    }

    // `WNOWAIT`: report the event and leave it where it is, so the next wait
    // finds it again - Linux's "look before you collect". A caller that reaps
    // between the two would otherwise lose the record.
    let kept = child_that_exits(9);
    r.check("a child that exits 9 was forked", kept > 0);
    if kept > 0 {
        let mut first = poisoned();
        let answered = waitid(libc::P_PID, kept, &mut first, libc::WEXITED | libc::WNOWAIT);
        r.check(
            "WNOWAIT reports the exit",
            answered == 0 && unsafe { first.si_status() } == 9,
        );

        let mut second = poisoned();
        let answered = waitid(
            libc::P_PID,
            kept,
            &mut second,
            libc::WEXITED | libc::WNOWAIT,
        );
        r.check(
            "and the same exit is still there for the next look",
            answered == 0 && unsafe { second.si_pid() } == kept,
        );

        let mut collected = poisoned();
        r.check(
            "while a wait without WNOWAIT collects it",
            waitid(libc::P_PID, kept, &mut collected, libc::WEXITED) == 0
                && unsafe { collected.si_status() } == 9,
        );
    }

    // ---- the fifth argument: `struct rusage` -------------------------------
    //
    // `waitid(..., *rusage)` fills the caller's record for every event it
    // *reports* - an exit, a stop and a resume alike - and leaves the caller's
    // bytes alone when it reports nothing. Linux's own order is what shows in a
    // bad pointer: its wrapper copies the `struct rusage` **before** it writes
    // the six-word record, so an unwritable `rusage` is `EFAULT` with the info
    // record still the caller's own - where `wait4` has by then already written
    // `*status`. Measured, `.workbuddy/probe_wait4_rusage.c` scenarios E, H, I,
    // K, M and O, Linux 7.2.9. (Only the fill and the no-report half are pinned
    // here; `t-wait4` carries the CPU value and the pointer-refusal claims for
    // the shape both doors share, and the stop/resume records are above.)
    let worker = child_that_burns(150);
    r.check(
        "a CPU-burning child was forked for the rusage claim",
        worker > 0,
    );
    if worker > 0 {
        let mut info = poisoned();
        let mut ru = Rusage::seeded();
        let answered = waitid_rusage(libc::P_PID, worker, &mut info, libc::WEXITED, &mut ru);
        r.check(
            "waitid reports the exit when it was asked for a rusage",
            answered == 0 && unsafe { info.si_status() } == 0,
        );
        r.check(
            "and the record carries the child's CPU",
            ru.ru_utime.micros() > 0,
        );
    }

    // A call that reports *nothing* writes nothing, and `WNOHANG` finding the
    // child still running is that call - the zero a polling parent gets is not
    // an event (measured, scenario K).
    let napper = child_that_sleeps(1500);
    r.check(
        "a still-running child was forked for the WNOHANG claim",
        napper > 0,
    );
    if napper > 0 {
        let mut info = poisoned();
        let mut ru = Rusage::seeded();
        let answered = waitid_rusage(
            libc::P_PID,
            napper,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
            &mut ru,
        );
        r.check("WNOHANG with nothing to report answers 0", answered == 0);
        r.check(
            "and leaves the caller's rusage record untouched",
            !ru.written(),
        );
        // Collect it, so no later claim inherits a running child.
        let mut done = poisoned();
        let _ = waitid_rusage(
            libc::P_PID,
            napper,
            &mut done,
            libc::WEXITED,
            core::ptr::null_mut(),
        );
    }

    // ---- `si_uid` is the child's *real* id, and only an asymmetry makes that
    // ---- a claim
    //
    // Linux documents `si_uid` as "the real user ID of the process" and fills
    // it for every event class - measured on a current host with
    // `.workbuddy/probe_waitid.c`: `CLD_EXITED`, `CLD_TRACED`, `CLD_STOPPED`
    // and `CLD_CONTINUED` all carry the child's uid, while a call that faces no
    // event at all is a cleared record instead. The three checks above read it
    // off a child that changed nothing, which a record left at zero would also
    // pass.
    //
    // Moving an id does not fix that on its own: the payload runs as **root**,
    // so a child moved to some other uid would still be compared against the
    // zero the bug writes. What makes the difference is an *asymmetry* - a
    // process whose real and effective ids differ - and the payload can have
    // one, because root may move its own real id while keeping its effective
    // one: after the call below this program is `ruid = MOVED, euid = 0`, and
    // every child it forks from here on inherits exactly that. So a record that
    // reports the *real* uid answers `MOVED`, while one that reports the
    // effective id - or never filled the field - answers 0, which is the
    // counterfactual these claims are written against. Moving an id needs
    // `CAP_SETUID`, which the payload has and a hand host run often does not
    // (measured: this sandbox's shell has an empty effective capability set,
    // and its uid is already `MOVED`, so nothing here would move); the claims
    // are therefore made where they can be and named where they cannot, in
    // `t-posix`'s shape.
    //
    // Nothing has to exist under `MOVED` or `ELSEWHERE`: what is pinned is that
    // the *number* in the record is the one the child's real id became.
    const MOVED: u32 = 1000;
    const ELSEWHERE: u32 = 1234;

    if mpos_user_tests::host_run() {
        println!(
            "note: a child's uid cannot be moved without CAP_SETUID, so the si_uid claims below are not made on this host run"
        );
    } else {
        // `u32::MAX` is `setresuid`'s "-1": leave that id alone.
        let asymmetric = unsafe { libc::setresuid(MOVED, u32::MAX, u32::MAX) } == 0;
        r.check(
            "the program can move its own real uid (CAP_SETUID) and keep euid 0",
            asymmetric,
        );
        if asymmetric {
            // An ordinary child, whose inherited real id is `MOVED` and whose
            // effective id is root's: the real one is what the record owes.
            let inherited = child_with_ids(None, None);
            r.check("an asymmetric child was forked", inherited > 0);
            if inherited > 0 {
                let mut info = poisoned();
                r.check(
                    "si_uid is the child's real uid, not its effective one",
                    waitid(libc::P_PID, inherited, &mut info, libc::WEXITED) == 0
                        && unsafe { info.si_status() } == 0
                        && unsafe { info.si_uid() } == MOVED,
                );
            }

            // The same child moving its real id once more, so what is pinned is
            // that the field follows the real id rather than the value it started
            // at.
            let moved_again = child_with_ids(Some(ELSEWHERE), None);
            r.check(
                "a child that moves its real uid again was forked",
                moved_again > 0,
            );
            if moved_again > 0 {
                let mut info = poisoned();
                r.check(
                    "and the record follows it: the moved id is the one reported",
                    waitid(libc::P_PID, moved_again, &mut info, libc::WEXITED) == 0
                        && unsafe { info.si_status() } == 0
                        && unsafe { info.si_uid() } == ELSEWHERE,
                );
            }

            // The live path: a *stopped* child's record is written while it is
            // still in the task table, so its uid is asked of the task rather than
            // carried on a record - and it is the same real id.
            let stopped = child_with_ids_stopping(None);
            r.check("an asymmetric child that stops was forked", stopped > 0);
            if stopped > 0 {
                match await_event(stopped, libc::WSTOPPED) {
                    Some(info) => r.check(
                        "a stopped child's si_uid is its real id too",
                        info.si_code == libc::CLD_STOPPED && unsafe { info.si_uid() } == MOVED,
                    ),
                    None => r.check("a stopped child's si_uid is its real id too", false),
                }
                unsafe {
                    libc::kill(stopped, libc::SIGKILL);
                    let mut info = poisoned();
                    let _ = waitid(libc::P_PID, stopped, &mut info, libc::WEXITED);
                }
            }
        }
    }

    // ---- `__WCLONE` and `__WALL`: which kind of child this door may report ---
    //
    // The two bits narrow by the byte `clone` was given, and `waitid` reads them
    // the way `wait4` does - a "clone" child (born to report nothing or something
    // other than `SIGCHLD`) is `ECHILD` to a call that did not ask for that kind,
    // and for the kinds it *does* ask about the record is written exactly as
    // before. Measured on a current host first (`.workbuddy/probe_wclone.c`,
    // Linux 7.2.9: scenarios A and B are these four claims).
    const WCLONE: i32 = 0x8000_0000u32 as i32;
    const WALL: i32 = 0x4000_0000u32 as i32;

    /// A child started through raw `clone` with `sig` in the low byte: `fork`
    /// cannot choose that byte - it is `clone(SIGCHLD)` and nothing else.
    fn start_clone(sig: i32, code: i32) -> libc::pid_t {
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone,
                sig as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        if pid != 0 {
            return pid as libc::pid_t;
        }
        unsafe {
            // `exit_group`, so this child does not flush the stdout copy it
            // inherited.
            libc::syscall(libc::SYS_exit_group, code as libc::c_long);
            libc::_exit(code)
        }
    }

    // A forked child is a non-clone child, and `__WCLONE` alone does not admit
    // it - while `__WALL` does. One child, two calls, so the pair differs in the
    // option word and nothing else.
    let wchild = child_that_exits(6);
    r.check(
        "a forked child for the __WCLONE claims was forked",
        wchild > 0,
    );
    if wchild > 0 {
        sleep_ms(60);
        let mut info = poisoned();
        r.check(
            "waitid(P_ALL, WEXITED|__WCLONE) is ECHILD for a SIGCHLD child",
            waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | WCLONE) == -(libc::ECHILD as isize),
        );
        let mut info = poisoned();
        r.check(
            "while __WALL reports it, with its code",
            waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | WALL) == 0
                && unsafe { info.si_pid() } == wchild
                && unsafe { info.si_status() } == 6,
        );
    }

    // A clone(0) child: invisible without `__WCLONE`, reported with it - and
    // `WNOHANG` answers `ECHILD` rather than the "nothing yet" zero, because the
    // child is not one this call could ever hear.
    let kchild = start_clone(0, 8);
    r.check("a clone(0) child was started", kchild > 0);
    if kchild > 0 {
        sleep_ms(60);
        let mut info = poisoned();
        r.check(
            "waitid(P_ALL, WEXITED|WNOHANG) is ECHILD for a clone child",
            waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | libc::WNOHANG)
                == -(libc::ECHILD as isize),
        );
        let mut info = poisoned();
        r.check(
            "__WCLONE reports it, with its own code",
            waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | WCLONE) == 0
                && unsafe { info.si_pid() } == kchild
                && unsafe { info.si_status() } == 8,
        );
    }

    // ---- where the two writers go when one of them cannot ------------------
    //
    // Both of this door's writes land in the caller, and Linux's order between
    // them and the wait itself is visible only to a caller that hands over a
    // pointer the kernel cannot write to: `do_waitid` collects (or refuses)
    // inside `kernel_waitid`, copies the `struct rusage` when it has an event to
    // report, and writes the six-word record **last** - on every path, refusal
    // included, with the copy's own `EFAULT` outranking whatever the wait said.
    // Measured whole on a current host before these claims were written
    // (`.workbuddy/probe_waitid_record.c`, Linux 7.2.9, scenarios P-Z, AA-AG),
    // because a success path cannot see any of it.

    /// [`waitid_rusage`] with the record spelled as a raw address, so a claim can
    /// hand over one the kernel must refuse.
    fn waitid_raw(idtype: u32, id: i32, infop: usize, options: i32, rusage: *mut Rusage) -> isize {
        let r = unsafe {
            libc::syscall(
                libc::SYS_waitid,
                idtype as libc::c_long,
                id as libc::c_long,
                infop as libc::c_long,
                options as libc::c_long,
                rusage as libc::c_long,
            )
        };
        if r < 0 {
            -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
        } else {
            r as isize
        }
    }

    /// A page this program owns and cannot write - the pointer the kernel is
    /// owed a refusal for. Zero when the mapping failed.
    fn unwritable_page() -> usize {
        let p = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                PAGE,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED { 0 } else { p as usize }
    }

    /// Two pages with the second one `PROT_NONE`, answered as the *boundary*
    /// between them: a record laid down just before it has a mapped head and an
    /// unmapped tail, which is the only way to see whether this door checks the
    /// record or each word of it. Zero when the mapping failed.
    fn page_boundary() -> usize {
        let p = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                2 * PAGE,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return 0;
        }
        if p as usize % PAGE != 0 {
            return 0;
        }
        let rc = unsafe { libc::mprotect(p, PAGE, libc::PROT_READ | libc::PROT_WRITE) };
        if rc != 0 {
            return 0;
        }
        p as usize + PAGE
    }

    /// Every byte of a record, for the claims about the bytes a write did not
    /// reach.
    fn record_bytes(info: &libc::siginfo_t) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                (info as *const libc::siginfo_t).cast::<u8>(),
                size_of::<libc::siginfo_t>(),
            )
        }
    }

    const PAGE: usize = 4096;

    // (1) A `rusage` the kernel cannot write: the refusal comes *after* the
    // child is collected, and leaves the record exactly as the caller wrote it -
    // the record is written last, so it was never reached.
    let poison_target = child_that_exits(6);
    r.check(
        "a child was forked for the unwritable-rusage claim",
        poison_target > 0,
    );
    if poison_target > 0 {
        sleep_ms(60);
        let mut info = poisoned();
        let answered = waitid_raw(
            libc::P_PID,
            poison_target,
            (&raw mut info) as usize,
            libc::WEXITED,
            1usize as *mut Rusage,
        );
        r.check(
            "an unwritable rusage is EFAULT",
            answered == -(libc::EFAULT as isize),
        );
        r.check(
            "and leaves the record as the caller wrote it, because the record is written last",
            record_bytes(&info).iter().all(|&b| b == 0xFF),
        );
        let mut after = poisoned();
        r.check(
            "while the child it named is collected anyway",
            waitid(
                libc::P_PID,
                poison_target,
                &mut after,
                libc::WEXITED | libc::WNOHANG,
            ) == -(libc::ECHILD as isize),
        );
    }

    // (2) A record the kernel cannot write: `EFAULT`, with the rusage already
    // filled and the child already collected - the other half of the same order.
    let guard = unwritable_page();
    r.check(
        "a PROT_NONE page was mapped for the record claims",
        guard != 0,
    );
    let unrecorded = child_that_exits(6);
    r.check(
        "a child was forked for the unwritable-record claim",
        unrecorded > 0,
    );
    if unrecorded > 0 && guard != 0 {
        sleep_ms(60);
        let mut ru = Rusage::seeded();
        let answered = waitid_raw(libc::P_PID, unrecorded, guard, libc::WEXITED, &mut ru);
        r.check(
            "an unwritable record is EFAULT",
            answered == -(libc::EFAULT as isize),
        );
        r.check(
            "with the rusage already filled, because it is written first",
            ru.written(),
        );
        let mut after = poisoned();
        r.check(
            "and the child collected before either refusal",
            waitid(
                libc::P_PID,
                unrecorded,
                &mut after,
                libc::WEXITED | libc::WNOHANG,
            ) == -(libc::ECHILD as isize),
        );
    }

    // (3) A refusal still writes the record, and writes it as six zeros: the
    // words `waitid` owns go to zero while the caller's other bytes - the pad at
    // 12 and everything from 28 on - stay its own. This is what a whole-record
    // clear gets wrong, and it is the shape a caller can *see*.
    let refused = child_that_exits(6);
    r.check("a child was forked for the refusal claim", refused > 0);
    if refused > 0 {
        sleep_ms(60);
        let mut info = poisoned();
        let answered = waitid_raw(
            libc::P_PID,
            refused,
            (&raw mut info) as usize,
            0x4000,
            core::ptr::null_mut(),
        );
        r.check(
            "a call that asks for no event class is EINVAL",
            answered == -(libc::EINVAL as isize),
        );
        r.check(
            "and writes the record's six words as zeros",
            info.si_signo == 0
                && info.si_errno == 0
                && info.si_code == 0
                && unsafe { info.si_pid() } == 0
                && unsafe { info.si_uid() } == 0
                && unsafe { info.si_status() } == 0,
        );
        r.check(
            "while the pad at 12 and the tail from 28 stay the caller's",
            record_bytes(&info)[12..16].iter().all(|&b| b == 0xFF)
                && record_bytes(&info)[28..].iter().all(|&b| b == 0xFF),
        );
        let mut after = poisoned();
        r.check(
            "the refusal collected nothing",
            waitid(libc::P_PID, refused, &mut after, libc::WEXITED) == 0
                && unsafe { after.si_status() } == 6,
        );

        // (4) The record's refusal outranks the wait's own: the same refused
        // word with a record that cannot be written is `EFAULT`, not `EINVAL`.
        if guard != 0 {
            r.check(
                "an unwritable record outranks the refusal",
                waitid_raw(libc::P_PID, refused, guard, 0x4000, core::ptr::null_mut())
                    == -(libc::EFAULT as isize),
            );
        }
    }
    if guard != 0 {
        r.check(
            "and outranks ECHILD, which has no child to collect at all",
            waitid_raw(
                libc::P_PID,
                4_000_000,
                guard,
                libc::WEXITED,
                core::ptr::null_mut(),
            ) == -(libc::EFAULT as isize),
        );
    }

    // (5) A null record is legal - "I only wanted the reap" - and collects.
    let reaped = child_that_exits(6);
    r.check("a child was forked for the null-record claim", reaped > 0);
    if reaped > 0 {
        sleep_ms(60);
        r.check(
            "a null record is accepted and answers 0",
            waitid_raw(libc::P_PID, reaped, 0, libc::WEXITED, core::ptr::null_mut()) == 0,
        );
        let mut after = poisoned();
        r.check(
            "and it collected the child",
            waitid(
                libc::P_PID,
                reaped,
                &mut after,
                libc::WEXITED | libc::WNOHANG,
            ) == -(libc::ECHILD as isize),
        );
    }

    // (6) `WNOHANG` with nothing to report is still a record to write - the
    // "nothing yet" zero - so an unwritable record is `EFAULT` here too.
    let napper2 = child_that_sleeps(300);
    r.check(
        "a child was forked for the WNOHANG record claim",
        napper2 > 0,
    );
    if napper2 > 0 && guard != 0 {
        r.check(
            "WNOHANG with nothing to report and an unwritable record is EFAULT",
            waitid_raw(
                libc::P_PID,
                napper2,
                guard,
                libc::WEXITED | libc::WNOHANG,
                core::ptr::null_mut(),
            ) == -(libc::EFAULT as isize),
        );
        let mut done = poisoned();
        let _ = waitid(libc::P_PID, napper2, &mut done, libc::WEXITED);
    }

    // (7) The check is per *word*, not for the record: a record whose tail is
    // unmapped but whose six words all land is written and answers 0 - and one
    // whose third word straddles the boundary is `EFAULT` with the two words
    // before it already written. Both are Linux, and both are what tells a
    // word-at-a-time writer from a range-checked one.
    let edge = page_boundary();
    r.check("two pages were mapped for the boundary claims", edge != 0);
    if edge != 0 {
        let straddled = child_that_exits(6);
        r.check("a child was forked for the boundary claim", straddled > 0);
        if straddled > 0 {
            sleep_ms(60);
            // 64 bytes of record in the writable page, 64 past the boundary.
            let at = edge - 64;
            unsafe { core::ptr::write_bytes(at as *mut u8, 0xFF, 64) };
            let answered = waitid_raw(
                libc::P_PID,
                straddled,
                at,
                libc::WEXITED,
                core::ptr::null_mut(),
            );
            r.check(
                "a record whose tail is unmapped is still written, because every word it writes landed",
                answered == 0,
            );
            r.check(
                "with the child's own pid in it",
                unsafe { core::ptr::read_unaligned(at as *const i32) } == 17
                    && unsafe { core::ptr::read_unaligned((at + 16) as *const i32) } == straddled,
            );
            r.check(
                "and the caller's bytes past the six words untouched",
                (28..64).all(|i| unsafe { *((at + i) as *const u8) } == 0xFF),
            );
        }

        let split = child_that_exits(6);
        r.check("a child was forked for the straddling claim", split > 0);
        if split > 0 {
            sleep_ms(60);
            // `si_code` at +8 lands with two bytes on each side of the boundary.
            let at = edge - 11;
            unsafe { core::ptr::write_bytes(at as *mut u8, 0xFF, 11) };
            let answered = waitid_raw(libc::P_PID, split, at, libc::WEXITED, core::ptr::null_mut());
            r.check(
                "a word that straddles the boundary stops the record there",
                answered == -(libc::EFAULT as isize),
            );
            r.check(
                "with the words before it already written",
                unsafe { core::ptr::read_unaligned(at as *const i32) } == 17
                    && unsafe { core::ptr::read_unaligned((at + 4) as *const i32) } == 0,
            );
            r.check(
                "and the word that could not be written left alone",
                (8..11).all(|i| unsafe { *((at + i) as *const u8) } == 0xFF),
            );
        }
    }

    // (8) The rusage is copied the same way, and it is Linux's `copy_to_user`
    // rather than a check-then-write: a range copy writes the pages it can and
    // answers `EFAULT` for the one it cannot, so a record that straddles into
    // the unmapped page leaves its mapped head written (scenario AH).
    if edge != 0 {
        let billed = child_that_exits(6);
        r.check(
            "a child was forked for the straddling-usage claim",
            billed > 0,
        );
        if billed > 0 {
            sleep_ms(60);
            // 64 bytes of the 144 in the writable page, the rest past it.
            let at = edge - 64;
            unsafe { core::ptr::write_bytes(at as *mut u8, 0xAA, 64) };
            let answered = waitid_raw(libc::P_PID, billed, 0, libc::WEXITED, at as *mut Rusage);
            r.check(
                "a rusage that straddles the boundary is EFAULT",
                answered == -(libc::EFAULT as isize),
            );
            r.check(
                "with its mapped head already written, because the copy is a range",
                (0..64).any(|i| unsafe { *((at + i) as *const u8) } != 0xAA),
            );
            let mut after = poisoned();
            r.check(
                "and the child collected before that refusal too",
                waitid(
                    libc::P_PID,
                    billed,
                    &mut after,
                    libc::WEXITED | libc::WNOHANG,
                ) == -(libc::ECHILD as isize),
            );
        }
    }

    r.finish()
}
