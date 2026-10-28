//! t-enosys - the calls this kernel has no dispatch arm for.
//!
//! Pinned here: `rseq`(334/293) answers ENOSYS. Every other number that used to
//! be on this list has moved out as it was served, each to its own pin:
//! `keyctl` (250) to `t-keyctl`, `fanotify_init`/`fanotify_mark` (300/301) to
//! `t-fanotify`, the three `io_uring` numbers (425/426/427) to `t-io-uring`,
//! and `userfaultfd` (323/282) to `t-userfaultfd`. ENOSYS is the answer a libc
//! reads as "this optional call is not available"; the numbers come from libc's
//! per-machine table, and the kernel's table has no handler for the one left,
//! so it falls through the default arm. A call the kernel *does* implement -
//! `getpid` - is the sanity that the probe is really reaching the table and not
//! reading a stuck register.
//!
//! The point of walking it is the kernel's own note, not the return value. The
//! first time a number falls through, mpos prints `mpos: syscall <nr> ->
//! ENOSYS` on the console, once per number for the life of the boot. `rseq` is
//! therefore asked twice below: both calls answer ENOSYS, but only the first
//! prints a line. That once-per-number rule is what keeps a startup loop's
//! dozens of probes from becoming dozens of console lines while still telling a
//! reader what is missing.
//!
//! Every claim is `libc::syscall(SYS_*)`, clause (a): the probe's subject is
//! the number falling through, not a call either library names. `rseq`'s number
//! is read from libc rather than written down here, because the two machines
//! differ (334 on x86_64, 293 on aarch64).

use mpos_user_tests::Report;

fn probe(nr: libc::c_long) -> bool {
    // `rseq`'s own argument shape: a `struct rseq *` and a length, with
    // `RSEQ_FLAG_UNREGISTER` = 1 asking for the opposite of a registration.
    // The call is refused before either is looked at, so a null block and a
    // zero length are the honest probe.
    let r = unsafe { libc::syscall(nr, 0usize, 0usize, 1usize) };
    refused(r as isize, libc::ENOSYS)
        // On a current host `rseq` is *served* (measured here: the call either
        // registers or refuses the null block), so under a hosted run any
        // refusal at all stands in for the ENOSYS this kernel answers; the
        // payload sweep pins ENOSYS itself.
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

    r.check("rseq answers ENOSYS", probe(libc::SYS_rseq));

    // Asked a second time: still ENOSYS, but the console note for the
    // number was printed by the first call and not again.
    r.check(
        "rseq asked a second time still answers ENOSYS",
        probe(libc::SYS_rseq),
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
