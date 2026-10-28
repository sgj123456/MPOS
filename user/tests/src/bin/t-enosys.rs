//! t-enosys - the calls this kernel has no dispatch arm for.
//!
//! Pinned here: userfaultfd(323) answers ENOSYS. (`keyctl` used to be on this
//! list; it is served now, so the family's pin is `t-keyctl`. `fanotify_init`/
//! `fanotify_mark` left the same way, their pin is `t-fanotify`. `io_uring_setup`/
//! `enter`/`register` (425/426/427) left the same way too: the ring is served,
//! pinned by `t-io-uring`.) That is the answer a libc reads
//! as "this optional call is not available"; the numbers come from libc's
//! per-machine table, and the kernel's table has no constant or handler for
//! any of them, so each one falls through the default arm. A call the kernel
//! *does* implement - `getpid` - is the sanity that the probe is really
//! reaching the table and not reading a stuck register.
//!
//! The point of walking these is the kernel's own note, not the return
//! value. The first time a number falls through, mpos prints
//! `mpos: syscall <nr> -> ENOSYS` on the console, once per number for the
//! life of the boot. `userfaultfd` is therefore asked twice below: both calls
//! answer ENOSYS, but only the first prints a line. That once-per-number
//! rule is what keeps a startup loop's dozens of probes from becoming dozens
//! of console lines while still telling a reader what is missing.
//!
//! Every claim is `libc::syscall(SYS_*)`, clause (a): the probe's subject is
//! the number falling through, not a call either library names. On the second
//! machine the one number that differs (userfaultfd 282) is read from libc
//! rather than written down here.

use mpos_user_tests::Report;

fn probe(nr: libc::c_long) -> bool {
    let r = unsafe { libc::syscall(nr, 0usize, 0usize, 0usize) };
    refused(r as isize, libc::ENOSYS)
        // On a current host these calls are *served* (keyctl EPERM,
        // userfaultfd EINVAL/EAGAIN, io_uring EINVAL -
        // measured here), so under a hosted run any refusal at all stands
        // in for the ENOSYS this kernel answers; the payload sweep pins
        // ENOSYS itself.
        || mpos_user_tests::host_run() && r == -1
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-enosys");
    let mut r = Report::new();

    r.check("userfaultfd answers ENOSYS", probe(libc::SYS_userfaultfd));

    // Asked a second time: still ENOSYS, but the console note for the
    // number was printed by the first call and not again.
    r.check(
        "userfaultfd asked a second time still answers ENOSYS",
        probe(libc::SYS_userfaultfd),
    );

    // The control: an implemented call does not answer ENOSYS, so the probe
    // above is measuring the table and not a broken register.
    let pid = rustix::process::getpid();
    r.check(
        "getpid is implemented, so it does not answer ENOSYS",
        pid.as_raw_nonzero().get() > 0,
    );

    r.finish()
}
