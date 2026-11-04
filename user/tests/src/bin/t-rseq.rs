//! t-rseq - `rseq(2)`, the restartable-sequence registration.
//!
//! The shape pinned here is Linux 7.2.11's, measured as real root
//! (`build/scratch/rseq/`, every probe a fresh `musl-gcc -static` task: a glibc
//! binary registers a 33-byte area at startup, and every later call in that
//! thread then answers `EINVAL`). What the measurement established:
//!
//! * `rseq(area, 32, 0, sig)` answers 0 for a 32-byte-aligned area and writes
//!   `rseq_cs = 0`, the id words (the running cpu), and the trailing
//!   node/concurrency words.
//! * Re-registering the same area/length is `EBUSY`; a different area or length
//!   is `EINVAL`; a different signature `EPERM`; a misaligned area `EINVAL`;
//!   `len` 0/31 `EINVAL`; a flags word outside `RSEQ_FLAG_SLICE_EXT_DEFAULT_ON`
//!   `EINVAL`.
//! * Unregistering is `rseq(area, len, RSEQ_FLAG_UNREGISTER, sig)` with the
//!   *registered* triple - a null pointer is not the shape - and resets the ids.
//!   A wrong signature is `EPERM`, a wrong area/length `EINVAL`, a flags word of
//!   `1 | 2` `EINVAL`, and unregistering nothing `EINVAL`.
//! * A `fork` child inherits the registration; a thread starts with none.
//! * A tick that preempts a task inside its armed span ends the sequence: Linux
//!   clears `rseq_cs` and resumes at `abort_ip`. This kernel keeps the same
//!   promise a different way - it spends the tick instead of switching, and a
//!   task still inside its span on the next tick is ended with `SIGSEGV`,
//!   because a span is instructions and never a millisecond (a named
//!   divergence, `docs/notes/` §6.35).
//!
//! The one shape this kernel refuses is Linux's newer area: `len > 32` is
//! `EOPNOTSUPP` (this kernel carries no `node_id`/`mm_cid`/`slice_ctrl` facts),
//! and that claim is guest-only - the host branch prints that Linux serves it.

use mpos_user_tests::{Report, host_run};

/// The signature word a restartable sequence puts four bytes before its abort
/// handler.
const RSEQ_SIG: u32 = 0x5305_3053;

/// `RSEQ_CPU_ID_UNINITIALIZED`.
const CPU_ID_UNINITIALIZED: u32 = u32::MAX;

const ON_X86_64: bool = cfg!(target_arch = "x86_64");

/// A registration block, 64-byte aligned so both the legacy 32-byte area and
/// Linux's v2 area have the alignment their shapes ask for.
#[repr(C, align(64))]
struct Area([u8; 64]);

static mut AREA: Area = Area([0; 64]);
static mut SECOND: Area = Area([0; 64]);

fn area_of(area: &Area) -> usize {
    area as *const Area as usize
}

/// Read a `u32` field of a registration block.
///
/// SAFETY: the caller names an offset inside the 64-byte block, and the block
/// outlives the program.
unsafe fn field(area: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((area + off) as *const u32) }
}

fn rseq(area: usize, len: usize, flags: usize, sig: u32) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(libc::SYS_rseq, area, len, flags, sig as usize) };
    if ret == -1 {
        Err(unsafe { *libc::__errno_location() })
    } else {
        Ok(ret as isize)
    }
}

fn register(area: usize) -> Result<isize, i32> {
    rseq(area, 32, 0, RSEQ_SIG)
}

fn unregister(area: usize, len: usize, sig: u32) -> Result<isize, i32> {
    rseq(area, len, 1, sig)
}

// The armed-section probe: it arms `area`'s critical-section word around a
// spin of roughly a few clock ticks, commits, and returns 0 - or, on a kernel
// that rewinds a preempted section, resumes at the abort label and returns 7.
//
// The block is a real one: the signature word sits four bytes before the abort
// label, exactly where Linux reads it. Intel syntax, because that is the
// assembler syntax this crate's assembly is written in.
#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    r#"
    .text
    .globl rseq_arm_and_spin
    .hidden rseq_arm_and_spin
