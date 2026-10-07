//! t-set-robust-list - the registration of a robust-futex head, which glibc
//! calls at startup (`set_robust_list(0x..., 24) = 0` in the strace).
//!
//! Pinned here, per the man page's contract this kernel answers: the exact
//! head size Linux demands - 24 bytes answers 0, anything else answers
//! EINVAL. The head's contents are never touched by this syscall; only the
//! registration is pinned.
//!
//! The migrated shape goes through `libc::syscall(SYS_set_robust_list, ..)`
//! rather than the named `libc::set_robust_list`, because that wrapper does
//! not exist on linux-musl (measured: libc 0.2.190 declares it for glibc and
//! uclibc only) - clause (a), a call whose binding is absent on one of the
//! two machines. The number comes from libc's per-machine table (273 on the
//! first, 99 on the second), so both reach the same dispatch arm.

use mpos_user_tests::Report;

/// The head itself: one link word, plus the two futex words Linux's
/// `struct robust_list_head` carries after it. Only its address and the
/// declared length matter to this call; the bytes are never read.
#[repr(C)]
struct RobustHead {
    list: [u64; 3],
}

static HEAD: RobustHead = RobustHead { list: [0; 3] };

fn set_robust_list(len: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_set_robust_list, &HEAD as *const _ as usize, len) as isize }
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-set-robust-list");
    let mut r = Report::new();

    r.check(
        "registering the 24-byte head succeeds",
        set_robust_list(24) == 0,
    );
    r.check(
        "a wrong head size answers EINVAL",
        refused(set_robust_list(23), libc::EINVAL),
    );

    r.finish()
}
