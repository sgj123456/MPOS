//! t-enosys - the calls this kernel has no dispatch arm for.
//!
//! Pinned here: `memfd_secret` (447) and the `pkey_*` trio (329/330/331) answer
//! `ENOSYS`. Every other number that used to be on this list has moved out as it
//! was served, each to its own pin: `keyctl` (250) to `t-keyctl`,
//! `fanotify_init`/`fanotify_mark` (300/301) to `t-fanotify`, the three
//! `io_uring` numbers (425/426/427) to `t-io-uring`, `userfaultfd` (323/282) to
//! `t-userfaultfd`, and `rseq` (334/293) to `t-rseq`.
//!
//! Both survivors are refused for a **measured** reason rather than for want of a
//! handler:
//!
//! * `memfd_secret` wants a direct-map carve-out and a page-table shape this
//!   kernel does not carry.
//! * The `pkey_*` trio runs on Linux only when the kernel was built with
//!   `CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS`, and this kernel has no PKU at
//!   all: `CR4.PKE` is never set, no PTE carries a protection-key field, and
//!   there is no per-task `PKRU`. The measurement that makes this a refusal
//!   rather than an omission: **the protection is real on a host that has it**.
//!   `build/scratch/opathmmap/pkey4.c` allocates a `PKEY_DISABLE_ACCESS` key,
//!   maps memory under it, and the child that touches it dies of `SIGSEGV`.
//!   Serving the calls while keys enforce nothing would hand a caller a zero for
//!   a boundary that is not there, which is the one thing `AGENTS.md` §3.2
//!   forbids.
//!
//! `ENOSYS` is the answer a libc reads as "this optional call is not available";
//! the numbers come from libc's per-machine table, and the kernel's table has no
//! handler for either survivor, so they fall through the default arm. A call the
//! kernel *does* implement - `getpid` - is the sanity that the probe is really
//! reaching the table and not reading a stuck register.
//!
//! The point of walking it is the kernel's own note, not the return value. The
//! first time a number falls through, mpos prints `mpos: syscall <nr> ->
//! ENOSYS` on the console, once per number for the life of the boot.
//! `memfd_secret` is therefore asked twice below: both calls answer `ENOSYS`, but
//! only the first prints a line. That once-per-number rule is what keeps a
//! startup loop's dozens of probes from becoming dozens of console lines while
//! still telling a reader what is missing.
//!
//! Every claim is `libc::syscall(*)`, clause (a): the probe's subject is the
//! number falling through, not a call either library names. `memfd_secret`'s
//! number is the same on both machines (447, asm-generic and x86_64 agree); the
//! literal is written down because libc has no `SYS_memfd_secret` constant.

use mpos_user_tests::Report;

/// `memfd_secret(2)`'s number: 447 on both machines' tables.
const SYS_MEMFD_SECRET: libc::c_long = 447;

/// The `pkey_*` trio's numbers. 329/330/331 on x86_64; the asm-generic table
/// aarch64 uses has `pkey_mprotect` 288, `pkey_alloc` 289 and `pkey_free` 290,
/// so the literals are per machine - a single set would call other calls on one
/// of them (the defect item 314's pin records).
#[cfg(target_arch = "x86_64")]
const PKEY_MPROTECT: libc::c_long = 329;
#[cfg(target_arch = "x86_64")]
const PKEY_ALLOC: libc::c_long = 330;
#[cfg(target_arch = "x86_64")]
const PKEY_FREE: libc::c_long = 331;
#[cfg(not(target_arch = "x86_64"))]
const PKEY_MPROTECT: libc::c_long = 288;
#[cfg(not(target_arch = "x86_64"))]
const PKEY_ALLOC: libc::c_long = 289;
#[cfg(not(target_arch = "x86_64"))]
const PKEY_FREE: libc::c_long = 290;

fn probe(nr: libc::c_long) -> bool {
    // `memfd_secret`'s own argument shape: a flags word. The call is refused
    // before the argument is looked at, so a zero flag word is the honest probe.
    let r = unsafe { libc::syscall(nr, 0usize) };
    // On a current host `memfd_secret` is **served** (measured: it answers a
    // descriptor, fd 3), so a hosted run cannot see this kernel's refusal at
    // all - the claim is about the guest. The first version wrote
    // `host_run() && r == -1`, which reads as "any refusal stands in", but the
    // host does not refuse: `r` is a descriptor, the clause is false, and the
    // *host* leg of the sweep failed on two claims that were working as
    // designed. `host_run()` alone is the right escape: the claim is "this
    // kernel answers ENOSYS", and a machine that serves the call is not the
    // subject.
    refused(r as isize, libc::ENOSYS) || mpos_user_tests::host_run()
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

    // === the `pkey_*` trio ==================================================
    //
    // Refused because this kernel has no PKU, not because nobody wired a
    // handler - and the note in the header carries the measurement that makes
    // the difference: on a host with protection keys the memory really does
    // fault, so a 0 here would be a promise about a boundary that does not
    // exist. On a current **host** these are served (it has PKU), so the same
    // hosted-run escape the `memfd_secret` claim takes applies.
    for (label, nr) in [
        ("pkey_mprotect", PKEY_MPROTECT),
        ("pkey_alloc", PKEY_ALLOC),
        ("pkey_free", PKEY_FREE),
    ] {
        // SAFETY: each is a raw number whose subject is the dispatch's default
        // arm; the arguments are never looked at by a call that is not there.
        let rc = unsafe { libc::syscall(nr, 0usize, 0usize, 0usize) };
        let ok = refused(rc as isize, libc::ENOSYS)
            || (mpos_user_tests::host_run() && (rc >= 0 || rc == -1));
        r.check(&format!("{label} answers ENOSYS ({rc})"), ok);
    }

    // The control: an implemented call does not answer ENOSYS, so the probe
    // above is measuring the table and not a broken register.
    let pid = rustix::process::getpid();
    r.check(
        "getpid is implemented, so it does not answer ENOSYS",
        pid.as_raw_nonzero().get() > 0,
    );

    r.finish()
}
