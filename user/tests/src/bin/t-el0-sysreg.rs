//! t-el0-sysreg - the two system instructions a JIT's code flush is built from
//! are *answered* by the kernel at EL0 rather than killing the program.
//!
//! Both are aarch64's, and both are trapped rather than executed because this
//! machine's `SCTLR_EL1` leaves `UCT` and `UCI` clear - as Linux leaves them, on
//! purpose: the trap is what lets the kernel answer with the value or the
//! operation the *system* agrees on instead of the one this CPU happens to
//! carry. Linux's `do_el0_sys` walks a table of them
//! (`arch/arm64/kernel/traps.c`), and the pair here is the pair a code flush
//! reaches, in the order it reaches them:
//!
//! * `mrs Xt, ctr_el0` - the cache type register, whose `DminLine`/`IminLine`
//!   fields are the line sizes a flush strides by. `llvmpipe` - the software GL
//!   behind this image's weston - died on exactly this instruction: measured on
//!   the aarch64 desktop image, `user fault ... ESR 0x6232c061`, which is
//!   `EC 0x18` with `ISS 0x32c061`: a *read* of `op0=3, op1=3, CRn=0, CRm=0,
//!   op2=1` into `Rt = x3`, i.e. `mrs x3, ctr_el0`.
//! * `dc cvau, Xt` and `ic ivau, Xt` - the clean and the instruction-cache
//!   invalidate a flush issues over the code it has just written. With the read
//!   answered, the very next trap of the same flush was measured as
//!   `ESR 0x6212dc56` = `SYS #3, C7, C11, #1` - `DC CVAU`.
//!
//! Each half is stated so that a wrong answer cannot pass as a right one:
//!
//! * The register is read twice into two *different* registers, each seeded with
//!   a sentinel a real `CTR_EL0` can never hold. The kernel learns which register
//!   to answer from the trap's `ISS[9:5]`, and a single read cannot see a wrong
//!   decode of it - the value would land somewhere else and the program would
//!   read whatever was there. A kernel that wrote to one fixed register, or
//!   wrote nothing, then reads as a failure rather than as a coincidence.
//! * The two cache operations are run over the program's *own* code and over a
//!   stack slot, and the claim is the program's progress past them: a kernel
//!   that answered with a signal where it should have answered with an operation
//!   leaves the check unprinted, which the sweep counts as a failure.
//! * The address nothing maps is run in a **child**, because the answer there
//!   *is* a death - `SIGSEGV`, Linux's answer to a maintenance fault
//!   (`arm64_notify_segfault`), and the parent reads it off the status word. A
//!   kernel that instead took the fault at EL1 and could not resolve it would
//!   stop the machine, which this check would show as a hung sweep rather than
//!   as a passed claim.
//!
//! The skip is pinned by the shape of the test rather than by a check: a kernel
//! that answered without advancing past the trapped instruction would run it
//! again, and again, and the program would never finish.
//!
//! x86_64 has none of these instructions, so the program says so and leaves
//! there; its real pin runs on the machine that has them.

#[cfg(target_arch = "aarch64")]
use mpos_user_tests::Report;
#[cfg(target_arch = "aarch64")]
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

/// What each destination register is seeded with. Zero, because no `CTR_EL0`
/// this architecture can describe reads zero: `CWG` and `DminLine` are both
/// cache-line fields, and a machine whose smallest cache line is zero bytes is a
/// machine with no cache to describe.
#[cfg(target_arch = "aarch64")]
const SEEDED: u64 = 0;

fn main() {
    // The instructions are aarch64's own; the other machine names them nowhere,
    // so this program says why it measured nothing there rather than pretending
    // to a claim it cannot make.
    #[cfg(not(target_arch = "aarch64"))]
    {
        println!("t-el0-sysreg: skipped (this machine has no CTR_EL0, DC CVAU or IC IVAU)");
    }
    #[cfg(target_arch = "aarch64")]
    aarch64_main();
}

/// A function whose own *code* the test flushes. An instruction-cache operation
/// wants a code address, and this is one the program can name without guessing
/// where its image was mapped.
#[cfg(target_arch = "aarch64")]
fn flushed_code() -> u8 {
    0
}

