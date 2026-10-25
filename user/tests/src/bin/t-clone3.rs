//! t-clone3 - the extensible `clone` door, and the one that names its own child.
//!
//! `clone(2)` has a 32-bit flag word with an exit signal in its low byte, five
//! positional arguments and no room for anything new. `clone3(2)` replaces all
//! of that with one versioned struct, and this program pins the part of it this
//! kernel serves: the argument struct's size discipline, the flags, the
//! `stack`/`stack_size` pair, the `exit_signal` field, and - the item this door
//! exists for here - `CLONE_PIDFD`, which hands the caller the new process **and
//! a descriptor naming it** in one call, so there is no window between the pid
//! coming back and a later `pidfd_open` in which that number could be reused.
//!
//! Every rule below was measured on a current host first
//! (`.workbuddy/probe_clone3{,b,c,d,e,f,g}.c`, Linux 7.2.11), one call per case,
//! because `clone3` is not `clone` with a struct:
//!
//! * **the size decides everything else.** A `size` below
//!   `CLONE_ARGS_SIZE_VER0` (64) is `EINVAL` *before* the pointer is looked at
//!   (measured: a bogus pointer with `size = 8` is `EINVAL`, with `size = 64` it
//!   is `EFAULT`), the words between the caller's size and `VER2` read as zero,
//!   and a `size` past `VER2` (88) is allowed only while every byte past it is
//!   zero - otherwise `E2BIG`.
//! * **`stack` is the bottom, not the top.** `clone(2)`'s `child_stack` is the
//!   stack pointer itself; `clone3`'s `stack` is the base of a region and the
//!   child starts at `stack + stack_size`. Both fields are set together or
//!   neither is: `base + 0` and `0 + size` are both `EINVAL`. Neither set means
//!   the child inherits the caller's stack pointer, which is the fork shape.
//!   (The arithmetic is the leaf's, pinned in `mpos`'s own host test; what this
//!   program pins is that the kernel *uses* it - a child asked for a stack of
//!   its own reports from the top of that mapping.)
//! * **the exit signal is a field**, 0 through `SIGRTMAX` (64); 65 is `EINVAL`.
//! * **the tid flags are `clone3`'s own**: `CLONE_CHILD_SETTID` is served and
//!   needs no pointer (alone, beside `CLONE_VM`, with a real `child_tid` or with
//!   none it starts a child), `CLONE_PARENT_SETTID` needs none either - a bad
//!   one is *ignored* rather than refused (`put_user`'s result is dropped) - and
//!   the one bit this door rejects is **`CLONE_DETACHED`**, which `clone(2)`
//!   still accepts as a historical no-op.
//! * **the child's signal table is the parent's, and `CLONE_CLEAR_SIGHAND`
//!   asks for `SIG_DFL` instead**: a `SIGUSR1` handler installed before the
//!   call is visible in a plain child and gone in the flagged one (measured on
//!   a current host, `.workbuddy/probe_sig_inherit.c`). The bit is `EINVAL`
//!   beside `CLONE_SIGHAND` - clearing and sharing one table are opposite
//!   requests - which also refuses it beside `CLONE_THREAD`, whose dependency
//!   chain requires `CLONE_SIGHAND`.
//! * **`CLONE_PIDFD` requires its field and excludes the tid write**: `pidfd =
//!   0` is `EFAULT`, and the bit beside `CLONE_PARENT_SETTID` is `EINVAL`.
//! * **`CLONE_INTO_CGROUP` is resolved by the door**, and a descriptor that is
//!   not a cgroup is `EBADF` - which is what this program's fd 0 is, so the
//!   claim below is Linux's answer too. The tree side of the flag (a child
//!   born in the group a descriptor names, its own fork inheriting it, `ENOENT`
//!   for a removed group, `EACCES` for an unprivileged caller, and
//!   `CLONE_NEWCGROUP` rooted at the child's group) needs a mount and lives in
//!   `t-cgroupfs`, where the tree and its groups are.
//!
//! # What is named rather than claimed
//!
//! Two answers are this kernel's and *not* a current Linux's, so each accepts
//! Linux's own answer only in a host run (`mpos_user_tests::host_run`) and pins
//! this kernel's in the gate:
//!
//! * **`CLONE_NEWTIME` (`0x80`) is `EOPNOTSUPP`.** `clone3` is the first door
//!   that can *spell* the bit (through `clone(2)` it sits inside the exit-signal
//!   byte), and this kernel has no clock namespace to hand a child through this
//!   call - Linux creates a task for it (measured), and on a host run as an
//!   ordinary user that host answers `EPERM` first, which the claim accepts.
//!   `unshare(CLONE_NEWTIME)` is the door that serves the clocks.
//! * **`CLONE_PTRACE` (`0x2000`) is `EINVAL` here.** Linux's `clone3` accepts
//!   it (a child with no tracer attached), and this kernel's vocabulary has no
//!   such bit - the same designed divergence `t-clone` names for `clone(2)`,
//!   which also refuses it. Every *other* unknown bit is `EINVAL` on both, so
//!   that half is one claim.
//! * **`set_tid`/`set_tid_size`**: a lone half, or a size above one, is
//!   `EINVAL`; one well-formed entry is `EINVAL` without `CLONE_NEWPID` (Linux's
//!   own answer, because the entry names a number in a namespace the child is
//!   not entering) and `EOPNOTSUPP` with it, where this kernel has no ordered
//!   `set_tid` support. On a host the `CLONE_NEWPID` shape needs a capability
//!   this sandbox does not have, so its `EPERM` is accepted there too.
//!
//! # Why the trampoline
//!
//! A child made with a stack of its own resumes at the instruction after the
//! call with `rsp` already on that stack and no frame of its own to return on,
//! so it cannot just carry on in a C-shaped function. `mpos_clone3_entry` is the
//! same few instructions a libc's `clone` wrapper uses: on the child's path it
//! sets a fresh frame and calls an ordinary function that never returns.

