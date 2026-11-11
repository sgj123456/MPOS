//! `getcpu`'s two output pointers are written independently.
//!
//! The rule this pins is not "what does `getcpu` answer" — one core, node
//! zero, so both numbers are 0 and there is nothing to see move. It is *how
//! many of the two pointers get written when one of them cannot be*: Linux
//! performs two separate `put_user`s and or's the errors, so a bad `node`
//! still leaves the caller's `cpu` filled in, and a bad `cpu` still leaves
//! `node` filled in. Measured on the host (`build/scratch/getcpu/r.c`, both
//! buffers reset to a sentinel before each call):
//!
//! ```text
//! (good,BAD) ret=-1 errno=14 cpu=0x0000000b node=0xbbbbbbbb  -> cpu WRITTEN
//! (BAD,good) ret=-1 errno=14 cpu=0xaaaaaaaa node=0x00000000  -> node WRITTEN
//! (BAD,BAD)  ret=-1 errno=14 cpu=0xaaaaaaaa node=0xbbbbbbbb  -> neither
//! ```
//!
//! and a NULL pointer is not an error at all — `(NULL, good)` answers 0 and
//! writes only `node`, `(NULL, NULL)` answers 0 and writes nothing
//! (`build/scratch/getcpu/q.c`). The third parameter, `tcache`, is the one the
//! man page retires: Linux never reads it, so a `tcache` at address 1 answers
//! 0 both when the outputs are readable and when they are NULL.
//!
//! A kernel that checks both pointers before writing either answers the same
//! `EFAULT` and loses the half it could have written, which is the shape this
//! program exists to distinguish: the claims below assert *both* the errno and
//! the side effect, because the side effect is the half that was wrong.

use mpos_user_tests::Report;

/// An address that is mapped on neither the host nor this machine: page zero
/// is never mapped, so a `put_user` to it faults. `1usize` is the habit the
/// other pins in this suite use (`t-itimer-order.rs`, `t-timer-order.rs`).
const BAD: usize = 1;

/// The sentinel each buffer starts at, so "was it written" is a comparison
/// rather than an assumption. `getcpu` writes a `u32` either way, and 0 is a
/// legal answer, so the sentinel has to be something 0 can be told apart from.
const SENTINEL: u32 = 0xAAAA_AAAA;

const OK: i32 = 0;
const EFAULT: i32 = 14;

fn raw(a1: usize, a2: usize, a3: usize) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(libc::SYS_getcpu, a1, a2, a3) };
    if ret == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as isize)
    }
}

fn outcome(r: Result<isize, i32>) -> i32 {
    match r {
        Ok(_) => OK,
        Err(e) => e,
    }
}

/// One call with both output buffers reset to the sentinel first. Returns the
/// errno and what each buffer holds afterwards — the pair, because either half
/// alone is satisfied by the wrong kernel.
fn probe(cpu: usize, node: usize) -> (i32, u32, u32) {
    let mut c = SENTINEL;
    let mut n = SENTINEL;
    let errno = outcome(raw(cpu, node, 0));
    // Read through the pointers the call actually saw. When a pointer was BAD
    // the kernel must not have touched it, so reading our own local is the
    // right question: did the call leave the buffer alone?
    if cpu != 0 && cpu != BAD {
        c = unsafe { *(cpu as *const u32) };
    }
    if node != 0 && node != BAD {
        n = unsafe { *(node as *const u32) };
    }
    (errno, c, n)
}

fn want(report: &mut Report, label: &str, got: i32, expect: i32) {
    report.check(
        &format!("{label} (got {got}, want {expect})"),
        got == expect,
    );
}

fn main() {
    let mut r = Report::new();

    // The plain shape both kernels must agree on, so the rest of the file is
    // read against a known-good call.
    let mut cpu = SENTINEL;
    let mut node = SENTINEL;
    let good = outcome(raw(
        &mut cpu as *mut u32 as usize,
        &mut node as *mut u32 as usize,
        0,
    ));
    want(&mut r, "getcpu(self, self) answers 0", good, OK);
    r.check(
        &format!("and it filled both words (cpu {cpu:#010x}, node {node:#010x})"),
        cpu != SENTINEL && node != SENTINEL,
    );

    // A readable `cpu` beside an unreadable `node`: the errno is EFAULT and the
    // cpu word is written anyway. This is the claim the old kernel fails — it
    // answered the right errno and wrote neither word.
    let mut c = SENTINEL;
    let (errno, seen_cpu, _) = probe(&mut c as *mut u32 as usize, BAD);
    want(
        &mut r,
        "an unreadable node beside a good cpu is EFAULT",
        errno,
        EFAULT,
    );
    r.check(
        &format!("and the good cpu was written anyway ({seen_cpu:#010x})"),
        seen_cpu != SENTINEL,
    );

    // The mirror image, which is what proves the rule is "each pointer on its
    // own" rather than "cpu is special".
    let mut n = SENTINEL;
    let (errno, _, seen_node) = probe(BAD, &mut n as *mut u32 as usize);
    want(
        &mut r,
        "an unreadable cpu beside a good node is EFAULT",
        errno,
        EFAULT,
    );
    r.check(
        &format!("and the good node was written anyway ({seen_node:#010x})"),
        seen_node != SENTINEL,
    );

    // Both unreadable: the errno is the same and nothing can have been written.
    let (errno, _, _) = probe(BAD, BAD);
    want(
        &mut r,
        "two unreadable pointers are still just EFAULT",
        errno,
        EFAULT,
    );

    // NULL is "do not tell me that half", not an error.
    let mut n = SENTINEL;
    let errno = outcome(raw(0, &mut n as *mut u32 as usize, 0));
    want(&mut r, "a NULL cpu is not an error", errno, OK);
    r.check(
        &format!("and it still filled node ({n:#010x})"),
        n != SENTINEL,
    );

    let mut c = SENTINEL;
    let errno = outcome(raw(&mut c as *mut u32 as usize, 0, 0));
    want(&mut r, "a NULL node is not an error", errno, OK);
    r.check(
        &format!("and it still filled cpu ({c:#010x})"),
        c != SENTINEL,
    );

    want(
        &mut r,
        "two NULLs are a legal query",
        outcome(raw(0, 0, 0)),
        OK,
    );

    // `tcache` is retired: Linux never reads it, so its being unreadable says
    // nothing about the call.
    want(
        &mut r,
        "an unreadable tcache with both outputs NULL is ignored",
        outcome(raw(0, 0, BAD)),
        OK,
    );
    want(
        &mut r,
        "an unreadable tcache with both outputs readable is ignored",
        outcome(raw(
            &mut cpu as *mut u32 as usize,
            &mut node as *mut u32 as usize,
            BAD,
        )),
        OK,
    );

    // The value is the processor the caller ran on. One CPU means zero, and
    // zero is also the sentinel's neighbour, so this is asserted as "the word
    // is not the sentinel" above; here the interesting part is that a second
    // call agrees with the first (a kernel answering a counter rather than a
    // processor would drift).
    let mut first = SENTINEL;
    let mut second = SENTINEL;
    let _ = raw(&mut first as *mut u32 as usize, 0, 0);
    let _ = raw(&mut second as *mut u32 as usize, 0, 0);
    r.check(
        &format!("the processor is stable across calls ({first}, {second})"),
        first == second,
    );

    r.finish();
}