/// `CTR_EL0`, read at EL0 twice and into two registers the program names.
#[cfg(target_arch = "aarch64")]
fn read_cache_type() -> (u64, u64) {
    let (mut first, mut second) = (SEEDED, SEEDED);
    // SAFETY: two reads of a read-only system register, into the two registers
    // the block names; `mrs` has no other effect, and the kernel answers the
    // trap the two instructions take on this machine - which is the subject of
    // this test rather than something the block assumes.
    unsafe {
        core::arch::asm!(
            "mrs x3, ctr_el0",
            "mrs x5, ctr_el0",
            inout("x3") SEEDED => first,
            inout("x5") SEEDED => second,
            options(nomem, nostack),
        );
    }
    (first, second)
}

/// `DC CVAU` - clean the data cache line the address names to the point of
/// unification: the first maintenance operation a code flush issues.
#[cfg(target_arch = "aarch64")]
unsafe fn dc_cvau(addr: usize) {
    // SAFETY: the caller names the address; the instruction reads and writes no
    // memory, so an address nothing maps is a fault and not a corruption.
    unsafe {
        core::arch::asm!(
            "dc cvau, {addr}",
            addr = in(reg) addr,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// `IC IVAU` - invalidate the instruction cache line the address names.
#[cfg(target_arch = "aarch64")]
unsafe fn ic_ivau(addr: usize) {
    // SAFETY: as [`dc_cvau`].
    unsafe {
        core::arch::asm!(
            "ic ivau, {addr}",
            addr = in(reg) addr,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Fork, run `body`, and answer the child's raw wait status.
///
/// A status rather than an exit code because one of the claims below *is* a
/// death by signal: the parent has to read `WIFSIGNALED`/`WTERMSIG`, which a
/// code cannot carry. A body that returns instead of dying leaves through
/// `_exit`, so nothing of the parent's buffered output goes with it.
#[cfg(target_arch = "aarch64")]
fn child_status(body: impl FnOnce()) -> libc::c_int {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return -1;
    }
    if pid == 0 {
        body();
        unsafe { libc::_exit(0) };
    }
    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    if waited != pid {
        return -1;
    }
    status
}

#[cfg(target_arch = "aarch64")]
fn aarch64_main() {
    let mut r = Report::new();

    let (first, second) = read_cache_type();
    r.check(
        &format!("the trapped read into x3 answered the register ({first:#018x})"),
        first != SEEDED,
    );
    r.check(
        &format!("the trapped read into x5 answered the register ({second:#018x})"),
        second != SEEDED,
    );
    r.check(
        "two reads into two different registers agree",
        first == second,
    );
    // The two fields a caller of this register actually reads. `CWG` is the
    // cache writeback granule and `DminLine` the smallest data-cache line, the
    // one a flush loop shifts by - so a zero in either is an answer no flush can
    // use, whatever it says about the read having "happened".
    r.check(
        &format!("CWG is a cache line ({:#x} bytes)", 4 << (first & 0xf)),
        first & 0xf != 0,
    );
    r.check(
        &format!(
            "DminLine is a cache line ({:#x} bytes)",
            4 << ((first >> 16) & 0xf)
        ),
        (first >> 16) & 0xf != 0,
    );

    // The flush itself, over the program's own code and over its own stack: both
    // operations, both addresses, and the claim is that the program is still
    // here to say so.
    let code = flushed_code as fn() -> u8 as usize;
    let mut slot = 0u8;
    let data = core::ptr::addr_of_mut!(slot) as usize;
    let flushed;
    // SAFETY: both addresses are inside this process's own mapped image and
    // stack; the instructions touch no memory.
    unsafe {
        dc_cvau(code);
        ic_ivau(code);
        dc_cvau(data);
        ic_ivau(data);
        flushed = true;
    }
    r.check(
        "DC CVAU and IC IVAU over the program's own code and stack all return",
        flushed,
    );

    // An address nothing maps. The kernel's answer is `SIGSEGV`, which is
    // Linux's (`arm64_notify_segfault` out of the flush's own fault), and it is
    // the one answer a flush cannot be given politely - there is nowhere to
    // return an errno. The fallback to NULL is deliberate: a failed `mmap`
    // leaves an address that is unmapped too, so the claim is the same one.
    let len = 4096usize;
    let unmapped = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            len,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    }
    .ok()
    .map_or(0, |addr| {
        let _ = unsafe { munmap(addr, len) };
        addr as usize
    });
    let status = child_status(|| unsafe { dc_cvau(unmapped) });
    r.check(
        &format!("a flush over an address nothing maps is SIGSEGV (child status {status:#x})"),
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV,
    );

    r.finish()
}
