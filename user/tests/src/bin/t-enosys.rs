//! t-enosys - the calls this kernel has no dispatch arm for.
//!
//! Pinned here: `memfd_secret`(447/447) answers ENOSYS. Every other number that
//! used to be on this list has moved out as it was served, each to its own pin:
//! `keyctl` (250) to `t-keyctl`, `fanotify_init`/`fanotify_mark` (300/301) to
//! `t-fanotify`, the three `io_uring` numbers (425/426/427) to `t-io-uring`,
//! `userfaultfd` (323/282) to `t-userfaultfd`, and `rseq` (334/293) to
//! `t-rseq`. `memfd_secret` is the one number still falling through: secret
//! memory wants a direct-map carve-out and a page-table shape this kernel does
//! not carry. ENOSYS is the answer a libc
//! reads as "this optional call is not available"; the numbers come from libc's
//! per-machine table, and the kernel's table has no handler for the one left,
//! so it falls through the default arm. A call the kernel *does* implement -
//! `getpid` - is the sanity that the probe is really reaching the table and not
//! reading a stuck register.
//!
//! The point of walking it is the kernel's own note, not the return value. The
//! first time a number falls through, mpos prints `mpos: syscall <nr> ->
//! ENOSYS` on the console, once per number for the life of the boot.
//! `memfd_secret` is therefore asked twice below: both calls answer ENOSYS, but
//! only the first prints a line. That once-per-number rule is what keeps a startup loop's
//! dozens of probes from becoming dozens of console lines while still telling a
//! reader what is missing.
//!
//! Every claim is `libc::syscall(*)`, clause (a): the probe's subject is the
//! number falling through, not a call either library names. `memfd_secret`'s
//! number is the same on both machines (447, asm-generic and x86_64 agree); the
//! literal is written down because libc has no `SYS_memfd_secret` constant.

use mpos_user_tests::Report;

/// `memfd_secret(2)`'s number: 447 on both machines' tables.
const SYS_MEMFD_SECRET: libc::c_long = 447;

fn probe(nr: libc::c_long) -> bool {
    // `memfd_secret`'s own argument shape: a flags word. The call is refused
    // before the argument is looked at, so a zero flag word is the honest probe.
    let r = unsafe { libc::syscall(nr, 0usize) };
    refused(r as isize, libc::ENOSYS)
        // On a current host `memfd_secret` is *served* (measured: it answers a
        // descriptor), so under a hosted run any refusal at all stands in for
        // the ENOSYS this kernel answers; the guest sweep pins ENOSYS itself.
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

    r.check("memfd_secret answers ENOSYS", probe(SYS_MEMFD_SECRET));

    // Asked a second time: still ENOSYS, but the console note for the
    // number was printed by the first call and not again.
    r.check(
        "memfd_secret asked a second time still answers ENOSYS",
        probe(SYS_MEMFD_SECRET),
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