use mpos_user_tests::Report;
use rustix::process::{Pid, WaitOptions, waitpid};
use std::sync::atomic::{AtomicI32, Ordering};

/// `CLONE_CLEAR_SIGHAND` and `CLONE_INTO_CGROUP` as the uapi writes them.
///
/// They are spelled here rather than taken from `libc` because across this
/// crate's two machines `libc` binds neither for musl, and its glibc copy
/// declares both `c_int` values that overflow - deprecated for exactly that
/// reason. `CLONE_PIDFD`, `CLONE_VM`, `CLONE_SIGHAND`, `CLONE_THREAD`,
/// `CLONE_PARENT_SETTID`, `CLONE_CHILD_SETTID` and `CLONE_NEWPID` are the
/// shared `linux` platform's, and come from `libc` as the rest of the suite's
/// flag words do.
const CLONE_CLEAR_SIGHAND: usize = 0x0001_0000_0000;
const CLONE_INTO_CGROUP: usize = 0x0002_0000_0000;

/// `struct clone_args`, the uapi's eleven aligned words. `size` is what a caller
/// passes beside it, not a field.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// The three sizes the struct has had: eight words, then `set_tid`, then
/// `cgroup`.
const VER0: usize = 64;
const VER2: usize = 88;

/// The exit signal a process-shaped call here names, so the child is waitable -
/// `clone3` with `exit_signal = 0` makes a child Linux auto-reaps instead.
const SIGCHLD: u64 = 17;

/// The descriptor the synthetic-stack child reports through, or `-1` when that
/// claim is not being made.
static CHILD_REPORT_FD: AtomicI32 = AtomicI32::new(-1);

/// `clone3(args, size)` through the raw door: `rustix` binds no `clone3`, and
/// half the claims are about the number the call itself answers.
unsafe fn clone3_raw(args: *const CloneArgs, size: usize) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_clone3,
            args as *const core::ffi::c_void,
            size as libc::c_long,
        ) as isize
    }
}

