//! t-kill-numbers - what a pid number names, once the thread carrying it has
//! gone: six shapes, and Linux's answer differs between the two doors.
//!
//! `kill` names a **process**, and a process outlives the thread that carries
//! its number: a group whose leader thread exited under a parked worker still
//! answers for that number, and a real signal to it reaches the worker. `tkill`
//! names a **thread**, and a thread's number goes with the thread - except when
//! that thread led the group, whose number is the group's own and outlives it.
//!
//! Measured on a current host before these claims were written
//! (`build/scratch/zombiekill/p{,2,3,4,6}.c`, Linux 7.2.11):
//!
//! ```text
//! kill(<leaderless group>)   = 0, and SIGKILL reaps it
//! kill(<zombie>)             = 0
//! kill(<dead worker's tid>)  = -1 ESRCH, while the group lives
//! kill(<reaped number>)      = -1 ESRCH
//! tkill(<dead leader>)       = 0, and the surviving worker's handler does not run
//! tkill(<dead worker>)       = -1 ESRCH
//! ```
//!
//! This machine answered `-1 ESRCH` for all of the 0s: it resolved a number to
//! *a slot* and then refused an exited target, so a program that killed the
//! group it had just orphaned could not reap it (`docs/notes/` §6.77e - and
//! the same `ESRCH` is what hung the `KILL_THREAD` fixture of `t-seccomp`).
//!
//! The leader thread is ended with a raw `exit(2)` - the one call that ends the
//! calling thread and leaves its group alone - because that is the shape being
//! measured; `pthread_exit` in this crate's `std` is a thread return, which
//! also cannot end the leader of the main thread. The worker publishes its tid
//! through a shared page, since a plain static is not shared across `fork`.

use mpos_user_tests::Report;

/// `kill`/`tkill`/`tgkill` with `errno` read at the right moment: the raw door
/// is what these claims need (signal 0 has no `Signal` variant, and `tkill` is
/// not in rustix's vocabulary), so `errno` is cleared before the call and read
/// straight after it.
fn call3(nr: libc::c_long, a: libc::c_long, b: libc::c_long, c: libc::c_long) -> (isize, i32) {
    unsafe {
        *libc::__errno_location() = 0;
    }
    let ret = unsafe { libc::syscall(nr, a, b, c) } as isize;
    let errno = unsafe { *libc::__errno_location() };
    (ret, errno)
}

fn refused(ret: isize, errno: i32, want: i32) -> bool {
    ret == -1 && errno == want
}

