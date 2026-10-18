//! t-clone-exit - the *signal* a `clone` child's death delivers to its parent.
//!
//! `clone(2)`'s low byte is not a flag: it is the signal the child's death
//! should report. `fork` is `clone(SIGCHLD)`; a raw `clone(SIGTERM)` child's
//! death arrives as a `SIGTERM` with the exit's fields, and a `clone(0)` child
//! reports **nothing at all**. `t-wait4` pins the other end of the same byte -
//! which *kind* of child a `wait4(__WCLONE|__WALL)` may be answered about - and
//! this program pins the delivery, the half `docs/uapi/` §5.2 named until now.
//!
//! # The rule, and why it is not one rule
//!
//! Whether the parent is told is the parent's own disposition and mask, and the
//! answer differs between `SIGCHLD` and every other signal, so neither half
//! alone is the rule. Measured first, one scenario per disposition, on a host
//! (`.workbuddy/probe_exitsig.c`, Linux 7.2.11), the child's byte the only
//! variable:
//!
//!   * **byte `0`**: nothing, whatever the parent's mask.
//!   * **byte `SIGCHLD`** (17): queued when the parent has a handler *or* has
//!     it **blocked**, never when it is `SIG_IGN` - a blocked, ignored
//!     `SIGCHLD` is still dropped, which is the one place "a blocked signal is
//!     never ignored" does not hold.
//!   * **any other signal**: queued when the parent has a handler, has it
//!     blocked, **or** its default action is not one Linux ignores
//!     (`SIGCHLD`, `SIGURG`, `SIGWINCH`). So a blocked, ignored
//!     `SIGTERM` **is** delivered - the opposite of `SIGCHLD` - and a
//!     `SIG_DFL`, unblocked parent is **killed** by its `clone(SIGTERM)`
//!     child's death. `SIGCONT` is one of these and not an ignored-by-default
//!     signal, which is measured the hard way: `SIGCONT`'s default is *resume*,
//!     no-op-looking for a running parent, but a `clone(SIGCONT)` child's death
//!     *resumes a stopped* parent on Linux (`.workbuddy/probe_exitsig_cont.c`,
//!     where the other probe's J row cannot tell the two apart).
//!
//! # The record
//!
//! `si_signo` is the exit signal, `si_code` is `CLD_EXITED`, `si_pid` the
//! child, `si_uid` the child's **real** uid and `si_status` the exit code -
//! the same fields a `SIGCHLD` record has, only under the number the child
//! chose. The read door is `rt_sigtimedwait` with the exit signal blocked,
//! which consumes the record without running a handler. On the machine that is
//! also the door `t-signalfd` and `t-waitid` use, shared through
//! `hal::SignalInfo::chld_union`, so a record written with the right number and
//! the wrong union would be caught by the `si_status` claims below.
//!
//! # Why `libc` and not `rustix`
//!
//! Two calls are outside `rustix`'s reach and both are its rule's first clause
//! (a call it cannot be relied on to bind): raw `clone` - `rustix` binds no
//! `clone` a test can put a signal byte into - and `rt_sigtimedwait`, which is
//! `rustix`'s doc-hidden `runtime` module on this crate's feature set, exactly
//! as `t-wait4`'s header records. `libc::sigtimedwait` exists but takes a
//! `*mut crate::siginfo_t`, which this libc does not define for linux, so the
//! record is spelled here - the same 128 bytes the kernel writes, the same
//! offsets `handle_rt_sigtimedwait` fills.

use mpos_user_tests::Report;

/// The kernel's `siginfo_t`, the 128-byte record `rt_sigtimedwait` fills.
///
/// The three header words, then the `_sigchld` member of the union at offset
/// 16: `si_pid`/`si_uid` at 16/20, `si_status` at 24, and the two 8-byte
/// `clock_t`s at 32/40 - the layout `handle_rt_sigtimedwait` writes and the one
/// a handler's frame uses. Spelled here rather than taken from `libc`, which
/// does not define `siginfo_t` for linux at all.
#[repr(C)]
#[derive(Clone, Copy)]
struct Info {
    signo: i32,
    _errno: i32,
    code: i32,
    _pad0: i32,
    pid: i32,
    uid: u32,
    status: i32,
    _pad1: i32,
    _utime: i64,
    _stime: i64,
    _rest: [u8; 80],
}

