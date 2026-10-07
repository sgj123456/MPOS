//! t-getuid - one test for the credential quartet: getuid, getgid, geteuid and
//! getegid.
//!
//! The man page pins: getuid and geteuid return the real and effective user id,
//! getgid and getegid the group ids - always successful, no errno. A fresh
//! process on this kernel starts as root, so all four answers are exactly 0
//! here; pinned exactly rather than "some id", so a caller branching on
//! `uid == 0` to decide whether to try something privileged gets a stable answer
//! at startup. `t-posix` is the test that pins where they go after a `setuid`
//! dance moves them.
//!
//! The ids come from `rustix::process` rather than from a number written down
//! here, so what is compared is this machine's libc and this kernel - the two
//! things that have to agree - and the test runs unchanged on both machines
//! (`PLAN.md` §15.11).

use mpos_user_tests::Report;

fn main() {
    println!("t-getuid");
    let mut r = Report::new();

    r.check("getuid is 0", rustix::process::getuid().as_raw() == 0);
    r.check("getgid is 0", rustix::process::getgid().as_raw() == 0);
    r.check("geteuid is 0", rustix::process::geteuid().as_raw() == 0);
    r.check("getegid is 0", rustix::process::getegid().as_raw() == 0);

    r.finish()
}