/// Reap `kid` if it is reaped within `ms`, and say whether it was.
///
/// A plain blocking `waitpid` is what these claims are *about*, but it is also
/// the one thing this program must not do: on a kernel where the group cannot
/// be signalled, the waiter never returns and the whole lane hangs instead of
/// going red (`docs/notes/` §6.77e - the same shape hung `t-seccomp`'s
/// `KILL_THREAD` fixture for a minute). A bounded poll reports the same fact
/// and lets the program finish.
fn reaped_within(kid: libc::pid_t, ms: u32) -> bool {
    let mut waited = 0;
    while waited < ms {
        let mut st: libc::c_int = 0;
        let got = unsafe { libc::waitpid(kid, &mut st, libc::WNOHANG) };
        if got == kid {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        waited += 10;
    }
    false
}

/// A group whose leader thread has exited and whose worker is parked: the
/// child ends its leader with `exit(2)` - this thread only - leaving the
/// worker alive under the same number.
fn leaderless_group() -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        std::thread::spawn(|| {
            // Park for a long time: nothing wakes this, so the group stays
            // alive while the leader's number is the one under test.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        unsafe { libc::syscall(libc::SYS_exit, 0) };
        unreachable!();
    }
    kid
}

/// A whole zombie: every thread gone, not yet reaped.
fn zombie() -> libc::pid_t {
    let kid = unsafe { libc::fork() };
    if kid == 0 {
        unsafe { libc::_exit(7) };
    }
    kid
}

fn main() -> ! {
    println!("t-kill-numbers");
    let mut r = Report::new();

    // Shape 1: the leader's number under a live worker.
    let kid = leaderless_group();
    std::thread::sleep(std::time::Duration::from_millis(700));
    let num = kid as libc::c_long;
    let (ret, errno) = call3(libc::SYS_kill, num, 0, 0);
    r.check(
        "kill names the process, not the thread that carried its number",
        ret == 0,
    );
    println!("     kill({num}, 0) = {ret} errno={errno}");

    let (ret, errno) = call3(libc::SYS_tkill, num, 0, 0);
    r.check(
        "tkill at a group leader's number answers for the group it still names",
        ret == 0,
    );
    println!("     tkill({num}, 0) = {ret} errno={errno}");

    let (ret, errno) = call3(libc::SYS_tgkill, num, num, 0);
    r.check(
        "tgkill at a dead leader's own thread id is accepted",
        ret == 0,
    );
    println!("     tgkill({num}, {num}, 0) = {ret} errno={errno}");

    let (ret, errno) = call3(libc::SYS_kill, num, libc::SIGKILL as libc::c_long, 0);
    r.check(
        "a signal at that number reaches the worker the number still names",
        ret == 0,
    );
    println!("     kill({num}, SIGKILL) = {ret} errno={errno}");
    r.check(
        "and the group is reaped, so a wait for it answers",
        reaped_within(kid, 3000),
    );

    // Shape 2: a whole zombie - every thread exited, the number still owned.
    let dead = zombie();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let dead_num = dead as libc::c_long;
    let (ret, errno) = call3(libc::SYS_kill, dead_num, 0, 0);
    r.check(
        "a zombie's number is still owned, so kill answers success",
        ret == 0,
    );
    println!("     kill({dead_num}, 0) = {ret} errno={errno}");
    let (ret, errno) = call3(libc::SYS_tkill, dead_num, 0, 0);
    r.check(
        "and tkill at it is accepted too, because it is the leader's number",
        ret == 0,
    );
    println!("     tkill({dead_num}, 0) = {ret} errno={errno}");
    r.check("the zombie is reaped", reaped_within(dead, 3000));

    // Shape 3: the same number once nobody owns it.
    let (ret, errno) = call3(libc::SYS_kill, dead_num, 0, 0);
    r.check(
        "a reaped number names nobody, so kill is ESRCH",
        refused(ret, errno, libc::ESRCH),
    );
    println!("     kill({dead_num}, 0) = {ret} errno={errno}");
    let (ret, errno) = call3(libc::SYS_tkill, dead_num, 0, 0);
    r.check("and tkill at it is ESRCH", refused(ret, errno, libc::ESRCH));
    println!("     tkill({dead_num}, 0) = {ret} errno={errno}");

    // Shape 4: a worker's number dies with the worker, while the group lives.
    // The worker publishes its tid through a pipe, which survives `fork` and
    // is the same door a real caller reads `/proc` through.
    let (read_end, write_end) = {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    };
    let kid2 = unsafe { libc::fork() };
    if kid2 == 0 {
        unsafe { libc::close(read_end) };
        let tid = std::thread::spawn(|| unsafe { libc::syscall(libc::SYS_gettid) as i32 });
        // The thread returns at once, so its number dies with it.
        let tid = tid.join().unwrap();
        unsafe {
            libc::write(write_end, &tid as *const i32 as *const libc::c_void, 4);
            libc::close(write_end);
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::close(write_end) };
    let mut tid: i32 = 0;
    let got = unsafe { libc::read(read_end, &mut tid as *mut i32 as *mut libc::c_void, 4) };
    unsafe { libc::close(read_end) };
    r.check("the worker published its thread id", got == 4 && tid > 1);
    // The worker is gone and the leader lives on for another 600 ms, which is
    // the window the claim is about.
    let (ret, errno) = call3(libc::SYS_tkill, tid as libc::c_long, 0, 0);
    r.check(
        "a worker's number goes with the thread, so tkill at it is ESRCH",
        refused(ret, errno, libc::ESRCH),
    );
    println!("     tkill(<dead worker {tid}>, 0) = {ret} errno={errno}");
    // The last fixture left a live group behind: end it so nothing goes on
    // running after the report (`_exit(0)` there is this thread only).
    unsafe { libc::kill(kid2, libc::SIGKILL) };
    let _ = reaped_within(kid2, 3000);

    r.finish()
}
