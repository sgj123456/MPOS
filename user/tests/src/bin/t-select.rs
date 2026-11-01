//! t-select - `select(2)`'s count is a right, not a size; its sets are judged
//! by what the machine can hold.
//!
//! Pinned here, against Linux 7.2.11 measured on the host
//! (`build/scratch/sel/`, `docs/notes/` §10.14):
//!
//! * `nfds` is **never** refused for being past `FD_SETSIZE`. That number is a
//!   libc constant, not a kernel limit: Linux serves 1024, 1025, 2^20 and even
//!   2^40 against a mapped set, and refuses only a **negative** count.
//! * What it actually bounds is the descriptor table's capacity rounded up to
//!   a power of two (minimum 64). Bits at or above that are never read, never
//!   reported and never cleared - so a caller may name a hundred million
//!   descriptors and still pass an array only as large as its own fd table.
//! * The write-back width follows that same bound, not the caller's `nfds`.
//! * The timeout is judged **before** the count: a negative `nfds` with an
//!   unmapped timeval is `EFAULT`, and with `tv_sec = -1` it is `EINVAL`.
//! * `select` folds an over-unit `tv_usec` into seconds rather than refusing
//!   it (`{0, 1_000_000}` sleeps a second); `pselect6` refuses `tv_nsec` of a
//!   billion or more with `EINVAL`; both refuse a negative field.
//! * A signal ends the wait with `EINTR`, the sets untouched but the **time
//!   left rewritten** in the caller's own unit.
//!
//! # The migration
//!
//! `rustix` has no `select` at all, and libc's `fd_set` is 1024 bits - too
//! short to state any of the claims above. So the doors go through
//! `libc::syscall(SYS_select, ...)` with sets this program lays out itself, in
//! the kernel's own byte shape: one bit per descriptor, 64 bits per word.
//!
//! Run as `MPOS_ON_HOST=1 ./target/x86_64-unknown-linux-gnu/debug/t-select` it
//! is a reference against the host's own select(2).

use mpos_user_tests::{Report, host_run};
use std::time::Instant;

/// Linux's `FD_SETSIZE`. Not a kernel limit - the number this program is
/// mostly about proving the kernel does *not* use.
const FD_SETSIZE: usize = 1024;

/// The clamp floor: with nothing open the table has no slots, and Linux still
/// reads one word (64 bits / 8 bytes).
const CLAMP_FLOOR: usize = 64;

fn set_bit(set: &mut [u8], fd: usize) {
    set[fd / 8] |= 1 << (fd % 8);
}

fn bit_is(set: &[u8], fd: usize) -> bool {
    set[fd / 8] >> (fd % 8) & 1 == 1
}

/// The machine has no `select` number of its own on aarch64 - the asm-generic
/// table never had one, exactly as Linux's arm64 does not (`src/syscall/
/// linux/nr_aarch64.rs` records it as `unreachable on aarch64`). libc there
/// emulates `select` on top of `pselect6`, turning the `timeval` into a
/// `timespec` first, so this program does the same rather than claiming a
/// door the machine does not have.
const HAS_SELECT: bool = cfg!(target_arch = "x86_64");

/// `select(2)` with raw sets, answering `(rc, errno)`.
fn select_raw(
    nfds: libc::c_int,
    read: Option<&mut [u8]>,
    write: Option<&mut [u8]>,
    except: Option<&mut [u8]>,
    mut timeout: Option<&mut [i64; 2]>,
) -> (isize, i32) {
    let ptr = |s: Option<&mut [u8]>| -> usize {
        match s {
            Some(s) if !s.is_empty() => s.as_mut_ptr() as usize,
            _ => 0,
        }
    };
    let (r, w, e) = (ptr(read), ptr(write), ptr(except));
    // `as_mut_ptr` *moves* the `&mut [i64; 2]`, and the aarch64 arm needs the
    // same buffer a second time to convert it. Borrow here and let the arm
    // that needs the original take it.
    #[cfg(target_arch = "x86_64")]
    let t = match timeout.as_mut() {
        Some(t) => t.as_mut_ptr() as usize,
        None => 0,
    };
    #[cfg(target_arch = "x86_64")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_select,
            nfds as libc::c_long,
            r as libc::c_long,
            w as libc::c_long,
            e as libc::c_long,
            t as libc::c_long,
        )
    };
    // The emulation libc performs on arm64: microseconds become nanoseconds,
    // and `exceptfds` has no `pselect6` argument to carry it.
    #[cfg(not(target_arch = "x86_64"))]
    let rc = {
        let mut ns = match timeout.as_ref() {
            Some(tv) => Some([tv[0], tv[1] * 1_000]),
            None => None,
        };
        let tp = match ns.as_mut() {
            Some(t) => t.as_mut_ptr() as usize,
            None => 0,
        };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pselect6,
                nfds as libc::c_long,
                r as libc::c_long,
                w as libc::c_long,
                e as libc::c_long,
                tp as libc::c_long,
                0 as libc::c_long,
            )
        };
        // libc's own emulation converts the answer back, or the caller reads
        // a `timespec` through a `timeval`'s field. The copy is unconditional
        // on purpose: a served call (and one interrupted by a signal) leaves
        // the remainder there, and a refused one - `EINVAL` for an over-unit
        // field - leaves the buffer untouched, so converting it back is the
        // identity and costs nothing.
        if let (Some(ns), Some(tv)) = (ns, timeout.as_mut()) {
            tv[0] = ns[0];
            tv[1] = ns[1] / 1_000;
        }
        rc
    };
    // errno is only written when a call fails and is never cleared by
    // success, so a served call must report 0 - not whatever the call before
    // it left behind.
    let e = if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    };
    (rc as isize, e)
}

