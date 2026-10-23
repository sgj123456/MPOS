//! t-signal-inherit - a child is born with its parent's dispositions, and an
//! `exec` resets only the caught ones.
//!
//! Measured whole on a current host first (`.workbuddy/probe_sig_inherit.c`,
//! Linux 7.2.11):
//!
//! * **`fork` copies the table.** A `SIGUSR1` handler, a `SIGUSR2` `SIG_IGN`
//!   and the blocked mask all cross to the child; so does the **alternate
//!   signal stack**, whose `ss_sp` and size the child reads back unchanged
//!   (`.workbuddy/probe_altstack_fork.c`).
//! * **`exec` resets caught dispositions and keeps ignored ones.** After the
//!   exec the handler read back `SIG_DFL`, the `SIG_IGN` stayed, and the
//!   blocked mask crossed. A signal that was pending and blocked before the
//!   exec was still pending after it. The old alternate stack is dropped - but
//!   a hosted Rust image installs one of its own at startup (measured under
//!   `strace`), so the reset is not claimed from this side; the kernel's own
//!   reset is `src/sched/runtime/child_exit.rs`'s exec path.
//!
//! The `clone3` half - `CLONE_CLEAR_SIGHAND` asking for the same `SIG_DFL`
//! table in a freshly cloned child, and its `EINVAL` beside `CLONE_SIGHAND` -
//! is pinned in `t-clone3`, where the raw `clone3` harness already lives.
//!
//! The post-exec image is this same program: the child execs its own path
//! (`std::env::current_exe`) with `after-exec` as the first argument and a
//! descriptor to report on, so what is measured is a real `execve` of a real
//! ELF and not a simulated reset. The report is a bit mask over that pipe;
//! nothing on the child's stdout can be mistaken for a claim.

use mpos_user_tests::Report;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};

extern "C" fn handler(_: libc::c_int) {}

/// The alternate stack `setup` installs, so a child can tell "the parent's
/// stack crossed" from "some stack is installed".
static ALT_ADDR: AtomicUsize = AtomicUsize::new(0);

const H_HANDLER: u32 = 1 << 0;
const H_IGNORE: u32 = 1 << 1;
const H_DFL: u32 = 1 << 2;
const H_BLOCK_WINCH: u32 = 1 << 3;
const H_BLOCK_USR1: u32 = 1 << 4;
const H_PENDING_USR1: u32 = 1 << 5;
const H_ALTSTACK: u32 = 1 << 6;

/// The five facts a fork child must show, in one word.
const INHERITED: u32 = H_HANDLER | H_IGNORE | H_BLOCK_WINCH | H_BLOCK_USR1 | H_ALTSTACK;
/// Everything an `exec` image must show: the caught handler reset, the ignored
/// one kept, both blocks kept, the pending signal kept. The alternate stack is
/// deliberately absent: the hosted runtime installs its own after the exec, so
/// from a Rust program the old-and-new stacks are indistinguishable by
/// presence (see the module comment).
const AFTER_EXEC: u32 = H_IGNORE | H_DFL | H_BLOCK_WINCH | H_BLOCK_USR1 | H_PENDING_USR1;

/// Read the signal state this test is about, as a bit word.
fn bits() -> u32 {
    let mut u1: libc::sigaction = unsafe { core::mem::zeroed() };
    let mut u2: libc::sigaction = unsafe { core::mem::zeroed() };
    let mut blocked: libc::sigset_t = unsafe { core::mem::zeroed() };
    let mut pending: libc::sigset_t = unsafe { core::mem::zeroed() };
    let mut alt: libc::stack_t = unsafe { core::mem::zeroed() };
    // SAFETY: every pointer names a local this frame owns; the queries write
    // only those out-parameters.
    unsafe {
        libc::sigaction(libc::SIGUSR1, core::ptr::null(), &mut u1);
        libc::sigaction(libc::SIGUSR2, core::ptr::null(), &mut u2);
        libc::sigprocmask(libc::SIG_BLOCK, core::ptr::null(), &mut blocked);
        libc::sigpending(&mut pending);
        libc::sigaltstack(core::ptr::null(), &mut alt);
    }
    let mut b = 0;
    if u1.sa_sigaction == handler as *const () as libc::sighandler_t {
        b |= H_HANDLER;
    }
    if u1.sa_sigaction == libc::SIG_DFL {
        b |= H_DFL;
    }
    if u2.sa_sigaction == libc::SIG_IGN {
        b |= H_IGNORE;
    }
    // SAFETY: the sets are locals the calls above filled.
    unsafe {
        if libc::sigismember(&blocked, libc::SIGWINCH) == 1 {
            b |= H_BLOCK_WINCH;
        }
        if libc::sigismember(&blocked, libc::SIGUSR1) == 1 {
            b |= H_BLOCK_USR1;
        }
        if libc::sigismember(&pending, libc::SIGUSR1) == 1 {
            b |= H_PENDING_USR1;
        }
    }
    if alt.ss_flags & libc::SS_DISABLE == 0 && alt.ss_sp as usize == ALT_ADDR.load(Ordering::SeqCst)
    {
        b |= H_ALTSTACK;
    }
    b
}

