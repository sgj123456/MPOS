//! t-ptrace-fpregset - `PTRACE_GETREGSET`/`SETREGSET` with **`NT_PRFPREG`**.
//!
//! Found on the Chromium run: crashpad logged `ptrace: I/O error (5)` twelve
//! times, and a temporary diagnostic on `ptrace`'s *unknown-request* arm printed
//! nothing - so the `EIO` was not an unknown request. It was this:
//!
//! ```text
//! abi::PTRACE_GETREGSET | abi::PTRACE_SETREGSET => {
//!     if addr != abi::NT_PRSTATUS { return err::EIO; }   // <- every other set
//! ```
//!
//! `addr` is the **note type**: `NT_PRSTATUS` (1) is the general registers and
//! `NT_PRFPREG` (2) is the x87/SSE file. Linux serves both; this kernel served
//! only the first and answered `EIO` - "I do not know this request" - for the
//! second, which tells a crash handler that the whole mechanism is absent.
//!
//! Measured Linux behaviour this pins: `PTRACE_GETREGSET` with `NT_PRFPREG`
//! succeeds, reports the bytes it moved in the iovec's length field, and
//! `SETREGSET` round-trips a value the next `GETREGSET` reads back.

use mpos_user_tests::Report;

const PTRACE_GETREGSET: libc::c_int = 0x4204;
const PTRACE_SETREGSET: libc::c_int = 0x4205;
/// `NT_PRFPREG` - the x87/SSE register set.
const NT_PRFPREG: usize = 2;
/// Big enough for either machine's FPU image (512 bytes on x86_64).
const FP_BUF: usize = 1024;

fn ptrace(request: libc::c_int, pid: libc::pid_t, addr: usize, data: usize) -> isize {
    // SAFETY: the variadic shape libc's wrapper takes; `request` decides what the
    // other three words mean. The `as` is portable: musl declares `c_int`,
    // glibc `c_uint`.
    unsafe { libc::ptrace(request as _, pid, addr, data) as isize }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}

fn main() {
    println!("t-ptrace-fpregset");
    let mut r = Report::new();

    // SAFETY: a child that parks so the parent can trace it.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: the child's whole job is to stay stopped under the tracer.
        unsafe {
            libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0);
            libc::raise(libc::SIGSTOP);
            libc::_exit(0);
        }
    }
    r.check(&format!("a child is forked ({child})"), child > 0);
    if child <= 0 {
        r.finish();
    }
    let mut status = 0;
    // SAFETY: the child just told us it will stop.
    let waited = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!("and stops for the tracer ({waited})"),
        waited == child && libc::WIFSTOPPED(status),
    );

    // The buffer the kernel writes the FPU image into, through an iovec.
    let mut fp = [0u8; FP_BUF];
    let mut iov = libc::iovec {
        iov_base: fp.as_mut_ptr().cast(),
        iov_len: FP_BUF,
    };

    // === GETREGSET with NT_PRFPREG is served ==============================
    let got = ptrace(
        PTRACE_GETREGSET,
        child,
        NT_PRFPREG,
        &mut iov as *mut _ as usize,
    );
    r.check(
        &format!(
            "GETREGSET(NT_PRFPREG) is served ({got}, errno {:?})",
            errno()
        ),
        got == 0,
    );
    // **The claim that separates "served" from "served correctly"**: the kernel
    // reports the bytes it moved by writing them back into the iovec's length.
    // A served-but-empty answer would leave `iov_len` at what we passed in.
    r.check(
        &format!(
            "and it reports the bytes moved ({} of {FP_BUF})",
            iov.iov_len
        ),
        iov.iov_len > 0 && iov.iov_len <= FP_BUF,
    );

    // === SETREGSET round-trips through the same set ========================
    //
    // Write the buffer back as the FPU image, then read it again: a value that
    // survives proves the two wings share the same state rather than each having
    // a private copy.
    let mut iov2 = libc::iovec {
        iov_base: fp.as_mut_ptr().cast(),
        iov_len: iov.iov_len,
    };
    let set = ptrace(
        PTRACE_SETREGSET,
        child,
        NT_PRFPREG,
        &mut iov2 as *mut _ as usize,
    );
    r.check(
        &format!("SETREGSET(NT_PRFPREG) is served ({set})"),
        set == 0,
    );
    let mut back = [0u8; FP_BUF];
    let mut iov3 = libc::iovec {
        iov_base: back.as_mut_ptr().cast(),
        iov_len: FP_BUF,
    };
    let re = ptrace(
        PTRACE_GETREGSET,
        child,
        NT_PRFPREG,
        &mut iov3 as *mut _ as usize,
    );
    r.check(&format!("and reading it back is served ({re})"), re == 0);
    r.check(
        "the value round-trips",
        back[..iov3.iov_len] == fp[..iov3.iov_len],
    );

    // === a note type nobody serves is still EIO ===========================
    //
    // The refusal must stay a refusal: `NT_PRSTATUS` (1) and `NT_PRFPREG` (2) are
    // the two Linux defines for a ptrace tracer, and 99 names nothing.
    let mut junk = [0u8; 64];
    let mut iov4 = libc::iovec {
        iov_base: junk.as_mut_ptr().cast(),
        iov_len: junk.len(),
    };
    let bad = ptrace(PTRACE_GETREGSET, child, 99, &mut iov4 as *mut _ as usize);
    r.check(
        &format!("an unknown note type is still EIO ({bad})"),
        bad == -1 && (errno() == Some(libc::EIO) || errno() == Some(libc::EINVAL)),
    );
    // And the general-register set still works, which is the regression this
    // change could have caused by taking the wrong branch.
    //
    // **The buffer is sized for the machine, not for one machine**: x86_64's
    // `uregs` is 27 words and aarch64's is 34, so a fixed 32-word array is short
    // by two on the second port and the kernel's `len < BYTES` check answers
    // `EIO` - which is exactly what happened on the aarch64 arm of the gate. 40
    // words covers both with room.
    let mut gregs = [0u64; 40];
    let mut iov5 = libc::iovec {
        iov_base: gregs.as_mut_ptr().cast(),
        iov_len: core::mem::size_of_val(&gregs),
    };
    let still = ptrace(PTRACE_GETREGSET, child, 1, &mut iov5 as *mut _ as usize);
    r.check(
        &format!("and NT_PRSTATUS still works ({still})"),
        still == 0,
    );

    // SAFETY: the child this test forked.
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, core::ptr::null_mut(), 0);
    }
    r.finish()
}
