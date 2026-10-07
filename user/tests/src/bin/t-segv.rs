//! t-segv - a fault the *process* made is the process's to bear, never the
//! machine's.
//!
//! Pinned here: an instruction-fetch fault at an address the process does not
//! have - jumping into a `PROT_NONE` page - ends that process with `SIGSEGV`
//! and leaves the machine running. The parent observes the child's death
//! through `wait4` and then keeps going, which is the assertion that matters:
//! the kernel was still there to run it.
//!
//! This is a counterfactual for a real panic. The `#PF` handler runs a
//! diagnostic that prints the faulting instruction's own bytes, and it read
//! them at `rip` - which on an *instruction-fetch* fault is the faulting
//! address itself, so the read faulted again from ring 0 and the machine died
//! with `KERNEL PANIC: #PF ...` while describing a user fault. A traced
//! `niri` hit it by jumping to a freed heap address. With the guard in place
//! the diagnostic cannot fault, and this program sees an ordinary `SIGSEGV`.
//!
//! Two faults, because the fixed one is *not* the common case and must not
//! break the common one: a jump into a `PROT_NONE` page (fetch), and a store
//! through a null pointer (write, where `rip`'s page is mapped and the
//! diagnostic's read was never in doubt).
//!
//! # Why fork/wait4 stay raw
//!
//! The claim is the *child's death*: SIGSEGV-delivered, observed through
//! wait4's status word, with the parent alive afterwards. `std::process`
//! cannot name either half (no fork; its exit-status type decodes only normal
//! exits and signals it chooses to report), so both doors are `libc::fork` /
//! `libc::wait4` - clause (b) in the crate's words, the wrapper is not the
//! call the ABI promises here. The faulting stores themselves are plain
//! volatile writes; the jump goes through a function pointer built from the
//! PROT_NONE page's address.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
/// The signal the child is expected to die of.
const SIGSEGV: i32 = 11;

fn main() {
    println!("t-segv");
    let mut r = Report::new();

    // A page the process holds and may not touch, so the faulting address is
    // deterministic rather than whatever happens to be unmapped under ASLR.
    let guard = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGE,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    r.check("a PROT_NONE page was mapped", guard != libc::MAP_FAILED);

    if guard != libc::MAP_FAILED {
        let entry = guard as usize;
        // The fetch fault: an indirect call into a page with no execute
        // permission. This is the shape that used to panic the kernel.
        child_dies_of_segv(
            &mut r,
            "a jump into a PROT_NONE page is SIGSEGV",
            move || {
                let jump: extern "C" fn() = unsafe { core::mem::transmute(entry) };
                jump();
            },
        );
    }

    // The write fault, which the diagnostic always could describe: `rip` is
    // inside the program's own text, so reading the instruction there never
    // faults. It is here so a guard that refused to read *any* instruction
    // bytes - the wrong fix - would still be caught.
    child_dies_of_segv(&mut r, "a store through a null pointer is SIGSEGV", || {
        unsafe { core::ptr::write_volatile(0x10usize as *mut u8, 1) };
    });

    // The machine is still here, which is the point of the program: both
    // children faulted inside the kernel's fault handler, and neither took
    // the kernel with it.
    println!("ok:   the machine survived both faults");

    if guard != libc::MAP_FAILED {
        let _ = unsafe { libc::munmap(guard, PAGE) };
    }

    r.finish()
}

/// Fork a child, run `body` in it, and report whether it died of `SIGSEGV`.
/// `body` is expected to fault, so it never returns.
fn child_dies_of_segv(r: &mut Report, label: &str, body: impl FnOnce() + Copy) {
    // SAFETY: the child runs only `body`, which is expected to fault; the
    // fork itself touches no allocator state between here and the exec-free
    // path, and the parent waits immediately below.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check(label, false);
        return;
    }
    if pid == 0 {
        body();
        // A body that returned did not fault, so say so with a code no
        // signal produces and let the parent's assertion fail.
        unsafe { libc::_exit(70) };
    }
    let mut status = 0i32;
    let waited = unsafe { libc::wait4(pid, &raw mut status, 0, core::ptr::null_mut()) };
    r.check(
        &format!("{label}: the parent waited for the child"),
        waited == pid,
    );
    // Linux's wait-status encoding: the low seven bits are the signal, and
    // zero there means an ordinary exit.
    r.check(
        &format!("{label}: died of SIGSEGV, not of an exit code"),
        (status & 0x7f) == SIGSEGV,
    );
}
