//! t-ioprio - `ioprio_get(2)`/`ioprio_set(2)`, the block-I/O priority word.
//!
//! Neither number was declared, so the dispatch answered `ENOSYS`. That is the
//! honest answer to an unknown number and the wrong one for the block layer's
//! own ABI: `ionice(1)` is the caller everybody has, and a program that sets a
//! class and reads it back must see what it set.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{iop,iop2,iop3,iop5,iop6,
//! iop7}.c`) before any claim:
//!
//! | what | Linux |
//! |---|---|
//! | `which` values | **1** process, **2** group, **3** user; `0` and `4` are `EINVAL` |
//! | a fresh process | reads **0** (`IOPRIO_CLASS_NONE`) |
//! | a `who` naming nothing | `ESRCH` |
//! | the four classes | `NONE`(0), `RT`(1), `BE`(2), `IDLE`(3) all record and read back |
//! | class 4..7 | `EINVAL` |
//! | data above the class's nominal max | **stored** (`BE/8` reads back `BE/8`) |
//! | bits above the 16-bit word | **truncated**, not refused |
//! | `RT` without `CAP_SYS_NICE` | `EPERM` |
//! | a group/user query | the **strongest** word any member holds |
//! | a child | inherits the parent's word |
//!
//! The last two are the interesting ones. "Strongest" is not a plain max: the
//! **class** is the highest any member has and the **data is the lowest number
//! within that class** - measured, a parent at `BE/7` beside a child at `BE/1`
//! answers `BE/1`.
//!
//! Numbers from libc's table: `rustix` has no `ioprio` binding at all (clause
//! (a), a call it does not write).

use mpos_user_tests::Report;

const WHO_PROCESS: i32 = 1;
const WHO_PGRP: i32 = 2;
const WHO_USER: i32 = 3;

const CLASS_SHIFT: u32 = 13;
const CLASS_NONE: u32 = 0;
const CLASS_RT: u32 = 1;
const CLASS_BE: u32 = 2;
const CLASS_IDLE: u32 = 3;

fn word(class: u32, data: u32) -> u32 {
    (class << CLASS_SHIFT) | data
}

fn class_of(w: i64) -> u32 {
    ((w as u32) >> CLASS_SHIFT) & 7
}
fn data_of(w: i64) -> u32 {
    (w as u32) & 0x1fff
}

fn get(which: i32, who: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_ioprio_get, which, who) as i64 }
}
fn set(which: i32, who: i32, w: u32) -> i64 {
    unsafe { libc::syscall(libc::SYS_ioprio_set, which, who, w) as i64 }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: i64, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}