/// The record is the kernel's 128 bytes, because the kernel writes all of it -
/// a shorter buffer would be written past its end.
const _: () = assert!(core::mem::size_of::<Info>() == 128);

/// The two `__W*` bits `wait4` reads, Linux's own private spelling rather than
/// the man page's: `__WCLONE` admits a "clone" child (born to report anything
/// but `SIGCHLD`), and `__WALL` admits both kinds. `t-wait4` spells them too.
const WCLONE: i32 = 0x8000_0000u32 as i32;

/// Start a child through raw `clone` with `sig` in the low byte, with a fresh
/// address space (no `CLONE_VM`), that immediately `exit_group`s with `code`.
///
/// Raw `clone` rather than `libc::fork` because `fork` cannot choose the byte -
/// it is always `SIGCHLD`. The byte is the first argument on both machines;
/// every argument after it is zero, which is what makes the two machines'
/// different orderings of `tls`/`child_tid` irrelevant. `exit_group` rather
/// than `_exit` so a child that inherited the parent's buffered stdout cannot
/// flush a second copy of it. Returns the child's pid, or a negative number.
fn spawn(sig: libc::c_int, code: libc::c_int) -> i32 {
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
        return pid as i32;
    }
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// A one-signal set, for both the block mask and the wait.
fn one(sig: libc::c_int) -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
    }
    set
}

/// Block `sig`, returning the mask to put back.
fn block(sig: libc::c_int) -> libc::sigset_t {
    let mut old: libc::sigset_t = unsafe { core::mem::zeroed() };
    let set = one(sig);
    unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old) };
    old
}

/// Put a mask returned by [`block`] back.
fn restore(old: &libc::sigset_t) {
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, old, core::ptr::null_mut()) };
}

/// Read one pending record for `set`, or `None` on timeout (`EAGAIN`).
///
/// The `sigsetsize` of 8 is Linux's kernel-side `sigset_t` and not this libc's
/// 128-byte one: `rt_sigtimedwait` answers `EINVAL` for anything else, which is
/// why the set is built with musl's helpers but passed as the kernel's word.
fn take(set: &libc::sigset_t) -> Option<Info> {
    let ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 40_000_000,
    };
    let mut info: Info = unsafe { core::mem::zeroed() };
    let r = unsafe {
        libc::syscall(
            libc::SYS_rt_sigtimedwait,
            set as *const libc::sigset_t as libc::c_long,
            &mut info as *mut Info as libc::c_long,
            &ts as *const libc::timespec as libc::c_long,
            8 as libc::c_long,
        )
    };
    (r >= 0).then_some(info)
}

/// `wait4(pid, *status, options, NULL)`: the answer and the status word.
fn wait4_opts(pid: i32, options: i32) -> (isize, i32) {
    let mut status: i32 = 0;
    let answer = unsafe {
        libc::syscall(
            libc::SYS_wait4,
            pid as libc::c_long,
            &mut status as *mut i32 as libc::c_long,
            options as libc::c_long,
            0 as libc::c_long,
        )
    };
    (answer as isize, status)
}

/// The errno the last syscall left, if it failed.
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}