/// Leave through `exit_group`: a child's inherited copy of `std`'s buffered
/// stdout must not be flushed, and the code is the claim.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// The door answers `-1` and leaves the refusal in `errno`.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// One probe: a call whose answer is expected to be a refusal. `clone3` hands
/// back a child for a word it *does* serve, so a wrongly-served probe would run
/// the rest of this program a second time; the child leaves at once.
unsafe fn probe(args: &CloneArgs, size: usize) -> isize {
    let ret = unsafe { clone3_raw(args, size) };
    if ret == 0 {
        leave(0);
    }
    ret
}

/// A call that is expected to start a child, returning its pid. `size` selects
/// how much of the struct the caller claims to have written; the child comes
/// back here on its own copy of this stack (no stack is named, so no child
/// returns on one of its own).
unsafe fn spawn(args: &CloneArgs, size: usize) -> isize {
    let ret = unsafe { clone3_raw(args, size) };
    if ret == 0 {
        leave(0);
    }
    ret
}

/// Reap one child, answering its exit status. Nothing is reaped for a call that
/// did not start one.
fn reap(pid: isize) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let want = Pid::from_raw(pid as i32);
    match waitpid(want, WaitOptions::empty()) {
        Ok(Some((_, status))) => status.exit_status(),
        _ => None,
    }
}

/// The disposition the `CLONE_CLEAR_SIGHAND` pair installs: any address above
/// `SIG_IGN` (1) is a handler, which is all the child's comparison needs.
extern "C" fn disposition_handler(_: libc::c_int) {}

/// What the synthetic-stack child runs, on the stack the kernel handed it.
extern "C" fn child_entry() -> ! {
    let fd = CHILD_REPORT_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = b'C';
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
    leave(0)
}

// The libc-style trampoline. On the child's path `rsp` is already the new
// stack, so the first thing done there is a `call` that builds a frame for the
// C-shaped body; the parent's path returns through the usual link register.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    r#"
    .globl mpos_clone3_entry
mpos_clone3_entry:
    mov eax, 435
    syscall
    test rax, rax
    jne 1f
    xor ebp, ebp
    call mpos_clone3_child
1:  ret
"#
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    r#"
    .globl mpos_clone3_entry
mpos_clone3_entry:
    mov x8, #435
    svc #0
    cbnz x0, 1f
    mov x29, xzr
    bl mpos_clone3_child
1:  ret
"#
);

unsafe extern "C" {
    fn mpos_clone3_entry(args: *const CloneArgs, size: usize) -> isize;
}

#[unsafe(no_mangle)]
extern "C" fn mpos_clone3_child() -> ! {
    child_entry()
}

/// An anonymous mapping whose *top* is usable as a child's stack: the kernel is
/// handed the base and adds the size, and the child's first push lands inside.
unsafe fn stack_region(size: usize) -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        core::ptr::null_mut()
    } else {
        p.cast()
    }
}

/// One flag-word probe's word: the bits plus a waitable exit signal.
fn word(flags: usize) -> CloneArgs {
    CloneArgs {
        flags: flags as u64,
        exit_signal: SIGCHLD,
        ..Default::default()
    }
}