/// `pselect6(2)`; `sigmask` stays null, which is the shape this program needs.
fn pselect_raw(
    nfds: libc::c_int,
    read: Option<&mut [u8]>,
    timeout: Option<&mut [i64; 2]>,
) -> (isize, i32) {
    let ptr = |s: Option<&mut [u8]>| -> usize {
        match s {
            Some(s) if !s.is_empty() => s.as_mut_ptr() as usize,
            _ => 0,
        }
    };
    let r = ptr(read);
    let t = match timeout.as_ref() {
        Some(t) => t.as_ptr() as usize,
        None => 0,
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pselect6,
            nfds as libc::c_long,
            r as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            t as libc::c_long,
            0 as libc::c_long,
        )
    };
    // errno is only written when a call fails and is never cleared by
    // success, so a served call must report 0 - not whatever the call before
    // it left behind.
    let e = if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    };
    (rc as isize, e)
}

/// Call the counted door with `nfds = -1` and an unmapped timeout pointer, so
/// the answer says which of the two the kernel judged first. Split by
/// `cfg` rather than by a runtime branch: `SYS_select` does not exist in
/// libc's aarch64 table at all, so a dead arm would still not compile.
#[cfg(target_arch = "x86_64")]
fn order_probe(tvp: usize) -> (libc::c_long, i32) {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_select,
            -1 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            tvp as libc::c_long,
        )
    };
    (
        rc,
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )
}

#[cfg(not(target_arch = "x86_64"))]
fn order_probe(tvp: usize) -> (libc::c_long, i32) {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pselect6,
            -1 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            tvp as libc::c_long,
            0 as libc::c_long,
        )
    };
    (
        rc,
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )
}

fn pipe_pair() -> (libc::c_int, libc::c_int) {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "a pipe is a door this machine opens");
    (fds[0], fds[1])
}

/// Open `n` descriptors so the table's capacity is known, returning them.
///
/// The clamp is the table's high-water mark rounded up, so the only way to
/// make a claim about it is to have opened something. `/dev/null` keeps the
/// descriptors quiet - a pipe end would become ready and end the wait.
fn open_quiet(n: usize) -> Vec<libc::c_int> {
    let mut held = Vec::new();
    for _ in 0..n {
        let fd = unsafe { libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDONLY) };
        if fd < 0 {
            break;
        }
        held.push(fd);
    }
    held
}

extern "C" fn on_alarm(_: libc::c_int) {}

