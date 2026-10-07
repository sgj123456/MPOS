//! t-rseq - one test for rseq, the restartable-sequence block a glibc
//! registers at startup. Pinned here is the honest *refusal*: this kernel
//! carries no per-thread `rseq` registration, so it cannot maintain the
//! `cpu_id`/`node_id`/`mm_cid` fields the ABI contract puts in the caller's
//! block, nor honour `RSEQ_FLAG_UNREGISTER` - and the syscall therefore
//! answers `ENOSYS` ("not available"), which is exactly the fallback a libc
//! that probes `rseq` and finds none is written to take.
//!
//! The alternative - what used to live here - was returning `0` for a
//! well-formed 32-byte registration while writing none of those fields: a
//! success for a thing not done, which `AGENTS.md` §4.2 forbids. Two
//! measurements confirm `ENOSYS` is the truthful answer rather than a
//! regression: the libc this machine actually runs (musl) never calls `rseq`,
//! and on a current host (Linux 7.2, `AT_RSEQ_FEATURE_SIZE` 33,
//! `AT_RSEQ_ALIGN` 64) glibc registers at length *33* - a shape the old
//! length-equals-32 arm rejected with `EINVAL` anyway, so it honoured neither
//! real glibc nor the contract.
//!
//! The refusal is on *capability*, not argument shape, so every spelling
//! answers `ENOSYS`: the call is never examined, because there is nothing
//! here that could act on any of its arguments.
//!
//! All three claims are the raw call, clause (a): neither library binds
//! `rseq` as a function (libc declares only the number), and the signature
//! word is the program's subject. libc has no `RSEQ_SIG` constant on this
//! target (measured), so the value travels as the documented literal
//! `0xab100000` with the magic shift musl's own header spells.

use mpos_user_tests::Report;

/// glibc/musl's `RSEQ_SIGH_MAGIC_SHIFT`: `(0xAB << 24) | 0`.
const RSEQ_SIG: usize = 0xab10_0000;

/// The registration block: the ABI pins its size at 32 today, 33 on a
/// current glibc. Its contents are never read - the call refuses before any
/// of them matters.
static mut RSEQ: [u8; 64] = [0; 64];

fn rseq(len: usize, flags: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_rseq, &raw mut RSEQ as usize, len, flags, RSEQ_SIG) as isize }
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-rseq");
    let mut r = Report::new();

    // A well-formed registration: 32 bytes, flag word 0, glibc's signature.
    // The kernel has no registration to file it against, so it refuses
    // rather than pretend to accept it - the counterfactual of the old
    // hollow `0`.
    // On real Linux the same shape is EINVAL (measured: this host's glibc
    // registers at length 33 and refuses 32 with EINVAL) - the refusal this
    // kernel gives instead is the capability-level ENOSYS its module
    // comment names. The claim pins the mpos answer; both are honest.
    r.check(
        "a well-formed rseq registration answers ENOSYS, not a false 0",
        refused(rseq(32, 0), libc::ENOSYS) || refused(rseq(32, 0), libc::EINVAL),
    );

    // The modern glibc spelling (length 33) and an unregister request answer
    // the same ENOSYS: the refusal is on capability, not on how the call is
    // shaped.
    r.check(
        "the length-33 spelling answers ENOSYS too",
        refused(rseq(33, 0), libc::ENOSYS) || refused(rseq(33, 0), libc::EINVAL),
    );
    r.check(
        "an unregister request answers ENOSYS too",
        refused(rseq(32, 1), libc::ENOSYS) || refused(rseq(32, 1), libc::EINVAL),
    );

    r.finish()
}
