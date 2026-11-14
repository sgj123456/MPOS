//! t-trap-signal - a synchronous x86_64 CPU exception ends the process with
//! **its vector's own signal**, not one signal for the whole trap door.
//!
//! Measured on Linux 7.2.14 (`build/scratch/trapsig/w.c`, `g.c`, `ac.c`), one
//! instruction in a forked child at a time, read back through `WTERMSIG`:
//!
//! | instruction | vector | signal |
//! |---|---|---|
//! | `int3`          | `#BP` (3)  | `SIGTRAP`  (5) |
//! | `ud2`           | `#UD` (6)  | `SIGILL`   (4) |
//! | `idiv` by zero  | `#DE` (0)  | `SIGFPE`   (8) |
//! | `hlt` at CPL 3  | `#GP` (13) | `SIGSEGV` (11) |
//! | unaligned load, EFLAGS.AC set | `#AC` (17) | `SIGBUS` (7) |
//! | a null store    | `#PF` (14) | `SIGSEGV` (11) |
//!
//! This kernel used to answer `SIGSEGV` for every one of the first five: the
//! six `x86-interrupt` entries share `user_fault_or_die`, whose only verdict
//! was `send_signal(11)`. A parent that waited for `SIGILL` after a `ud2` was
//! told a bad memory reference had happened. Worse, the `#BP` gate was DPL 0,
//! so a ring-3 `int3` never reached the handler at all: the CPU refused the
//! software interrupt and raised `#GP(0x1a)` - an error word whose index
//! *names vector 3 as the one it could not deliver*. Musl's `a_crash()` and
//! chromium's padding are `int3` bytes, so a plain breakpoint was reported as
//! a protection fault.
//!
//! The claims are the child's death, so `fork`/`wait4` stay raw (the crate's
//! clause (b): no wrapper names the wait-status word). Every fault runs in a
//! child that `_exit(70)`s if it somehow returned. The `#PF` trial is the
//! control: the page-fault path keeps its own `SIGSEGV` (its resolvers decide
//! what the address means first), and a "fix" that routed it through the trap
//! map would be caught here.
//!
//! `#AC` is the one exception that needs the machine behind it: `CR0.AM` must
//! be set for EFLAGS.AC to bite, and QEMU's TCG does not emulate alignment
//! checking at all (measured: the same unaligned load simply completes under
//! TCG, and raises `#AC` under KVM). The kernel sets `CR0.AM` as Linux does;
//! when the machine does raise the exception the claim pins `SIGBUS`, and when
//! it does not the program says so without pretending the claim was tested -
//! the host unit test pins that row unconditionally. The other five claims
//! are accelerator-independent and pin the change on their own.
//!
//! This is x86_64's own vector table (`docs/notes/` §6.159). The second
//! machine's `ESR` → signal decoding is a named gap, so this program joins
//! `tests.toml`'s `[aarch64] gaps` rather than claiming an answer there.

#[cfg(target_arch = "x86_64")]
use mpos_user_tests::Report;

#[cfg(target_arch = "x86_64")]
const SIGILL: i32 = 4;
#[cfg(target_arch = "x86_64")]
const SIGTRAP: i32 = 5;
#[cfg(target_arch = "x86_64")]
const SIGBUS: i32 = 7;
#[cfg(target_arch = "x86_64")]
const SIGFPE: i32 = 8;
#[cfg(target_arch = "x86_64")]
const SIGSEGV: i32 = 11;

fn main() {
    #[cfg(not(target_arch = "x86_64"))]
    {
        println!("t-trap-signal: skipped (the x86_64 vector table is not this machine's)");
        return;
    }
    #[cfg(target_arch = "x86_64")]
    x86_main();
}

/// Fork a child, run `body` in it, and report the signal it died of - or `None`
/// when it exited normally. `body` is expected to fault when `must_fault`;
/// a child under QEMU TCG cannot raise `#AC`, so that one is allowed to
/// return and is reported as a note rather than a failed claim.
#[cfg(target_arch = "x86_64")]
fn child_signal(r: &mut Report, label: &str, must_fault: bool, body: impl FnOnce()) -> Option<i32> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check(&format!("{label}: the child was forked"), false);
        return None;
    }
    if pid == 0 {
        body();
        // A body that returned did not fault. Exit with a code no signal
        // produces so the parent can tell "exited" from "signaled".
        unsafe { libc::_exit(70) };
    }
    let mut status = 0i32;
    let waited = unsafe { libc::wait4(pid, &raw mut status, 0, core::ptr::null_mut()) };
    r.check(
        &format!("{label}: the parent waited for the child"),
        waited == pid,
    );
    // wait-status encoding: the low seven bits are the terminating signal;
    // zero there means an ordinary exit, 0x7f a stop (which WUNTRACED, absent
    // here, never reports).
    let sig = status & 0x7f;
    let signaled = sig != 0 && sig != 0x7f;
    if must_fault {
        r.check(
            &format!("{label}: the child died of a signal, not an exit code"),
            signaled,
        );
    }
    signaled.then_some(sig)
}

