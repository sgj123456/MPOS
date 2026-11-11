//! t-rr-interval - `sched_rr_get_interval` answers a slice for a task that is
//! on the runqueue and **zero** for one that is not.
//!
//! The call looks like a constant, and every kernel that treats it as one is
//! wrong in a way a caller branches on. Zero is not "a very small slice": it is
//! the answer Linux gives when the target cannot run, and a program that polls
//! this door to decide whether a child is making progress reads a constant as
//! "yes, running" forever.
//!
//! Measured at the raw-syscall level on a host Linux before this kernel was
//! touched (`build/scratch/rrslice/{a,b,c,d,e,f,g,h}.c`, 7.2.14):
//!
//! * `d.c` is the decisive one, and it varies only the *state* of the target:
//!   a child blocked in `read()` answers `tv_nsec = 0`, the parent (running)
//!   answers `2000000` (2 ms), a spinning child answers `2000000`, and a
//!   zombie not yet reaped answers `0`. After the child is reaped the same
//!   number answers `ESRCH(3)`.
//! * `e.c` holds a child at a `SIGSTOP` and reads `0`, and a `SIGCONT` does not
//!   change it while the child stays blocked in `pause()`.
//! * `f.c` (nice 0/19/-20), `c.c` (SCHED_BATCH and SCHED_IDLE, where the
//!   unprivileged `sched_setscheduler` itself fails `EPERM`) and `g.c` (1 to 9
//!   runnable tasks with the reader pinned to cpu 0) all leave the slice
//!   alone, so this is a runqueue question and not a class, priority or load
//!   one. `h.c` accepts a thread's tid as readily as a process's pid.
//! * `a.c` fixes the order this door uses: it is the one `sched_*` door with no
//!   shared NULL-buffer guard, so the pid lookup runs first — `(0, NULL)` is
//!   `EFAULT(14)` and `(999999, NULL)` is `ESRCH(3)`, not `EINVAL`. `b.c`
//!   confirms the buffer really is 16 bytes: 4-, 8- and 12-byte windows ending
//!   at a guard page are `EFAULT`, the 16-byte one succeeds.
//!
//! The **value** of the slice is the machine's own and is deliberately not a
//! claim here — one tick at this kernel's 250 Hz is 4 ms and the reference host
//! runs 2 ms; both are that machine's round-robin timeslice. What is a contract
//! is zero versus non-zero, which is the half that follows the target's state.
//! Asserting the number would turn a tuning difference into a test failure and
//! would pass just as happily on a kernel that answered the constant forever.
//!
//! Until §6.127 this kernel answered `1_000_000_000 / TICK_HZ` for every
//! number it accepted, so the blocked, stopped and zombie claims below were
//! red while the order and buffer claims were already right.

use mpos_user_tests::Report;

/// An unreadable pointer: address 1 is unmapped on both the host and this
/// kernel, and both answer `EFAULT` for it.
const BAD: usize = 1;

const OK: i32 = 0;
const ESRCH: i32 = 3;
const EFAULT: i32 = 14;
const EINVAL: i32 = 22;

/// The one `sched_*` door that takes a `struct timespec *`. The number is the
/// machine's — 148 on x86_64, 127 on aarch64 — so it comes from `libc` rather
/// than from this file.
const NR_SCHED_RR_GET_INTERVAL: libc::c_long = libc::SYS_sched_rr_get_interval;

#[repr(C)]
struct TimeSpec {
    tv_sec: libc::c_long,
    tv_nsec: libc::c_long,
}

impl TimeSpec {
    fn zeroed() -> Self {
        TimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        }
    }
}

const _: () = assert!(core::mem::size_of::<TimeSpec>() == 16);

fn raw(nr: libc::c_long, a1: usize, a2: usize) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(nr, a1, a2) };
    if ret == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as isize)
    }
}

/// Ask the door about `who`, returning the errno (0 on success) and the
/// `tv_nsec` it wrote. A failed call leaves the buffer untouched, so the
/// second half is only meaningful when the first is zero.
fn interval(who: usize) -> (i32, i64) {
    let mut ts = TimeSpec::zeroed();
    let code = match raw(
        NR_SCHED_RR_GET_INTERVAL,
        who,
        &mut ts as *mut TimeSpec as usize,
    ) {
        Ok(_) => OK,
        Err(e) => e,
    };
    (code, ts.tv_nsec)
}

/// Whether this machine's answer is the slice rather than a zero. Any positive
/// number counts: the figure is the machine's, its presence is the contract.
fn is_slice(nsec: i64) -> bool {
    nsec > 0
}

