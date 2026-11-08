//! Linux's two `sched_*attr` doors negotiate the struct by size, but their
//! `size`/`flags` guards are 32-bit words with a specific **order**, and that
//! order is part of the errno a caller branches on (§6.116).
//!
//! Measured on Linux 7.2.14 (`build/scratch/schedattrorder/{q,r,s,order}.c`,
//! musl static, raw `syscall()` so no wrapper truncates a word or pre-validates
//! the buffer). `PAGE_SIZE` is 4096 on both the measuring host and this kernel
//! (`crates/vm/src/lib.rs:33`). The decisive rows:
//!
//! ```text
//! sched_getattr(pid, attr, usize, flags)   [usize is the ARG, not attr.size]
//!   usize 0/1/4/47/4097  -> EINVAL(22) on self, nobody and a negative pid
//!                           alike: the guard beats the pid lookup.
//!   usize 48/56/4096     -> self 0, nobody ESRCH(3)
//!   flags != 0           -> EINVAL(22) on self, but ESRCH(3) on nobody:
//!                           the lookup comes FIRST, flags AFTER it.
//!   usize 0 + flags 1    -> EINVAL(22): the size guard beats flags.
//!   2^32+48 / flags 2^32 -> served (both words truncate to 32 bits).
//!
//! sched_setattr(pid, attr, flags)          [size is attr.size IN the struct]
//!   flags != 0           -> EINVAL(22) on self AND nobody: here flags is
//!                           checked BEFORE the lookup (opposite of getattr).
//!   attr.size 4/47/4097  -> E2BIG(7) on self AND nobody: short/over-page
//!                           sizes are E2BIG, not EINVAL, and beat ESRCH.
//!   attr.size 0          -> self 0 (0 means "use the kernel's version"),
//!                           nobody ESRCH(3); it is NOT refused.
//!   attr.size 48/56/4096 -> self 0, nobody ESRCH(3)
//!   flags 2^32           -> served (truncates to zero).
//! ```
//!
//! Before §6.116 MPOS resolved the pid first, compared the full 64-bit words,
//! refused every short size with `EINVAL`, accepted sizes above a page, put
//! `setattr`'s flags guard after the lookup, and treated `attr.size = 0` as a
//! too-small struct. Every one of those shapes is a row below. Run with
//! `MPOS_ON_HOST=1` the same claims measure Linux, so the pin is the contract
//! on both arms rather than a snapshot of one machine.
//!
//! Out of scope, named and measured but not asserted here: the door-specific
//! EFAULT/ESRCH precedence for a *non-NULL bad* pointer combined with a short
//! size, `sched_getparam`'s priority range, and `rr_get_interval`'s slice.

use mpos_user_tests::Report;

/// 4096, measured as `sysconf(_SC_PAGESIZE)` on the host and fixed by
/// `PAGE_SHIFT = 12` in the kernel. The ceiling is Linux's `PAGE_SIZE`, not a
/// made-up number, so the pin names the value it is testing.
const PAGE: usize = 4096;
const SIZE_V1: u32 = 48;
const NOBODY: libc::c_long = 999_999;
const NEG: libc::c_long = -1;