fn main() {
    println!("t-clone-exit");
    let mut r = Report::new();
    let me = unsafe { libc::getuid() };

    // ---- the signal the byte names -----------------------------------------
    //
    // `clone(SIGTERM)` with SIGTERM blocked: the death arrives as a SIGTERM
    // record carrying the exit's fields, and the child is still a "clone" kind
    // to a `wait4` that did not ask for one.
    let old = block(libc::SIGTERM);
    let set = one(libc::SIGTERM);
    let pid = spawn(libc::SIGTERM, 7);
    r.check("a clone(SIGTERM) child was started", pid > 0);
    if pid > 0 {
        let rec = take(&set);
        r.check(
            "its death delivers a SIGTERM, not a SIGCHLD",
            rec.map(|i| i.signo) == Some(libc::SIGTERM),
        );
        r.check(
            "with CLD_EXITED in si_code",
            rec.map(|i| i.code) == Some(libc::CLD_EXITED),
        );
        r.check(
            "the child's exit code in si_status",
            rec.map(|i| i.status) == Some(7),
        );
        r.check("the child's pid in si_pid", rec.map(|i| i.pid) == Some(pid));
        r.check(
            "the child's real uid in si_uid",
            rec.map(|i| i.uid) == Some(me),
        );
        // The kind is unchanged by the delivery rule: a plain wait must not see
        // a clone child, while `__WCLONE` collects it.
        let (answer, _) = wait4_opts(pid, libc::WNOHANG);
        r.check(
            "a plain wait4 still does not see a clone child (ECHILD)",
            answer == -1 && errno() == Some(libc::ECHILD),
        );
        let (answer, status) = wait4_opts(pid, WCLONE);
        r.check(
            "__WCLONE still collects it, with its exit code",
            answer == pid as isize && (status >> 8) & 0xff == 7,
        );
    }
    restore(&old);

    // `clone(0)`: the child asked to be announced by nothing, and gets that
    // even though SIGCHLD is blocked - the spurious SIGCHLD this kernel used to
    // raise.
    let old = block(libc::SIGCHLD);
    let set = one(libc::SIGCHLD);
    let pid = spawn(0, 7);
    r.check("a clone(0) child was started", pid > 0);
    if pid > 0 {
        r.check(
            "clone(0) delivers nothing at all, even with SIGCHLD blocked",
            take(&set).is_none(),
        );
        let (answer, _) = wait4_opts(pid, WCLONE);
        r.check("while __WCLONE still collects it", answer == pid as isize);
    }
    restore(&old);

    // The control: the `SIGCHLD` byte keeps its own rule - blocked, it is one
    // record - and its child is a "non-clone" a plain wait collects.
    let old = block(libc::SIGCHLD);
    let set = one(libc::SIGCHLD);
    let pid = spawn(libc::SIGCHLD, 7);
    r.check("a clone(SIGCHLD) child was started", pid > 0);
    if pid > 0 {
        let rec = take(&set);
        r.check(
            "clone(SIGCHLD) blocked still delivers SIGCHLD",
            rec.map(|i| i.signo) == Some(libc::SIGCHLD),
        );
        r.check(
            "with the same exit fields",
            rec.map(|i| i.status) == Some(7) && rec.map(|i| i.pid) == Some(pid),
        );
        let (answer, _) = wait4_opts(pid, 0);
        r.check(
            "and a plain wait4 collects a non-clone child",
            answer == pid as isize,
        );
    }
    restore(&old);

    // ---- the parent's disposition decides ----------------------------------
    //
    // An ignored SIGTERM death delivers nothing while unblocked...
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    let set = one(libc::SIGTERM);
    let pid = spawn(libc::SIGTERM, 7);
    r.check(
        "a clone(SIGTERM) child for the ignore claims was started",
        pid > 0,
    );
    if pid > 0 {
        r.check(
            "an ignored SIGTERM death delivers nothing when unblocked",
            take(&set).is_none(),
        );
        let _ = wait4_opts(pid, WCLONE);
    }
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_DFL) };

    // ... but a *blocked*, ignored SIGTERM **is** delivered - the divergence
    // from the SIGCHLD rule, where a blocked ignore is still dropped.
    let old = block(libc::SIGTERM);
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    let set = one(libc::SIGTERM);
    let pid = spawn(libc::SIGTERM, 7);
    r.check(
        "a clone(SIGTERM) child for the blocked-ignore claim was started",
        pid > 0,
    );
    if pid > 0 {
        r.check(
            "a blocked, ignored SIGTERM death **is** delivered",
            take(&set).map(|i| i.signo) == Some(libc::SIGTERM),
        );
        let _ = wait4_opts(pid, WCLONE);
    }
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_DFL) };
    restore(&old);

    // A signal whose default Linux ignores (SIGURG): dropped at SIG_DFL
    // unblocked...
    let set = one(libc::SIGURG);
    let pid = spawn(libc::SIGURG, 7);
    r.check("a clone(SIGURG) child was started", pid > 0);
    if pid > 0 {
        r.check(
            "a default-ignored exit signal (SIGURG) delivers nothing unblocked",
            take(&set).is_none(),
        );
        let _ = wait4_opts(pid, WCLONE);
    }

    // ... and delivered, under its *own* number, once blocked.
    let old = block(libc::SIGURG);
    let set = one(libc::SIGURG);
    let pid = spawn(libc::SIGURG, 7);
    r.check(
        "a clone(SIGURG) child was started for the blocked claim",
        pid > 0,
    );
    if pid > 0 {
        r.check(
            "blocked, the death arrives as SIGURG - its own number, not SIGCHLD",
            take(&set).map(|i| i.signo) == Some(libc::SIGURG),
        );
        let _ = wait4_opts(pid, WCLONE);
    }
    restore(&old);

    // `SIGCONT` is *not* an ignored-by-default signal, whatever its `resume`
    // default suggests: blocked, a clone(SIGCONT) child's death is delivered
    // under its own number. (Its unblocked shape is the one a running parent
    // cannot see - a delivered SIGCONT is a no-op - which is why the stopped
    // case is the measurement: `.workbuddy/probe_exitsig_cont.c`.)
    let old = block(libc::SIGCONT);
    let set = one(libc::SIGCONT);
    let pid = spawn(libc::SIGCONT, 7);
    r.check("a clone(SIGCONT) child was started", pid > 0);
    if pid > 0 {
        r.check(
            "SIGCONT is not ignored by default: blocked, the death arrives",
            take(&set).map(|i| i.signo) == Some(libc::SIGCONT),
        );
        let _ = wait4_opts(pid, WCLONE);
    }
    restore(&old);

    // The SIGCHLD rule's own exception, kept: a blocked, ignored SIGCHLD death
    // is still dropped where the SIGTERM shape above is delivered.
    let old = block(libc::SIGCHLD);
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let set = one(libc::SIGCHLD);
    let pid = spawn(libc::SIGCHLD, 7);
    r.check(
        "a clone(SIGCHLD) child for the blocked-ignore control was started",
        pid > 0,
    );
    if pid > 0 {
        r.check(
            "an ignored SIGCHLD death is dropped even when blocked",
            take(&set).is_none(),
        );
        let _ = wait4_opts(pid, 0);
    }
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    restore(&old);

    // ---- the consequence: a SIG_DFL parent dies of it ----------------------
    //
    // This is why the byte must be delivered rather than folded into SIGCHLD. A
    // `SIG_DFL`, unblocked parent is killed by its `clone(SIGTERM)` child's
    // death, so the observable is a helper that dies of SIGTERM. The helper is
    // a `fork` of this process (a clone child would leave its death in *this*
    // process's queue); it resets SIGTERM to `SIG_DFL`, clears its mask, starts
    // the child and sleeps. If the death signal is delivered it never wakes.
    let helper = unsafe { libc::fork() };
    if helper == 0 {
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            let none: libc::sigset_t = core::mem::zeroed();
            libc::sigprocmask(libc::SIG_SETMASK, &none, core::ptr::null_mut());
            let _child = spawn(libc::SIGTERM, 7);
            let nap = libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            libc::nanosleep(&nap, core::ptr::null_mut());
            libc::_exit(0);
        }
    }
    r.check(
        "a helper process was forked for the must-die claim",
        helper > 0,
    );
    if helper > 0 {
        let mut status: i32 = 0;
        unsafe { libc::waitpid(helper, &mut status, 0) };
        r.check(
            "a SIG_DFL, unblocked parent is killed by its clone(SIGTERM) child's death",
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGTERM,
        );
    }

    r.finish()
}
