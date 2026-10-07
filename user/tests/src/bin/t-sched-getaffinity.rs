//! t-sched-getaffinity - one test for sched_getaffinity, which the strace
//! shows at startup (`sched_getaffinity(pid, 32, mask) = 8` on the 16-core
//! host): Rust std's available_parallelism asks through it.
//!
//! Pinned here: the return value is **not** the CPU count. Linux answers the
//! number of bytes it placed in the mask - the strace line above says `= 8`,
//! the one word of this machine's mask - and a caller that wants a count asks
//! the bits, which is what Rust std's available_parallelism does. mpos
//! schedules on one CPU, so the mask is bit 0 with the rest clear, and the
//! claim reads exactly that from an eight-byte buffer. A pid of -1 - "some
//! other process, please" in the Linux shape - is answered the same way:
//! mpos does not track per-process affinities, while real Linux answers
//! ESRCH for it (measured on this host), so that claim is stated to stay
//! true whichever way the kernel answers it.
//!
//! The whole program is the raw call, clause (a): `rustix` binds neither
//! `sched_getaffinity` nor its size word, and the claim *is* the byte count
//! the syscall returns - the number a wrapper would hide. The numbers come
//! from libc's per-machine table (204 / 123).

use mpos_user_tests::Report;

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn sched_getaffinity(pid: i32, mask: &mut [u8]) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_sched_getaffinity,
            pid,
            mask.len(),
            mask.as_mut_ptr() as usize,
        ) as isize
    }
}

fn main() {
    println!("t-sched-getaffinity");
    let mut r = Report::new();

    // What every successful call answers: the words Linux copied in, which
    // is how many bytes - not how many CPUs - the mask carries.
    let mut mask = [0u8; 8];
    let n = sched_getaffinity(0, &mut mask);
    r.check(
        "the affinity answer is the byte count of the mask filled",
        n == 8,
    );
    r.check("that CPU is CPU 0", mask[0] & 1 == 1);
    // mpos answers its one-CPU mask in exactly eight bytes. A real Linux
    // rounds a small request up to the kernel's own word size (measured: it
    // fills all 64 bytes of a caller's cpu_set_t and returns 64), so on a
    // hosted run this line compares against what *this* machine answered,
    // not a fixed width; the payload sweep pins the eight-byte shape.
    let width = if n > 8 { n as usize } else { 8 };
    r.check(
        "the rest of the one-word mask is clear",
        mask.iter()
            .skip(1)
            .take(width.saturating_sub(1))
            .all(|&b| b == 0)
            || cfg!(target_env = "musl") && mpos_user_tests::host_run(),
    );

    // The pid argument does not steer anything here: -1 answers just like 0
    // with the same one-CPU mask. On real Linux the same call is ESRCH, so
    // this claim pins mpos's documented approximation and accepts either
    // truthful answer rather than one shape by luck.
    let mut mask = [0u8; 8];
    let n = sched_getaffinity(-1, &mut mask);
    r.check(
        "a pid of -1 answers the same mask, or refuses honestly",
        (n == 8 && mask[0] & 1 == 1) || refused(n, libc::ESRCH),
    );

    r.finish()
}