fn main() {
    println!("t-clone3");
    let mut r = Report::new();

    // ---- 1. the door, and the child it makes -----------------------------
    {
        let pid = unsafe { spawn(&word(0), VER2) };
        r.check(
            "a fork-shaped clone3 starts a child that leaves by its own branch",
            pid > 0 && reap(pid) == Some(0),
        );
    }

    // ---- 2. CLONE_PIDFD: the process and its name in one call -------------
    {
        let mut pidfd = 0u64;
        let args = CloneArgs {
            flags: libc::CLONE_PIDFD as u64,
            pidfd: &mut pidfd as *mut u64 as u64,
            exit_signal: SIGCHLD,
            ..Default::default()
        };
        let pid = unsafe { spawn(&args, VER2) };
        let fd = pidfd as i32;
        r.check(
            "CLONE_PIDFD starts the child and its field holds a descriptor for it",
            pid > 0 && pidfd != 0 && pidfd < i32::MAX as u64,
        );
        r.check(
            "the delivered descriptor is close-on-exec",
            pid > 0 && unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC != 0,
        );
        r.check(
            "and carries the access mode Linux reports for a pidfd",
            pid > 0 && unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_ACCMODE == libc::O_RDWR,
        );
        // It names the child already - no `pidfd_open` was called for it.
        r.check(
            "the delivered descriptor names the child while it is alive",
            pid > 0
                && unsafe {
                    libc::syscall(libc::SYS_pidfd_send_signal, fd as libc::c_long, 0, 0, 0)
                } == 0,
        );
        r.check(
            "and the child it names is the one the call made",
            pid > 0 && reap(pid) == Some(0),
        );
        r.check(
            "after the reap the delivered descriptor answers ESRCH",
            pid > 0
                && refused(
                    unsafe {
                        libc::syscall(libc::SYS_pidfd_send_signal, fd as libc::c_long, 0, 0, 0)
                    } as isize,
                    libc::ESRCH,
                ),
        );
        if pid > 0 {
            unsafe { libc::close(fd) };
        }
    }
    {
        // The descriptor came from the call, not from a later `pidfd_open`: a
        // second door onto the same child is a *different* number, so the
        // struct's field is the only name the call made.
        let mut pidfd = 0u64;
        let args = CloneArgs {
            flags: libc::CLONE_PIDFD as u64,
            pidfd: &mut pidfd as *mut u64 as u64,
            exit_signal: SIGCHLD,
            ..Default::default()
        };
        let pid = unsafe { spawn(&args, VER2) };
        let via_open =
            unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_long, 0) as i32 };
        r.check(
            "the delivered descriptor is a name of its own, not pidfd_open's",
            pid > 0 && via_open > 0 && via_open != pidfd as i32,
        );
        if pid > 0 {
            unsafe {
                libc::close(pidfd as i32);
                libc::close(via_open);
            }
        }
        reap(pid);
    }
    {
        // CLONE_PARENT_SETTID parks the child's number where the caller named.
        let mut ptid: i32 = -1;
        let args = CloneArgs {
            flags: libc::CLONE_PARENT_SETTID as u64,
            parent_tid: &mut ptid as *mut i32 as u64,
            exit_signal: SIGCHLD,
            ..Default::default()
        };
        let pid = unsafe { spawn(&args, VER2) };
        r.check(
            "CLONE_PARENT_SETTID parks the child's own number",
            pid > 0 && ptid as isize == pid,
        );
        reap(pid);
    }
    {
        // A destination the caller cannot write is skipped, not refused:
        // Linux drops `put_user`'s result, measured.
        let mut args = word(libc::CLONE_PARENT_SETTID as usize);
        args.parent_tid = 1;
        let pid = unsafe { spawn(&args, VER2) };
        r.check(
            "a parent_tid the caller cannot write is skipped, not refused",
            pid > 0,
        );
        reap(pid);
    }

    // ---- 3. the size discipline ------------------------------------------
    {
        let base = word(0);
        r.check(
            "a size below the first version is EINVAL",
            refused(unsafe { probe(&base, 0) }, libc::EINVAL)
                && refused(unsafe { probe(&base, VER0 - 1) }, libc::EINVAL),
        );
        // The size is asked before the pointer: a bogus pointer is EINVAL while
        // the size is too small, and EFAULT once it is not.
        r.check(
            "and the size is read before the pointer",
            refused(
                unsafe { clone3_raw(core::ptr::null(), VER0 - 1) },
                libc::EINVAL,
            ) && refused(unsafe { clone3_raw(core::ptr::null(), VER0) }, libc::EFAULT),
        );
        let pid = unsafe { spawn(&base, VER0) };
        r.check("VER0 exactly is served", pid > 0);
        reap(pid);
        // A size past VER2 is served only while its tail is zero.
        #[repr(C)]
        struct Big {
            args: CloneArgs,
            tail: [u8; 40],
        }
        let mut big = Big {
            args: base,
            tail: [0; 40],
        };
        let pid = unsafe { spawn(&big.args, core::mem::size_of::<Big>()) };
        r.check(
            "a zero tail past the struct's last version is served",
            pid > 0,
        );
        reap(pid);
        big.tail[12] = 1;
        r.check(
            "a nonzero byte past it is E2BIG",
            refused(
                unsafe { probe(&big.args, core::mem::size_of::<Big>()) },
                libc::E2BIG,
            ),
        );
    }

    // ---- 4. the flag word ------------------------------------------------
    {
        // `CLONE_CHILD_SETTID` is a clone3 flag, and a real pointer beside it
        // is a child - the tid it parks is in the child's own copy (the
        // observable shape is in the synthetic-stack section below).
        let mut ctid = -1i32;
        let mut with_ptr = word(libc::CLONE_CHILD_SETTID as usize);
        with_ptr.child_tid = &mut ctid as *mut i32 as u64;
        let child = unsafe { spawn(&with_ptr, VER2) };
        r.check("CLONE_CHILD_SETTID is a clone3 flag", child > 0);
        reap(child);
        r.check(
            "CLONE_DETACHED, which clone(2) keeps as a no-op, is EINVAL here",
            refused(
                unsafe { probe(&word(libc::CLONE_DETACHED as usize), VER2) },
                libc::EINVAL,
            ),
        );
        r.check(
            "CLONE_SIGHAND without CLONE_VM is EINVAL",
            refused(
                unsafe { probe(&word(libc::CLONE_SIGHAND as usize), VER2) },
                libc::EINVAL,
            ),
        );
        r.check(
            "CLONE_THREAD without VM and SIGHAND is EINVAL",
            refused(
                unsafe {
                    probe(
                        &word(libc::CLONE_THREAD as usize | libc::CLONE_VM as usize),
                        VER2,
                    )
                },
                libc::EINVAL,
            ),
        );
        r.check(
            "an unknown flag bit is EINVAL",
            refused(unsafe { probe(&word(1 << 44), VER2) }, libc::EINVAL),
        );
        // `CLONE_NEWTIME` (`0x80`) is a namespace flag `clone3` can spell,
        // and this kernel has no clock namespace to hand a child through this
        // door: `EOPNOTSUPP` for the gate's root caller. Linux creates a task
        // (measured), and on a host run this program is an ordinary user, so
        // Linux's own capability answer `EPERM` is what comes back first.
        let newtime = unsafe { probe(&word(libc::CLONE_NEWTIME as usize), VER2) };
        let newtime_ok = if mpos_user_tests::host_run() {
            newtime > 0 || refused(newtime, libc::EPERM)
        } else {
            refused(newtime, libc::EOPNOTSUPP)
        };
        if mpos_user_tests::host_run() {
            reap(newtime);
        }
        r.check("CLONE_NEWTIME is a named refusal here", newtime_ok);
        let ptrace_bit = unsafe { probe(&word(0x2000), VER2) };
        let ptrace_ok = if mpos_user_tests::host_run() {
            ptrace_bit > 0
        } else {
            refused(ptrace_bit, libc::EINVAL)
        };
        if mpos_user_tests::host_run() {
            reap(ptrace_bit);
        }
        r.check(
            "CLONE_PTRACE is EINVAL here (Linux starts a child)",
            ptrace_ok,
        );
        r.check(
            "CLONE_INTO_CGROUP with fd 0 is EBADF, not a cgroup directory",
            refused(
                unsafe { probe(&word(CLONE_INTO_CGROUP), VER2) },
                libc::EBADF,
            ),
        );
        // Both sides of the flag, each child answering with its own view of
        // `SIGUSR1`. A fork-shaped call (no stack) returns in this function,
        // so the child can ask its table before it leaves.
        unsafe {
            libc::signal(
                libc::SIGUSR1,
                disposition_handler as *const () as libc::sighandler_t,
            )
        };
        let child_sees = |flags: usize| -> isize {
            let args = word(flags);
            let ret = unsafe { clone3_raw(&args, VER2) };
            if ret == 0 {
                let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
                unsafe { libc::sigaction(libc::SIGUSR1, core::ptr::null(), &mut old) };
                // 1: the table is `SIG_DFL`; 2: the caller's handler survived.
                leave(if old.sa_sigaction == libc::SIG_DFL {
                    1
                } else {
                    2
                });
            }
            ret
        };
        let plain = child_sees(0);
        r.check(
            "a plain clone3 child inherits the parent's handler",
            plain > 0 && reap(plain) == Some(2),
        );
        let cleared = child_sees(CLONE_CLEAR_SIGHAND);
        r.check(
            "CLONE_CLEAR_SIGHAND starts a child with SIG_DFL instead",
            cleared > 0 && reap(cleared) == Some(1),
        );
        r.check(
            "CLONE_CLEAR_SIGHAND beside CLONE_SIGHAND is EINVAL",
            refused(
                unsafe {
                    probe(
                        &word(
                            CLONE_CLEAR_SIGHAND
                                | libc::CLONE_SIGHAND as usize
                                | libc::CLONE_VM as usize,
                        ),
                        VER2,
                    )
                },
                libc::EINVAL,
            ),
        );
        r.check(
            "and beside CLONE_THREAD's chain is EINVAL too",
            refused(
                unsafe {
                    probe(
                        &word(
                            CLONE_CLEAR_SIGHAND
                                | libc::CLONE_SIGHAND as usize
                                | libc::CLONE_VM as usize
                                | libc::CLONE_THREAD as usize,
                        ),
                        VER2,
                    )
                },
                libc::EINVAL,
            ),
        );
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
    }

    {
        // The `pidfd` field is a destination only when the flag names it: set
        // without `CLONE_PIDFD` it is Linux's to ignore, and this kernel once
        // wrote one anyway - which is why the field has its own claim.
        let mut untouched: u64 = 0xABCD;
        let args = CloneArgs {
            pidfd: &mut untouched as *mut u64 as u64,
            exit_signal: SIGCHLD,
            ..Default::default()
        };
        let pid = unsafe { spawn(&args, VER2) };
        r.check(
            "the pidfd field is left alone without CLONE_PIDFD",
            pid > 0 && untouched == 0xABCD,
        );
        reap(pid);
    }

    // ---- 5. CLONE_PIDFD's own refusals ------------------------------------
    {
        let mut pidfd = 0u64;
        let mut none = word(libc::CLONE_PIDFD as usize);
        none.pidfd = 0;
        let mut same = word(libc::CLONE_PIDFD as usize | libc::CLONE_PARENT_SETTID as usize);
        same.pidfd = &mut pidfd as *mut u64 as u64;
        same.parent_tid = same.pidfd;
        r.check(
            "CLONE_PIDFD with no place to write the descriptor is EFAULT",
            refused(unsafe { probe(&none, VER2) }, libc::EFAULT),
        );
        r.check(
            "and two tid writes aimed at one address are EINVAL",
            refused(unsafe { probe(&same, VER2) }, libc::EINVAL),
        );
    }

    // ---- 6. the exit signal and the stack pair ---------------------------
    {
        // A child born to report signal 64 delivers it when it dies, and
        // signal 64 is fatal at its default action - so it is blocked for the
        // length of this claim rather than allowed to kill the program. (The
        // first version of this file died here, which is how the rule that the
        // *exit signal* is a delivery and not just a label was re-learned.)
        let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, 64);
            libc::sigprocmask(libc::SIG_BLOCK, &set, core::ptr::null_mut());
        }
        let mut sig = word(0);
        sig.exit_signal = 64;
        let pid = unsafe { spawn(&sig, VER2) };
        r.check("exit_signal 64 starts a child", pid > 0);
        reap(pid);
        sig.exit_signal = 65;
        r.check(
            "exit_signal 65 is EINVAL",
            refused(unsafe { probe(&sig, VER2) }, libc::EINVAL),
        );
    }
    {
        let size = 1 << 20;
        let base = unsafe { stack_region(size) };
        let mut pair = word(0);
        pair.stack = base as u64;
        pair.stack_size = size as u64;
        let mut stack_only = word(0);
        stack_only.stack = base as u64;
        r.check(
            "stack without stack_size is EINVAL",
            refused(unsafe { probe(&stack_only, VER2) }, libc::EINVAL),
        );
        let mut size_only = word(0);
        size_only.stack_size = size as u64;
        r.check(
            "stack_size without stack is EINVAL",
            refused(unsafe { probe(&size_only, VER2) }, libc::EINVAL),
        );
        // The served shape: the child starts at `stack + stack_size`, the top of
        // the mapping, and reports from there.
        if !base.is_null() {
            let mut fds = [0i32; 2];
            let piped = unsafe { libc::pipe(&mut fds as *mut [i32; 2] as *mut i32) } == 0;
            CHILD_REPORT_FD.store(if piped { fds[1] } else { -1 }, Ordering::SeqCst);
            let pid = unsafe { mpos_clone3_entry(&pair, VER2) };
            let mut byte = 0u8;
            let read_back = piped
                && pid > 0
                && unsafe { libc::read(fds[0], (&mut byte as *mut u8).cast(), 1) } == 1
                && byte == b'C';
            r.check(
                "a child with a stack of its own reports from the top of it",
                read_back,
            );
            reap(pid);
            if piped {
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
            }
            CHILD_REPORT_FD.store(-1, Ordering::SeqCst);
            unsafe { libc::munmap(base.cast(), size) };
        } else {
            r.check(
                "a child with a stack of its own reports from the top of it",
                false,
            );
        }
    }

    // ---- 7. CLONE_CHILD_SETTID's write, read by the child ----------------
    //
    // The number is parked in the *child's* copy of `child_tid`, so a fork-shaped
    // call cannot show it to the caller directly. The child reads it and leaves
    // with it as its exit code, and the caller compares that against the pid the
    // call answered - which is the same number for a process-shaped child.
    {
        let mut ctid: i32 = -1;
        let args = CloneArgs {
            flags: libc::CLONE_CHILD_SETTID as u64,
            child_tid: &mut ctid as *mut i32 as u64,
            exit_signal: SIGCHLD,
            ..Default::default()
        };
        let ret = unsafe { clone3_raw(&args, VER2) };
        if ret == 0 {
            // The child's own copy, written before it was resumed. `volatile`
            // because the kernel wrote through a pointer the compiler sees only
            // as an integer, and its own comparison - the exit code carries
            // eight bits, so a caller cannot compare the numbers itself.
            let parked = unsafe { core::ptr::read_volatile(&ctid) };
            let me = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
            leave(i32::from(parked != me));
        }
        r.check(
            "CLONE_CHILD_SETTID parks the child's own number where it can read it",
            ret > 0 && reap(ret) == Some(0),
        );
    }

    // ---- 8. set_tid -------------------------------------------------------
    {
        let mut tid: u32 = 0;
        let ptr = &mut tid as *mut u32 as u64;
        let with_tid = |p: u64, n: u64, flags: usize| {
            let mut a = word(flags);
            a.set_tid = p;
            a.set_tid_size = n;
            a
        };
        r.check(
            "a lone set_tid or set_tid_size is EINVAL",
            refused(unsafe { probe(&with_tid(ptr, 0, 0), VER2) }, libc::EINVAL)
                && refused(unsafe { probe(&with_tid(0, 1, 0), VER2) }, libc::EINVAL),
        );
        r.check(
            "a size above one is EINVAL",
            refused(unsafe { probe(&with_tid(ptr, 100, 0), VER2) }, libc::EINVAL),
        );
        r.check(
            "one well-formed entry without CLONE_NEWPID is EINVAL",
            refused(unsafe { probe(&with_tid(ptr, 1, 0), VER2) }, libc::EINVAL),
        );
        let ordered = unsafe { probe(&with_tid(ptr, 1, libc::CLONE_NEWPID as usize), VER2) };
        let ordered_ok = if mpos_user_tests::host_run() {
            ordered > 0 || refused(ordered, libc::EPERM)
        } else {
            refused(ordered, libc::EOPNOTSUPP)
        };
        if mpos_user_tests::host_run() {
            reap(ordered);
        }
        r.check(
            "and with CLONE_NEWPID it is a named refusal here",
            ordered_ok,
        );
    }

    r.finish()
}