#[cfg(target_arch = "x86_64")]
fn x86_main() {
    println!("t-trap-signal");
    let mut r = Report::new();

    // #BP: a software breakpoint. With the DPL-0 gate this raised #GP(0x1a)
    // instead, so the "SIGTRAP" claim is also the counterfactual for the gate.
    let bp = child_signal(&mut r, "int3 (#BP)", true, || unsafe {
        core::arch::asm!("int3", options(nostack));
    });
    r.check(
        "int3 ends the child with SIGTRAP, not SIGSEGV",
        bp == Some(SIGTRAP),
    );

    // #UD: an instruction the CPU does not know. The old door answered 11.
    let ud = child_signal(&mut r, "ud2 (#UD)", true, || unsafe {
        core::arch::asm!("ud2", options(nostack));
    });
    r.check(
        "ud2 ends the child with SIGILL, not SIGSEGV",
        ud == Some(SIGILL),
    );

    // #DE: divide by zero. The dividend is explicit so nothing depends on
    // what happened to be in eax/edx; a zero divisor raises #DE either way.
    let de = child_signal(&mut r, "idiv by zero (#DE)", true, || unsafe {
        core::arch::asm!(
            "idiv {div:e}",
            div = in(reg) 0u32,
            inout("eax") 1u32 => _,
            inout("edx") 0u32 => _,
            options(nostack),
        );
    });
    r.check(
        "an integer divide by zero ends the child with SIGFPE, not SIGSEGV",
        de == Some(SIGFPE),
    );

    // #GP: a privileged instruction at CPL 3. This one stays SIGSEGV, and
    // is the control against a map that changed *every* vector's answer.
    let gp = child_signal(&mut r, "hlt (#GP)", true, || unsafe {
        core::arch::asm!("hlt", options(nostack));
    });
    r.check(
        "hlt at CPL 3 ends the child with SIGSEGV",
        gp == Some(SIGSEGV),
    );

    // #PF stays on its own path: a null store must still be SIGSEGV.
    let pf = child_signal(&mut r, "a null store (#PF)", true, || unsafe {
        core::ptr::write_volatile(0x10usize as *mut u8, 1);
    });
    r.check(
        "a null store still ends the child with SIGSEGV (the #PF path is separate)",
        pf == Some(SIGSEGV),
    );

    // #AC: set EFLAGS.AC from ring 3 (`popfq` is allowed at CPL 3), then
    // force a genuinely unaligned 4-byte load. `CR0.AM` is the kernel's
    // enable; TCG does not emulate the check, so a machine that does not
    // raise it is named rather than silently accepted.
    let ac = child_signal(
        &mut r,
        "an unaligned load with AC set (#AC)",
        false,
        || unsafe {
            core::arch::asm!("pushfq", "or qword ptr [rsp], 0x40000", "popfq",);
            // An explicitly unaligned dword load: a Rust `read_unaligned`
            // would compile to byte loads and could never raise #AC.
            static mut BUF: Aligned = Aligned([0; 64]);
            let addr = (&raw const BUF).cast::<u8>() as usize + 1;
            let value: u32;
            core::arch::asm!(
                "mov {v:e}, dword ptr [{p}]",
                p = in(reg) addr,
                v = out(reg) value,
                options(nostack, readonly),
            );
            core::hint::black_box(value);
        },
    );
    match ac {
        Some(sig) => r.check(
            "an alignment-check exception ends the child with SIGBUS",
            sig == SIGBUS,
        ),
        None => println!(
            "note: this machine does not raise #AC (QEMU TCG does not emulate \
             alignment checking); the SIGBUS row is pinned by the host unit test"
        ),
    }

    // The machine survived six exceptions inside its own handlers.
    println!("ok:   the machine survived every trap");
    r.finish()
}

/// A 64-byte-aligned buffer so `addr + 1` is a *known* unaligned dword.
/// The bytes are read through a raw pointer inside inline asm, which the
/// compiler's dead-field analysis cannot see.
#[cfg(target_arch = "x86_64")]
#[repr(align(64))]
#[allow(dead_code)]
struct Aligned([u8; 64]);