/// Install the fixture: one caught signal, one ignored, two blocked, and an
/// alternate stack the parent owns.
fn setup() -> Vec<u8> {
    // SAFETY: `handler` is `extern "C" fn(c_int)`, the shape `signal` takes.
    unsafe {
        libc::signal(libc::SIGUSR1, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGUSR2, libc::SIG_IGN);
    }
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGWINCH);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigprocmask(libc::SIG_BLOCK, &set, core::ptr::null_mut());
    }
    let mut stack = vec![0u8; 64 * 1024];
    // The mapping has to outlive every fork; the vector stays in `main`'s
    // scope and its address is stable (no reallocation after this point).
    let ss = libc::stack_t {
        ss_sp: stack.as_mut_ptr().cast(),
        ss_flags: 0,
        ss_size: stack.len(),
    };
    ALT_ADDR.store(ss.ss_sp as usize, Ordering::SeqCst);
    unsafe { libc::sigaltstack(&ss, core::ptr::null_mut()) };
    stack
}

/// The image after `exec`: report the bit word on the inherited descriptor and
/// leave without ever touching the parent's report.
fn after_exec(fd: RawFd) -> ! {
    let word = bits().to_ne_bytes();
    unsafe {
        libc::write(fd, word.as_ptr().cast(), word.len());
        libc::_exit(0)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("after-exec") {
        let fd = argv
            .get(2)
            .and_then(|s| s.parse::<RawFd>().ok())
            .unwrap_or(-1);
        after_exec(fd);
    }

    println!("t-signal-inherit");
    let mut r = Report::new();
    // Held for the whole run: `setup` hands the kernel this vector's memory.
    let _alt_stack = setup();
    let parent = bits();
    r.check(
        "the fixture: a handler, an ignore, two blocks and an alt stack",
        parent & (H_HANDLER | H_IGNORE | H_BLOCK_WINCH | H_BLOCK_USR1)
            == (H_HANDLER | H_IGNORE | H_BLOCK_WINCH | H_BLOCK_USR1)
            && parent & H_ALTSTACK != 0,
    );

    // ------------------------------------------------------------------
    // fork: the table and the mask cross; the alternate stack does not.
    // ------------------------------------------------------------------
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let b = bits();
        unsafe { libc::_exit(if b == INHERITED { 0 } else { 1 }) };
    }
    r.check("fork succeeds", pid > 0);
    let mut status: libc::c_int = 0;
    // SAFETY: a child this call made, whose status nothing else reads.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check(
        "a fork child inherits the handler, the ignore, the mask and the alt stack",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // ------------------------------------------------------------------
    // fork + exec: the caught handler goes back to the default, the ignored
    // one stays, the mask and a pending blocked signal cross, the alt stack
    // is dropped.
    // ------------------------------------------------------------------
    let mut fds = [0i32; 2];
    r.check(
        "the report pipe opens",
        unsafe { libc::pipe(fds.as_mut_ptr()) } == 0,
    );
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The pre-exec image's own alt stack and a pending, blocked signal.
        unsafe { libc::close(fds[0]) };
        let mut stack = vec![0u8; 64 * 1024];
        let ss = libc::stack_t {
            ss_sp: stack.as_mut_ptr().cast(),
            ss_flags: 0,
            ss_size: stack.len(),
        };
        unsafe {
            libc::sigaltstack(&ss, core::ptr::null_mut());
            libc::kill(libc::getpid(), libc::SIGUSR1);
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
        let exe = CString::new(exe.into_os_string().into_encoded_bytes()).unwrap_or_default();
        let marker = CString::new("after-exec").unwrap();
        let fdarg = CString::new(fds[1].to_string()).unwrap();
        let args = [
            exe.as_ptr(),
            marker.as_ptr(),
            fdarg.as_ptr(),
            core::ptr::null(),
        ];
        unsafe {
            libc::execv(exe.as_ptr(), args.as_ptr());
            libc::_exit(111);
        }
    }
    unsafe { libc::close(fds[1]) };
    r.check("the exec child starts", pid > 0);

    let mut ready = libc::pollfd {
        fd: fds[0],
        events: libc::POLLIN,
        revents: 0,
    };
    let polled = unsafe { libc::poll(&mut ready, 1, 5000) } == 1;
    let mut word = [0u8; 4];
    let n = if polled {
        unsafe { libc::read(fds[0], word.as_mut_ptr().cast(), word.len()) }
    } else {
        -1
    };
    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe { libc::close(fds[0]) };
    r.check("the post-exec image reports its signal state", n == 4);
    let after = u32::from_ne_bytes(word);
    r.check(
        "after exec a caught signal is back to SIG_DFL",
        after & H_HANDLER == 0 && after & H_DFL != 0,
    );
    r.check("and an ignored signal stays ignored", after & H_IGNORE != 0);
    r.check(
        "and the blocked mask crosses the exec",
        after & (H_BLOCK_WINCH | H_BLOCK_USR1) == (H_BLOCK_WINCH | H_BLOCK_USR1),
    );
    r.check(
        "and a pending blocked signal is still pending",
        after & H_PENDING_USR1 != 0,
    );
    r.check(
        "the exec image leaves clean",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
    // The alternate-stack bit is the hosted runtime's own after an exec (the
    // module comment says why), so the whole-word claim leaves that one bit
    // out rather than pinning a runtime's allocator.
    r.check(
        "the whole after-exec word is the measured one",
        after & !H_ALTSTACK == AFTER_EXEC,
    );

    r.finish()
}