/// The errno a raw syscall answered: `0` for success, else the host's errno.
/// We pass four varargs to every door; each reads only its own named registers.
unsafe fn raw(
    nr: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
) -> i32 {
    let r = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if r < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

/// A 64 KiB buffer so a large `usize` can never overflow the stack, and every
/// `arm` writes the struct's own `size` field and zeroes the policy/nice.
struct Attr {
    bytes: Vec<u8>,
}
impl Attr {
    fn new() -> Self {
        Attr {
            bytes: vec![0u8; 65536],
        }
    }
    fn arm(&mut self, size: u32) -> libc::c_long {
        self.bytes[..48].fill(0);
        self.bytes[0..4].copy_from_slice(&size.to_ne_bytes());
        // policy (offset 4) = SCHED_OTHER (0), nice (offset 16) = 0.
        self.bytes.as_mut_ptr() as libc::c_long
    }
}

/// One `sched_getattr` call with a freshly armed, valid, writable buffer.
fn getattr(pid: libc::c_long, usize_word: libc::c_long, flags: libc::c_long) -> i32 {
    let mut a = Attr::new();
    let p = a.arm(SIZE_V1);
    unsafe { raw(libc::SYS_sched_getattr, pid, p, usize_word, flags) }
}

/// One `sched_setattr` call; the size lives in the struct, so `arm(size)` is
/// what this door reads. `flags` is the third argument.
fn setattr(pid: libc::c_long, in_struct_size: u32, flags: libc::c_long) -> i32 {
    let mut a = Attr::new();
    let p = a.arm(in_struct_size);
    unsafe { raw(libc::SYS_sched_setattr, pid, p, flags, 0) }
}

/// `sched_getattr` with a deliberately bad non-NULL pointer (`0x1`).
fn getattr_bad(pid: libc::c_long, usize_word: libc::c_long, flags: libc::c_long) -> i32 {
    unsafe { raw(libc::SYS_sched_getattr, pid, 0x1, usize_word, flags) }
}
/// `sched_setattr` with a deliberately bad non-NULL pointer (`0x1`). The
/// in-struct size cannot be read from it, so these rows only test the guards
/// that run before the copy: flags and the NULL/pid guard.
fn setattr_bad(pid: libc::c_long, flags: libc::c_long) -> i32 {
    unsafe { raw(libc::SYS_sched_setattr, pid, 0x1, flags, 0) }
}

fn main() {
    let mut r = Report::new();

    // ---- sched_getattr: the size guard is a 32-bit word, before the lookup ----
    // usize 0/1/4/47 are below the 48-byte first version; 4097 is one past
    // PAGE_SIZE. All five answer EINVAL for *every* pid, including the one
    // that would otherwise be ESRCH. This is the ordering claim.
    let small = [0i64, 1, 4, 47, (PAGE + 1) as i64];
    let small_rows: Vec<i32> = small
        .iter()
        .flat_map(|sz| {
            [
                getattr(0, *sz, 0),
                getattr(NOBODY, *sz, 0),
                getattr(NEG, *sz, 0),
            ]
        })
        .collect();
    r.check(
        &format!("getattr: a short or over-page usize is EINVAL on self, nobody and a negative pid ({small_rows:?})"),
        small_rows.iter().all(|e| *e == 22),
    );

    // The legal sizes let the lookup happen: self succeeds, nobody is ESRCH,
    // a negative pid stays EINVAL from §6.114's verdict.
    let legal = [48i64, 56, PAGE as i64];
    let legal_self: Vec<i32> = legal.iter().map(|sz| getattr(0, *sz, 0)).collect();
    let legal_nobody: Vec<i32> = legal.iter().map(|sz| getattr(NOBODY, *sz, 0)).collect();
    r.check(
        &format!("getattr: legal usizes serve self ({legal_self:?}) and give nobody ESRCH ({legal_nobody:?})"),
        legal_self.iter().all(|e| *e == 0) && legal_nobody.iter().all(|e| *e == 3),
    );

    // The high 32 bits are dropped: 2^32+48 is a legal 48, 2^32+0 is short.
    let trunc_size = getattr(0, 0x1_0000_0000i64 + 48, 0);
    let trunc_size_zero = getattr(0, 0x1_0000_0000i64, 0);
    r.check(
        &format!("getattr: usize is read as a 32-bit word (2^32+48 -> {trunc_size}, 2^32+0 -> {trunc_size_zero})"),
        trunc_size == 0 && trunc_size_zero == 22,
    );

    // ---- sched_getattr: flags is checked AFTER the pid lookup -----------------
    let f_self = getattr(0, 48, 1);
    let f_nobody = getattr(NOBODY, 48, 1);
    let f_neg = getattr(NEG, 48, 1);
    r.check(
        &format!("getattr: nonzero flags is EINVAL on self but ESRCH on nobody (self {f_self}, nobody {f_nobody}, neg {f_neg})"),
        f_self == 22 && f_nobody == 3 && f_neg == 22,
    );

    // flags is also a 32-bit word: 2^32 truncates to zero (served), 2^32+1 to
    // one (EINVAL). And the size guard beats flags: usize 0 + flags 1 is EINVAL.
    let f_zero = getattr(0, 48, 0x1_0000_0000i64);
    let f_one = getattr(0, 48, 0x1_0000_0001i64);
    let size_beats = getattr(NOBODY, 0, 1);
    r.check(
        &format!("getattr: flags truncates to 32 bits (2^32 -> {f_zero}, 2^32+1 -> {f_one}) and size beats flags (nobody,size0,flags1 -> {size_beats})"),
        f_zero == 0 && f_one == 22 && size_beats == 22,
    );

    // A bad pointer is EFAULT once the guards pass, but a nonzero flags word
    // still wins EINVAL over it (flags runs before the copy).
    let bad_ok = getattr_bad(0, 48, 0);
    let bad_flags = getattr_bad(0, 48, 1);
    r.check(
        &format!("getattr: a bad pointer is EFAULT ({bad_ok}) unless flags wins EINVAL first ({bad_flags})"),
        bad_ok == 14 && bad_flags == 22,
    );

    // ---- sched_setattr: flags is checked BEFORE the lookup -------------------
    let s_flags = [
        setattr(0, SIZE_V1, 1),
        setattr(NOBODY, SIZE_V1, 1),
        setattr(NEG, SIZE_V1, 1),
    ];
    r.check(
        &format!(
            "setattr: nonzero flags is EINVAL on self, nobody and a negative pid ({s_flags:?})"
        ),
        s_flags.iter().all(|e| *e == 22),
    );
    let s_flags_trunc = setattr(0, SIZE_V1, 0x1_0000_0000i64);
    let s_flags_one = setattr(0, SIZE_V1, 0x1_0000_0001i64);
    r.check(
        &format!(
            "setattr: flags truncates to 32 bits (2^32 -> {s_flags_trunc}, 2^32+1 -> {s_flags_one})"
        ),
        s_flags_trunc == 0 && s_flags_one == 22,
    );

    // ---- sched_setattr: short / over-page in-struct size is E2BIG ------------
    // 0 < size < 48, or size > PAGE_SIZE, answers E2BIG(7) on self AND on
    // nobody — the guard beats the pid lookup, and it is NOT EINVAL.
    let bad_sizes = [1u32, 4, 47, (PAGE + 1) as u32, u32::MAX];
    let e2big_rows: Vec<i32> = bad_sizes
        .iter()
        .flat_map(|sz| [setattr(0, *sz, 0), setattr(NOBODY, *sz, 0)])
        .collect();
    r.check(
        &format!("setattr: a short or over-page in-struct size is E2BIG on self and nobody ({e2big_rows:?})"),
        e2big_rows.iter().all(|e| *e == 7),
    );

    // size 0 means "use the kernel's version": it is served on self and only
    // answered ESRCH on nobody — never refused as too small.
    let zero_self = setattr(0, 0, 0);
    let zero_nobody = setattr(NOBODY, 0, 0);
    r.check(
        &format!("setattr: in-struct size 0 is the kernel default (self {zero_self}, nobody {zero_nobody})"),
        zero_self == 0 && zero_nobody == 3,
    );

    let good_sizes = [48u32, 56, PAGE as u32];
    let good_self: Vec<i32> = good_sizes.iter().map(|sz| setattr(0, *sz, 0)).collect();
    let good_nobody: Vec<i32> = good_sizes
        .iter()
        .map(|sz| setattr(NOBODY, *sz, 0))
        .collect();
    r.check(
        &format!("setattr: legal in-struct sizes serve self ({good_self:?}) and give nobody ESRCH ({good_nobody:?})"),
        good_self.iter().all(|e| *e == 0) && good_nobody.iter().all(|e| *e == 3),
    );

    // A bad pointer is EFAULT, but a nonzero flags word wins EINVAL over it,
    // and flags also beats the EFAULT/ESRCH on nobody (the guard is first).
    let s_bad_ok = setattr_bad(0, 0);
    let s_bad_flags = setattr_bad(NOBODY, 1);
    r.check(
        &format!("setattr: a bad pointer is EFAULT ({s_bad_ok}) and flags wins over nobody's lookup ({s_bad_flags})"),
        s_bad_ok == 14 && s_bad_flags == 22,
    );

    r.finish();
}