fn main() {
    println!("t-select");
    let mut r = Report::new();

    // ---- the count is a right -------------------------------------------
    //
    // A set as large as the biggest count any caller could write into it, so
    // that a refusal can only be about the number and never about the bytes.
    let mut widest = vec![0u8; FD_SETSIZE / 8];
    // A zero timeout, not "none": with nothing ready and no clock, a served
    // call parks forever, and this program has claims still to make.
    let mut timeout = [0i64; 2];
    let (rc, e) = select_raw(
        (FD_SETSIZE + 8) as libc::c_int,
        Some(&mut widest),
        None,
        None,
        Some(&mut timeout),
    );
    r.check(
        "a count past FD_SETSIZE is served, not refused",
        rc >= 0 && e == 0,
    );

    // 2^20 descriptors named against an 8-byte clamp: Linux answers 0 and
    // reads nothing above the table.
    let mut small = [0u8; 8];
    let mut timeout = [0i64; 2];
    let (rc, e) = select_raw(1 << 20, Some(&mut small), None, None, Some(&mut timeout));
    r.check(
        "a count a million past the table is still served",
        rc >= 0 && e == 0,
    );

    let (rc, e) = select_raw(-1, None, None, None, None);
    r.check(
        "a negative count is refused with EINVAL",
        rc == -1 && e == libc::EINVAL,
    );

    // ---- bits above the clamp are not ours to see ------------------------
    //
    // Three descriptors open -> the table holds 3 slots -> the clamp is 64.
    // Bit 2000 sits far above it: the kernel must answer 0 and leave the byte
    // exactly as it arrived.
    let held = open_quiet(3);
    let mut wide = vec![0u8; 256];
    set_bit(&mut wide, 2000);
    let mut timeout = [0i64; 2];
    let (rc, e) = select_raw(2048, Some(&mut wide), None, None, Some(&mut timeout));
    r.check(
        "a set naming a bit far above the table answers 0",
        rc == 0 && e == 0,
    );
    r.check(
        "and that bit is left exactly as the caller wrote it",
        bit_is(&wide, 2000),
    );
    r.check(
        "and the bytes above the table are not cleared",
        wide[250] == 0x01,
    );

    // A closer marked byte: everything past the clamp must survive even when
    // the call serves something inside it.
    let (read_end, write_end) = pipe_pair();
    let mut marked = [0u8; 8 + 8];
    set_bit(&mut marked, write_end as usize);
    marked[8] = 0x5a;
    let mut timeout = [0i64; 2];
    let (rc, e) = select_raw(64, None, Some(&mut marked), None, Some(&mut timeout));
    r.check(
        "a writable descriptor is reported through the write set",
        rc == 1 && e == 0 && bit_is(&marked, write_end as usize),
    );
    r.check(
        "and the word past the caller's count is left alone",
        marked[8] == 0x5a,
    );

    // ---- a dead descriptor inside the table ------------------------------
    //
    // The bit has to sit **below** the clamp or the kernel never looks at it:
    // with three descriptors open the table holds 64, so fd 7 is examined and
    // fd 900 is not. Measured on 7.2.11 (`build/scratch/sel/s4.c`): bit 7 with
    // `nfds = 8` answers `EBADF`, bit 2000 with `nfds = 2048` answers 0.
    // A descriptor this program has just given back, so "closed" is a fact
    // and not an assumption about which numbers the runtime happens to use.
    let gone = unsafe { libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDONLY) };
    assert!(gone >= 0 && gone < 64, "a free descriptor below the clamp");
    unsafe { libc::close(gone) };
    let mut dead = [0u8; 8];
    set_bit(&mut dead, gone as usize);
    let mut timeout = [0i64; 2];
    let (rc, e) = select_raw(gone + 1, Some(&mut dead), None, None, Some(&mut timeout));
    r.check(
        "a closed descriptor inside the table is EBADF for the whole call",
        rc == -1 && e == libc::EBADF,
    );

    // ---- the timeout, in both shapes -------------------------------------
    let start = Instant::now();
    let mut timeout = [0i64, 250_000];
    let (rc, e) = select_raw(0, None, None, None, Some(&mut timeout));
    let ms = start.elapsed().as_millis() as u64;
    r.check(
        "a zero count with a 250 ms timeout answers 0",
        rc == 0 && e == 0,
    );
    r.check("and it slept the timeout it was given", ms >= 150);
    r.check(
        "and the timeout is reported spent",
        timeout[0] == 0 && timeout[1] == 0,
    );

    // `select` folds an over-unit microsecond field into seconds instead of
    // refusing it - its own shape, measured on x86_64 (`build/scratch/sel/
    // t3.c`: `{0, 2_000_000}` parks 2.002 s). aarch64 has no `select` syscall
    // to have the shape; libc's emulation multiplies into nanoseconds, where
    // `pselect6` refuses a field of a second or more.
    #[cfg(target_arch = "x86_64")]
    {
        let start = Instant::now();
        let mut timeout = [0i64, 1_000_000];
        let (rc, e) = select_raw(0, None, None, None, Some(&mut timeout));
        let ms = start.elapsed().as_millis() as u64;
        r.check(
            "a tv_usec of one second is folded into seconds, not refused",
            rc == 0 && e == 0 && ms >= 900,
        );
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut timeout = [0i64, 1_000_000];
        let (rc, e) = select_raw(0, None, None, None, Some(&mut timeout));
        r.check(
            "an emulated select's over-unit tv_usec meets pselect6's refusal",
            rc == -1 && e == libc::EINVAL,
        );
    }

    let mut timeout = [-1i64, 0];
    let (rc, e) = select_raw(0, None, None, None, Some(&mut timeout));
    r.check(
        "a negative tv_sec is refused with EINVAL",
        rc == -1 && e == libc::EINVAL,
    );

    let mut timeout = [0i64, -1];
    let (rc, e) = select_raw(0, None, None, None, Some(&mut timeout));
    r.check(
        "a negative tv_usec is refused with EINVAL",
        rc == -1 && e == libc::EINVAL,
    );

    let mut ns = [0i64, 999_999_999];
    let (rc, e) = pselect_raw(0, None, Some(&mut ns));
    r.check(
        "pselect6 accepts a tv_nsec just below a second",
        rc == 0 && e == 0,
    );

    let mut ns = [0i64, 1_000_000_000];
    let (rc, e) = pselect_raw(0, None, Some(&mut ns));
    r.check(
        "pselect6 refuses a tv_nsec of a full second with EINVAL",
        rc == -1 && e == libc::EINVAL,
    );

    // ---- the timeout is judged before the count --------------------------
    //
    // An unmapped timeval pointer: the caller fault wins over the bad count.
    let unmapped = 0x1000usize;
    let (rc, e) = order_probe(unmapped);
    r.check(
        "a bad count with an unmapped timeval is EFAULT, not EINVAL",
        rc == -1 && e == libc::EFAULT,
    );

    // ---- a signal ends the wait with the clock written back --------------
    //
    // The sets keep the interest they arrived with (Linux leaves them alone on
    // EINTR) but the time left is the caller's to read. `alarm` is a whole
    // second, so the wait is given three.
    unsafe { libc::signal(libc::SIGALRM, on_alarm as libc::sighandler_t) };
    let mut interest = [0u8; 8];
    set_bit(&mut interest, read_end as usize);
    let mut timeout = [3i64, 0];
    unsafe { libc::alarm(1) };
    let start = Instant::now();
    let (rc, e) = select_raw(
        read_end + 1,
        Some(&mut interest),
        None,
        None,
        Some(&mut timeout),
    );
    let ms = start.elapsed().as_millis() as u64;
    unsafe { libc::alarm(0) };
    r.check(
        "a signal ends the wait with EINTR",
        rc == -1 && e == libc::EINTR && ms >= 900 && ms < 3000,
    );
    r.check(
        "and the interest bit survives the interruption",
        bit_is(&interest, read_end as usize),
    );
    // The contract is that the remainder is **written back**, not that it has
    // one particular value: the signal lands somewhere inside the second, so
    // Linux answers `{1, 999934}` on this host while a machine with a coarser
    // tick reports a figure a little closer to the full two seconds left. What
    // must not survive is the `{3, 0}` the caller sent.
    if timeout[0] >= 3 || (timeout[0] <= 0 && timeout[1] <= 0) {
        println!("   (tv={{ {}, {} }})", timeout[0], timeout[1]);
    }
    r.check(
        "and the time left is written back, not the timeout the caller sent",
        timeout[0] < 3 && (timeout[0] > 0 || timeout[1] > 0),
    );

    // ---- the clamp grows with the table, and the bytes follow it ---------
    //
    // 120 descriptors open pushes the table past one word, so the clamp is
    // 128 (16 bytes). A caller naming 2048 descriptors would need 256 bytes if
    // the kernel sized its copy from the count; because it sizes from the
    // table, a 16-byte set is enough and the call is served. No bits are set:
    // nothing can be ready, so nothing can be EBADF.
    let more = open_quiet(120);
    let mut grown = [0u8; 16];
    let mut timeout = [0i64; 2];
    let (rc, e) = select_raw(2048, Some(&mut grown), None, None, Some(&mut timeout));
    r.check(
        "a count far past the table needs only the bytes the table holds",
        rc == 0 && e == 0,
    );

    drop(more);
    for fd in held {
        unsafe { libc::close(fd) };
    }
    unsafe {
        libc::close(read_end);
        libc::close(write_end);
    }

    r.finish();
}