rseq_arm_and_spin:
    lea rax, [rip + rseq_descriptor]
    lea rcx, [rip + rseq_span_start]
    mov [rax + 8], rcx
    lea rcx, [rip + rseq_span_commit]
    lea rdx, [rip + rseq_span_start]
    sub rcx, rdx
    mov [rax + 16], rcx
    lea rcx, [rip + rseq_span_abort]
    mov [rax + 24], rcx
    mov [rdi + 8], rax
rseq_span_start:
    mov ecx, 0x30000000
2:
    dec ecx
    jnz 2b
rseq_span_commit:
    mov qword ptr [rdi + 8], 0
    xor eax, eax
    ret
    .long 0x53053053
rseq_span_abort:
    mov eax, 7
    ret
    .data
    .align 8
rseq_descriptor:
    .zero 32
"#
);

#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(
    r#"
    .text
    .globl rseq_arm_and_spin
    .hidden rseq_arm_and_spin
rseq_arm_and_spin:
    adrp x1, rseq_descriptor
    add x1, x1, :lo12:rseq_descriptor
    adrp x3, rseq_span_start
    add x3, x3, :lo12:rseq_span_start
    str x3, [x1, #8]
    mov x3, #(rseq_span_commit - rseq_span_start)
    str x3, [x1, #16]
    adrp x3, rseq_span_abort
    add x3, x3, :lo12:rseq_span_abort
    str x3, [x1, #24]
    str x1, [x0, #8]
rseq_span_start:
    movz w2, #0x1000
    movk w2, #0x3000, lsl #16
2:
    subs w2, w2, #1
    b.ne 2b
rseq_span_commit:
    str xzr, [x0, #8]
    mov w0, #0
    ret
    .long 0x53053053
rseq_span_abort:
    mov w0, #7
    ret
    .data
    .align 8
rseq_descriptor:
    .word 0
    .word 0
    .quad 0
    .quad 0
    .quad 0
"#
);

unsafe extern "C" {
    /// Arm the section and spin through it; 0 on a commit, 7 on an abort.
    fn rseq_arm_and_spin(area: *mut Area) -> i32;
}

/// Run the armed-section probe in a child, and report how it died.
///
/// The parent registers first, so the child inherits a registration and the
/// probe runs against a real one. On Linux the child either commits (no tick
/// landed inside the span) or aborts and exits 7; on this kernel a tick inside
/// the span is spent, and a second one ends the child with `SIGSEGV`.
fn armed_child() -> (bool, Option<i32>, Option<i32>) {
    let area = area_of(unsafe { &*core::ptr::addr_of!(AREA) });
    let _ = unregister(area, 32, RSEQ_SIG);
    assert_eq!(register(area), Ok(0), "the parent's registration");

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let code = unsafe { rseq_arm_and_spin(core::ptr::addr_of_mut!(AREA)) };
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let signaled = libc::WIFSIGNALED(status);
    let signal = if signaled {
        Some(libc::WTERMSIG(status))
    } else {
        None
    };
    let code = if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    };
    let _ = unregister(area, 32, RSEQ_SIG);
    (signaled, signal, code)
}

/// The same probe in a child whose thread group also has a thread parked in a
/// futex, reported as `(reaped, signal)`.
///
/// The signal alone cannot tell the two doors apart: a bare `exit(-11)` ends the
/// faulting thread with `SIGSEGV` exactly as the group-taking door does. What
/// differs is whether the *group* is reaped — with only the faulting thread gone
/// the parked sibling keeps the group alive, `exit` answers `group_dead == false`,
/// nothing is recorded as retired, and the parent's `wait` never returns. So the
/// fork is followed by a **bounded** poll rather than a blocking wait: a
/// counterfactual run must fail a claim, not hang the lane (`docs/notes/` §6.77f).
fn armed_child_with_parked_thread() -> (bool, Option<i32>) {
    let area = area_of(unsafe { &*core::ptr::addr_of!(AREA) });
    let _ = unregister(area, 32, RSEQ_SIG);
    assert_eq!(register(area), Ok(0), "the parent's registration");

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // A word of the child's own, so the park is this group's and nothing
        // else can wake it.
        let word = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        } as *mut u32;
        if word.is_null() || word as isize == -1 {
            unsafe { libc::_exit(70) };
        }
        unsafe { std::ptr::write(word, 2) };
        let address = word as usize;
        std::thread::spawn(move || unsafe {
            libc::syscall(
                libc::SYS_futex,
                address as *mut u32,
                128u32, // FUTEX_WAIT | FUTEX_PRIVATE_FLAG
                2u32,
                0usize,
                0usize,
                0usize,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let code = unsafe { rseq_arm_and_spin(core::ptr::addr_of_mut!(AREA)) };
        unsafe { libc::_exit(code) };
    }

    let mut status = 0;
    let mut reaped = false;
    for _ in 0..300 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if got == pid {
            reaped = true;
            break;
        }
    }
    if !reaped {
        // Leave nothing behind: a survivor from the unfixed shape would
        // otherwise outlive this program and run under the next one's name.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
    let signal = if reaped && libc::WIFSIGNALED(status) {
        Some(libc::WTERMSIG(status))
    } else {
        None
    };
    let _ = unregister(area, 32, RSEQ_SIG);
    (reaped, signal)
}

fn main() {
    println!("t-rseq");
    let mut r = Report::new();
    let area = area_of(unsafe { &*core::ptr::addr_of!(AREA) });
    let second = area_of(unsafe { &*core::ptr::addr_of!(SECOND) });

    // A registration on a fresh task answers 0 and leaves the section disarmed.
    r.check(
        "a 32-byte registration on a 32-byte-aligned area answers 0",
        register(area) == Ok(0),
    );
    r.check(
        "the block's critical-section word starts disarmed",
        unsafe { field(area, 8) == 0 },
    );
    r.check(
        "the block's ids name the running cpu, not the uninitialised value",
        unsafe { field(area, 0) != CPU_ID_UNINITIALIZED && field(area, 0) == field(area, 4) },
    );

    // Re-registration rules.
    r.check(
        "re-registering the same area and length is EBUSY",
        register(area) == Err(libc::EBUSY),
    );
    r.check(
        "re-registering a different area is EINVAL",
        register(second) == Err(libc::EINVAL),
    );
    r.check(
        "re-registering the same area with a different signature is EPERM",
        rseq(area, 32, 0, RSEQ_SIG ^ 0x0101_0101) == Err(libc::EPERM),
    );

    // Shape rules, all measured.
    r.check(
        "a misaligned area is EINVAL",
        register(area + 8) == Err(libc::EINVAL),
    );
    r.check(
        "a length of 31 is EINVAL",
        rseq(second, 31, 0, RSEQ_SIG) == Err(libc::EINVAL),
    );
    r.check(
        "a length of 0 is EINVAL",
        rseq(second, 0, 0, RSEQ_SIG) == Err(libc::EINVAL),
    );
    r.check(
        "a flags word outside the slice-extension bit is EINVAL",
        rseq(second, 32, 0x8000_0000, RSEQ_SIG) == Err(libc::EINVAL),
    );

    // Unregistration rules.
    r.check(
        "unregistering with a different signature is EPERM",
        unregister(area, 32, RSEQ_SIG ^ 0x0101_0101) == Err(libc::EPERM),
    );
    r.check(
        "unregistering with a different length is EINVAL",
        unregister(area, 33, RSEQ_SIG) == Err(libc::EINVAL),
    );
    r.check(
        "unregistering with the flag word widened is EINVAL",
        rseq(area, 32, 3, RSEQ_SIG) == Err(libc::EINVAL),
    );
    r.check(
        "the registered triple unregisters with 0",
        unregister(area, 32, RSEQ_SIG) == Ok(0),
    );
    r.check(
        "an unregistered block reads back the uninitialised id",
        unsafe { field(area, 0) == 0 && field(area, 4) == CPU_ID_UNINITIALIZED },
    );
    r.check(
        "unregistering when nothing is registered is EINVAL",
        unregister(area, 32, RSEQ_SIG) == Err(libc::EINVAL),
    );

    // `RSEQ_FLAG_SLICE_EXT_DEFAULT_ON` (2) is accepted on the legacy area.
    r.check(
        "the slice-extension flag is accepted on a fresh registration",
        rseq(area, 32, 2, RSEQ_SIG) == Ok(0),
    );
    let _ = unregister(area, 32, RSEQ_SIG);

    // A fork child keeps the parent's registration: it can unregister with the
    // parent's own triple.
    assert_eq!(register(area), Ok(0), "the fork's registration");
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let code = if unregister(area, 32, RSEQ_SIG) == Ok(0) {
            0
        } else {
            1
        };
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check(
        "a forked child inherits the registration and can unregister it",
        unsafe { libc::WIFEXITED(status) } && unsafe { libc::WEXITSTATUS(status) } == 0,
    );
    let _ = unregister(area, 32, RSEQ_SIG);

    // A thread does not inherit it: it can register its own area.
    let thread_area = second;
    let joined = std::thread::spawn(move || register(thread_area) == Ok(0)).join();
    r.check(
        "a thread starts with no registration and registers its own area",
        matches!(joined, Ok(true)),
    );

    // The armed-section promise.
    let (signaled, signal, code) = armed_child();
    if host_run() {
        println!(
            "note: the host rewinds a preempted section itself (signaled={signaled}, signal={signal:?}, exit={code:?})"
        );
    } else {
        r.check(
            "a tick inside an armed section ends it: the task dies of SIGSEGV, never resuming mid-span",
            signaled && signal == Some(libc::SIGSEGV),
        );
    }

    // And the same death with a sibling parked: a fault is a *process*'s death
    // on Linux, so the group must go with it. The signal cannot tell the two
    // doors apart - `exit(-11)` sends the same `SIGSEGV` - so the claim is about
    // the group being reaped at all (`docs/notes/` §6.77f).
    if !host_run() {
        let (reaped, faulted) = armed_child_with_parked_thread();
        r.check(
            "a tick inside an armed section reaps the whole process, parked sibling and all",
            reaped && faulted == Some(libc::SIGSEGV),
        );
    }

    // The `auxv` pair a libc reads before it registers: the feature size and
    // alignment. Both kernels answer the same numbers, which is the reference
    // direction - the host's `getauxval` is where these values came from.
    let feature = unsafe { libc::getauxval(27) };
    let alignment = unsafe { libc::getauxval(28) };
    r.check(
        &format!("the auxv names the feature size (is {feature})"),
        feature == 33,
    );
    r.check(
        &format!("and the feature alignment (is {alignment})"),
        alignment == 64,
    );

    // The v2 area: `len > 32` names the feature-sized shape, whose extra words
    // are the feature flags, the NUMA node id, the concurrency id and the
    // time-slice control. Both kernels accept it; what each writes into
    // `flags` is the feature bits it *offers* - Linux's slice extension is
    // available (0x10, and 0x30 when the default-on flag is asked), and this
    // kernel offers none (0), because answering otherwise would promise a
    // rewind/slice behaviour it does not have.
    let _ = register(second);
    let _ = unregister(second, 32, RSEQ_SIG);
    let v2 = rseq(second, 33, 0, RSEQ_SIG);
    r.check(
        &format!("the v2 area (len 33) is accepted (is {v2:?})"),
        v2 == Ok(0),
    );
    let flags = unsafe { field(second, 16) };
    let offered: u32 = if host_run() { 0x10 } else { 0 };
    r.check(
        &format!("its flags word names the feature bits the kernel offers (is {flags:#x})"),
        flags == offered,
    );
    let node = unsafe { field(second, 20) };
    r.check(
        &format!("the node id is 0 on a machine with one node (is {node})"),
        node == 0,
    );
    // One CPU: only one thread of this process ever runs at a time, so every
    // concurrency id is a valid index - this kernel answers 0.
    let cid = unsafe { field(second, 24) };
    r.check(
        &format!("the concurrency id is 0 on this machine (is {cid})"),
        cid == 0,
    );
    let slice = unsafe { field(second, 28) };
    r.check(
        &format!("the slice control word is 0, no extension is promised (is {slice:#x})"),
        slice == 0,
    );
    r.check(
        "a length between the two shapes (len 40 is a v2 length, len 20 is not) is EINVAL",
        rseq(second, 20, 0, RSEQ_SIG) == Err(libc::EINVAL),
    );
    r.check(
        "a v2 length on an area that is not 64-byte aligned is EINVAL",
        rseq(second + 8, 33, 0, RSEQ_SIG) == Err(libc::EINVAL),
    );
    r.check(
        "the v2 area unregisters with its own length",
        unregister(second, 33, RSEQ_SIG) == Ok(0),
    );
    r.finish()
}
