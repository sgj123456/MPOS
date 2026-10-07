//! t-set-tid-address - the clear-child-tid registration, which a musl
//! binary's strace calls right after arch_prctl: musl's `__init_tp` registers
//! the main thread's slot and uses the return value as the thread id.
//!
//! The contract pinned here: the call answers with a live tid, equal to
//! getpid - the main thread's tid is its pid on Linux. The other half of the
//! contract, the kernel storing 0 into the slot when the thread exits, is
//! observable only through memory shared between parent and child, which this
//! test does not have; it is documented as out of scope.
//!
//! The migrated shape keeps the raw call for one reason the std side cannot
//! express: the *return value* is the claim, and there is no wrapper that
//! returns it (`libc::syscall(SYS_set_tid_address, ..)` is the call itself).
//! Its number comes from libc's per-machine table - 218 on the first
//! machine, 96 on the second - which agrees with this kernel's own tables
//! (`nr_x86_64.rs` / `nr_aarch64.rs`, SET_TID_ADDRESS), so both machines
//! reach the same dispatch arm. The tid read beside it comes from
//! `rustix::thread::gettid`, so the two halves of the comparison have
//! different sources.

use mpos_user_tests::Report;
use rustix::thread::gettid;

/// The slot musl would register - left alone here, since its clear-on-exit
/// write is not observable from inside the process.
static mut SLOT: i32 = -1;

fn main() {
    println!("t-set-tid-address");
    let mut r = Report::new();

    let tid = unsafe { libc::syscall(libc::SYS_set_tid_address, &raw mut SLOT) as i32 };
    r.check("set_tid_address returns a live tid", tid > 0);
    r.check(
        "the tid is the process's own id",
        tid == gettid().as_raw_nonzero().get(),
    );

    r.finish()
}
