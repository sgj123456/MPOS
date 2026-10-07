//! t-prlimit64 - one test for prlimit64, which the strace shows twice
//! querying RLIMIT_STACK (`prlimit64(0, RLIMIT_STACK, NULL, {8192*1024,
//! RLIM64_INFINITY}) = 0`): Rust std reads the stack limit to size thread
//! stacks.
//!
//! Pinned here: a query with new=NULL fills the 16-byte struct rlimit64
//! with a usable soft limit and a hard limit that is at least as large.
//! The exact values are deliberately not pinned - only that the answer is
//! sane enough to compute with.
//!
//! Beyond the strace's call: RLIMIT_NOFILE is pinned to its exact soft
//! limit of 1024 - the number every descriptor-hoarding program branches
//! on - and the refusals are pinned too: a resource number past the table
//! is EINVAL, and old=NULL suppresses the query and answers 0.
//!
//! # The number this program is also about
//!
//! `RLIMIT_NOFILE` is `7`, and taking that from a maintained table
//! (`libc::RLIMIT_NOFILE`, whose value is `asm-generic/resource.h`'s) is what
//! turned this program from a migration into a bug report: the kernel's resource
//! table held the descriptor ceiling on row `5`, which is Linux's `RLIMIT_RSS`,
//! so the claim below was reading the process-count budget. The table and this
//! constant agreed with each other - the no-std version had `RLIMIT_NOFILE = 5`
//! written into it - which is why neither could catch it. Both are fixed
//! (`PLAN.md` §15.13), and the claim now means what it says.
//!
//! # Why the calls are raw
//!
//! Three of the four things asserted here are about the *call's own shape*, and
//! `rustix::process::getrlimit` has none of it: it takes a `Resource` and returns
//! an `Rlimit` with **no `Result`** - the success/failure these claims read is
//! discarded, the rule's clause (b), and for a reason that is exactly the bug:
//! a closed `Resource` enum cannot be `99`, so the wrapper had no failure to
//! report. Its `Rlimit` is an `Option<u64>` pair as well, where the ABI's shape
//! is two plain words, and the null-`old` claim is about the *pointer* protocol.
//! So the call goes through `libc::syscall` with the number from
//! `libc::SYS_prlimit64` (a maintained table; `PRLIMIT64` is `261` on aarch64 and
//! `302` on x86_64) and the resource numbers from `libc::RLIMIT_*`.
//!
//! The 16-byte struct is written out here rather than taken from a binding,
//! because there is no binding to take on this target: `libc::rlimit64` is
//! declared for Android and l4re and not for linux-musl. Its layout is the UAPI
//! (`docs/UAPI.md` §4 pins it to this program) and is the same on both machines.

use mpos_user_tests::Report;
use rustix::io::Errno;

/// `struct rlimit64` as the call reads and writes it: the soft ceiling and the
/// hard one, each a `u64`.
#[repr(C)]
struct Rlimit64 {
    cur: u64,
    max: u64,
}

/// The errno of the call just made, read immediately.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `prlimit64(pid 0, resource, new, old)` - the calling process's own table.
fn prlimit64(resource: i32, new: *const Rlimit64, old: *mut Rlimit64) -> Result<(), Errno> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_prlimit64,
            0 as libc::c_long,
            resource as libc::c_long,
            new,
            old,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(last_errno()))
    }
}

fn main() {
    println!("t-prlimit64");
    let mut r = Report::new();
    let mut rl = Rlimit64 { cur: 0, max: 0 };

    let stack = prlimit64(libc::RLIMIT_STACK, core::ptr::null(), &mut rl);
    r.check("querying RLIMIT_STACK succeeds", stack.is_ok());
    // Read out of the struct whether or not the call worked: if it did not, the
    // zeros it was filled with are what these two claims judge.
    r.check("the soft limit is usable", rl.cur > 0);
    r.check("the hard limit is at least the soft one", rl.max >= rl.cur);

    // RLIMIT_NOFILE is a number programs compute with, not just sanity-check:
    // the soft limit is pinned to the classic 1024 exactly.
    match prlimit64(libc::RLIMIT_NOFILE, core::ptr::null(), &mut rl) {
        Ok(()) => {
            r.check("querying RLIMIT_NOFILE succeeds", true);
            r.check("the RLIMIT_NOFILE soft limit is 1024", rl.cur == 1024);
        }
        Err(_) => {
            r.check("querying RLIMIT_NOFILE succeeds", false);
            r.check("the RLIMIT_NOFILE soft limit is 1024", false);
        }
    }

    // A resource number past the table has no row to read: EINVAL. 99 is the
    // first number past `RLIM_NLIMITS` (16) that is also obviously not a typo
    // for one of them.
    r.check(
        "an unknown resource of 99 is EINVAL",
        prlimit64(99, core::ptr::null(), core::ptr::null_mut()) == Err(Errno::INVAL),
    );

    // old=NULL suppresses the query: the call is a valid no-op.
    r.check(
        "a query with a null old pointer answers 0 and writes nothing",
        prlimit64(libc::RLIMIT_STACK, core::ptr::null(), core::ptr::null_mut()) == Ok(()),
    );

    r.finish()
}