fn main() {
    println!("t-ioprio");
    let mut r = Report::new();
    let pid = unsafe { libc::getpid() };

    // The fixture is the **caller's own** priority, restored at the end, because
    // this word is per-task state and a claim that left it set would change what
    // the next program in the lane reads.
    let original = get(WHO_PROCESS, pid);
    r.check(
        &format!("the caller's word is readable ({original:#x})"),
        original >= 0,
    );

    // === the word's shape ==================================================
    r.check(
        &format!("a fresh process reads IOPRIO_CLASS_NONE (0), got {original:#x}"),
        class_of(original) == CLASS_NONE,
    );
    for class in [CLASS_NONE, CLASS_BE, CLASS_IDLE] {
        let want = word(class, 0);
        let set_rc = set(WHO_PROCESS, pid, want);
        let got = get(WHO_PROCESS, pid);
        r.check(
            &format!("class {class} records and reads back ({set_rc}, {got:#x})"),
            set_rc == 0 && class_of(got) == class,
        );
    }
    // `RT` is the one class with teeth: Linux asks `CAP_SYS_NICE` for it and
    // answers `EPERM` to a caller without. Measured on the host
    // (`iop2.c`: `class 1 word 0x2000` -> `EPERM`) and in the guest the payload
    // is root, so both answers are *correct* - the claim is that it is one of
    // them, and that the refusal is `EPERM` rather than `EINVAL`.
    let rt = set(WHO_PROCESS, pid, word(CLASS_RT, 0));
    r.check(
        &format!("class RT is served for a capable caller or EPERM for one without ({rt})"),
        rt == 0 || refused(rt, libc::EPERM),
    );
    // The data is **not** validated against the class's nominal range: `BE/8`
    // is stored, even though best-effort stops at 7 on paper.
    r.check("BE data 8 is stored, not clamped", {
        set(WHO_PROCESS, pid, word(CLASS_BE, 8));
        data_of(get(WHO_PROCESS, pid)) == 8
    });
    r.check(
        "and the class alongside it is BE",
        class_of(get(WHO_PROCESS, pid)) == CLASS_BE,
    );
    for bad in 4..=7u32 {
        r.check(
            &format!("class {bad} is EINVAL"),
            refused(set(WHO_PROCESS, pid, bad << CLASS_SHIFT), libc::EINVAL),
        );
    }
    // Bits above the sixteen-bit word are **truncated**: measured, `1 << 16`
    // through `1 << 19` are served and `0x1c000` stores as `0x4000`.
    // The truncation claim needs a value whose **low** sixteen bits name a valid
    // class: `0x1_c000` truncates to `0xc000`, class 6, which is `EINVAL` - the
    // first draft used it and measured the class rule rather than the
    // truncation. `0x1_4000` truncates to `0x4000`, `BE/0`.
    r.check(
        "bits above the word are truncated, not refused",
        set(WHO_PROCESS, pid, 0x1_4000) == 0 && get(WHO_PROCESS, pid) == 0x4000,
    );

    // === the `which` values ================================================
    for bad in [0i32, 4, -1] {
        r.check(
            &format!("which={bad} is EINVAL"),
            refused(get(bad, 0), libc::EINVAL),
        );
    }
    r.check(
        &format!("which={WHO_PROCESS} with who=0 means the caller"),
        get(WHO_PROCESS, 0) == get(WHO_PROCESS, pid),
    );
    r.check(
        "a who that names nothing is ESRCH",
        refused(get(WHO_PROCESS, 99999), libc::ESRCH),
    );

    // === the group/user shapes answer the strongest member ==================
    //
    // Measured, and it is a min over the data rather than a max: `BE/7` beside
    // `BE/1` answers `BE/1`.
    //
    // **The claim runs in a session of its own**, and that is not tidiness: a
    // group query covers *every* member, and the sweep runs this program as one
    // of dozens in one lane's process group. A first draft asked
    // `get(PGRP, 0)` in place and read `0x4004` - some other program in the lane
    // had set `BE/4`, which is a *stronger* word than the fixture's `BE/7`, and
    // the answer was correct about a membership this test did not mean to name.
    // `setsid` gives the fixture a group of exactly two, so the rule is the only
    // thing that can decide the answer.
    // Two pipes: the child says "I have set my word", the parent says "you may
    // go". A first draft reused one pipe for both directions - the child read
    // the end the parent had already closed and blocked for ever, and the
    // claim timed out rather than measuring anything. A pipe end has one
    // direction; two questions need two pipes.
    // The group fixture runs in a **session of its own**, because a group query
    // covers every member and the sweep runs this program as one of dozens in
    // one lane's process group: a first draft asked `get(PGRP, 0)` in place and
    // read `BE/4` - some other program in the lane had set a word *stronger*
    // than the fixture's own, and the answer was right about a membership this
    // test did not mean to name.
    //
    // The sync is the C probe's own (`build/scratch/opathmmap/iop8.c`, which is
    // where the rule was measured): a short sleep in the parent, which is long
    // enough for a child to have run one syscall. Two pipe-based drafts of this
    // were **worse**, not better - the first reused one pipe in both directions
    // and the child blocked on an end its parent had closed; the second
    // announced with a byte and still lost the race, because the parent's read
    // returned before the child's `ioprio_set` was visible. A sleep that is
    // generous by two orders of magnitude is the honest tool for "let the child
    // run once", and it is what the measurement itself used.
    let fixture = |body: &dyn Fn() -> bool| -> bool {
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::setsid();
                libc::_exit(if body() { 0 } else { 1 });
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        (status >> 8) & 0xff == 0
    };
    r.check(
        "a group query answers the strongest member (BE/1 over BE/7)",
        fixture(&|| {
            let own = unsafe { libc::getpid() };
            set(WHO_PROCESS, own, word(CLASS_BE, 7));
            let child = unsafe { libc::fork() };
            if child == 0 {
                set(WHO_PROCESS, unsafe { libc::getpid() }, word(CLASS_BE, 1));
                unsafe {
                    libc::pause();
                    libc::_exit(0);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
            let seen = get(WHO_PGRP, 0);
            unsafe {
                libc::kill(child, libc::SIGKILL);
                let mut st = 0;
                libc::waitpid(child, &mut st, 0);
            }
            class_of(seen) == CLASS_BE && data_of(seen) == 1
        }),
    );

    // The **user** shape asks the same membership rule over a wider set - every
    // task of the caller's uid - so its answer is at least as strong as a single
    // process's own and never weaker than the group's. It is compared against the
    // caller's own word rather than a fixture value, because the set includes
    // processes this program did not make.
    let uid = unsafe { libc::getuid() } as i32;
    let by_user = get(WHO_USER, uid);
    r.check(
        &format!("a user query is served and at least the caller's own strength ({by_user:#x})"),
        by_user >= 0 && class_of(by_user) >= class_of(get(WHO_PROCESS, 0)),
    );
    // A uid nobody holds has no members, which is the `ESRCH` the other two
    // shapes give for a `who` naming nothing.
    let nobody = 4294967294u32 as i32;
    r.check(
        &format!("a user nobody holds is ESRCH ({})", get(WHO_USER, nobody)),
        refused(get(WHO_USER, nobody), libc::ESRCH),
    );

    // === inheritance =======================================================
    r.check(
        "a child inherits the parent's word",
        set(WHO_PROCESS, pid, word(CLASS_IDLE, 2)) == 0 && {
            let child = unsafe { libc::fork() };
            if child == 0 {
                let mine = get(WHO_PROCESS, unsafe { libc::getpid() });
                unsafe {
                    libc::_exit(if mine == word(CLASS_IDLE, 2) as i64 {
                        0
                    } else {
                        1
                    })
                };
            }
            let mut st = 0;
            unsafe { libc::waitpid(child, &mut st, 0) };
            (st >> 8) & 0xff == 0
        },
    );

    // Restore what the caller had, so the next program in the lane sees the
    // word it would have seen without this test.
    set(WHO_PROCESS, pid, original.max(0) as u32);
    r.finish()
}
