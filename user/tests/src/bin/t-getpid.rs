//! t-getpid - one test for the process-identity trio: getpid, gettid and
//! getppid.
//!
//! The man page pins: getpid returns the caller's process id, always positive;
//! gettid returns the calling thread's id, which for the single main thread of a
//! process IS the process id; and getppid returns the parent's id, 0 only for
//! init. Pinned here: the id is positive, stable across two calls - an id that
//! moved between reads would break every `/proc` lookup - the main thread's tid
//! equals the pid, and the parent is a real, different process (this test is
//! spawned by the runner, so the parent id is positive and not our own - which is
//! the one claim that depends on being spawned rather than on being pid 1).

use mpos_user_tests::Report;

fn main() {
    println!("t-getpid");
    let mut r = Report::new();

    let first = rustix::process::getpid().as_raw_nonzero().get();
    r.check("getpid answers a positive id", first > 0);

    let second = rustix::process::getpid().as_raw_nonzero().get();
    r.check("a second getpid answers the same id", second == first);

    // The main thread is the process: its tid is the pid.
    let tid = rustix::thread::gettid().as_raw_nonzero().get();
    r.check("the main thread's gettid equals getpid", tid == first);

    // The runner spawned this test, so the parent is real and not us.
    let parent = rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get());
    r.check(
        "getppid answers a positive parent id",
        parent.is_some_and(|pid| pid > 0),
    );
    r.check(
        "the parent is a different process than this one",
        parent != Some(first),
    );

    r.finish()
}