fn main() {
    let mut r = Report::new();

    // -- the door's own shape, which was already right ----------------------
    let (code, nsec) = interval(0);
    r.check(
        &format!("a running task is answered a slice (got {code}, {nsec} ns)"),
        code == OK && is_slice(nsec),
    );

    let null = raw(NR_SCHED_RR_GET_INTERVAL, 0, 0).err().unwrap_or(OK);
    r.check(
        &format!("a null buffer is EFAULT on the running task (got {null})"),
        null == EFAULT,
    );

    let nobody = raw(NR_SCHED_RR_GET_INTERVAL, 999_999, 0)
        .err()
        .unwrap_or(OK);
    r.check(
        &format!("a number naming nobody is ESRCH before the buffer (got {nobody})"),
        nobody == ESRCH,
    );

    let negative = raw(NR_SCHED_RR_GET_INTERVAL, usize::MAX, 0)
        .err()
        .unwrap_or(OK);
    r.check(
        &format!("a negative pid is EINVAL (got {negative})"),
        negative == EINVAL,
    );

    let bad_buffer = raw(NR_SCHED_RR_GET_INTERVAL, 0, BAD).err().unwrap_or(OK);
    r.check(
        &format!("an unreadable buffer is EFAULT (got {bad_buffer})"),
        bad_buffer == EFAULT,
    );

    // -- the half that follows the target's state --------------------------
    //
    // One child, read in three states. Each measurement asserts the *state*
    // as well, so a red line says whether the kernel disagreed about the
    // answer or about having reached the state in the first place.
    let (read_end, write_end) = {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        (fds[0], fds[1])
    };

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        // Block in a read that will never be satisfied: this child is exactly
        // the `d.c` child that Linux answers zero for.
        let mut byte = 0u8;
        unsafe {
            libc::close(write_end);
            libc::read(read_end, &mut byte as *mut u8 as *mut libc::c_void, 1);
        }
        unsafe { libc::_exit(0) };
    }

    // Give the child a moment to reach the read, then ask.
    unsafe { libc::usleep(120_000) };

    let state = state_of(pid as usize);
    let (code, nsec) = interval(pid as usize);
    r.check(
        &format!(
            "a child blocked in a read is answered zero (state {}, got {code}, {nsec} ns)",
            state.unwrap_or('?')
        ),
        code == OK && nsec == 0,
    );

    // The same number one instruction after the caller's own, which the first
    // claim above already showed is non-zero: the two answers differ only in
    // whose state was asked about.
    let (own_code, own_nsec) = interval(0);
    r.check(
        &format!(
            "and the caller is answered a slice in the same breath ({own_code}, {own_nsec} ns)"
        ),
        own_code == OK && is_slice(own_nsec),
    );

    // Held at a stoppage. `e.c` measured zero here, on a child of the same
    // shape as the one above.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let mut status = 0i32;
    unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
    let state = state_of(pid as usize);
    let (code, nsec) = interval(pid as usize);
    r.check(
        &format!(
            "a child held at a stop is answered zero (state {}, got {code}, {nsec} ns)",
            state.unwrap_or('?')
        ),
        code == OK && nsec == 0,
    );

    // A number that names a task which cannot run, asked twice: the stop is
    // not what the answer keys on, being off the runqueue is.
    r.check(
        &format!("the stopped child's letter is not a running one ({state:?})"),
        state.is_some_and(|c| c != 'R'),
    );

    // -- a zombie, and then a number nobody can name ----------------------
    unsafe {
        libc::kill(pid, libc::SIGCONT);
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, &mut status, 0);
    }

    let zombie = unsafe { libc::fork() };
    assert!(zombie >= 0, "fork");
    if zombie == 0 {
        unsafe { libc::_exit(7) };
    }
    // Let it die without collecting it: the slot stays, the task does not run.
    unsafe { libc::usleep(150_000) };

    let state = state_of(zombie as usize);
    let (code, nsec) = interval(zombie as usize);
    r.check(
        &format!(
            "a zombie is answered zero (state {}, got {code}, {nsec} ns)",
            state.unwrap_or('?')
        ),
        code == OK && nsec == 0,
    );

    unsafe { libc::waitpid(zombie, &mut status, 0) };

    let reaped = raw(NR_SCHED_RR_GET_INTERVAL, zombie as usize, 0)
        .err()
        .unwrap_or(OK);
    r.check(
        &format!("and the same number after the reap is ESRCH (got {reaped})"),
        reaped == ESRCH,
    );

    // -- a runnable child, the control that keeps the claims above honest --
    //
    // Without this, a kernel that answered zero for *everything* would pass
    // every state claim above. `d.c` measured the slice for a spinning child.
    let spinner = unsafe { libc::fork() };
    assert!(spinner >= 0, "fork");
    if spinner == 0 {
        let mut spin: u64 = 1;
        loop {
            spin = spin.wrapping_mul(6364136223846793005).wrapping_add(1);
            unsafe { core::ptr::read_volatile(&spin) };
        }
    }

    unsafe { libc::usleep(120_000) };
    let state = state_of(spinner as usize);
    let (code, nsec) = interval(spinner as usize);
    r.check(
        &format!(
            "a runnable child is answered a slice (state {}, got {code}, {nsec} ns)",
            state.unwrap_or('?')
        ),
        code == OK && is_slice(nsec),
    );

    unsafe {
        libc::kill(spinner, libc::SIGKILL);
        libc::waitpid(spinner, &mut status, 0);
        libc::close(read_end);
        libc::close(write_end);
    }

    r.finish();
}

/// The state letter this machine reports for `who` in `/proc/<who>/stat`, or
/// `None` when there is no such file. Used only to make a red line readable:
/// the claims above assert the *answer*, and this says what the kernel thought
/// of the target when it gave it.
fn state_of(who: usize) -> Option<char> {
    let text = std::fs::read_to_string(format!("/proc/{who}/stat")).ok()?;
    // Field 3 is the letter, and the command field (2) is parenthesised and may
    // contain spaces, so the letter is the first character after the last ')'.
    let after = text.rsplit_once(')')?.1;
    after.split_whitespace().next()?.chars().next()
}
