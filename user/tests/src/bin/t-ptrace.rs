//! t-ptrace - what a tracer asks the kernel for before it can trace anything.
//!
//! `strace`'s first act is not to trace: it is to prove the machine can be
//! traced at all. It forks, the child calls `PTRACE_TRACEME`, and the parent
//! reads a `PTRACE_GET_SYSCALL_INFO` at the resulting stop - and if any of that
//! answers `ENOSYS` it prints `PTRACE_TRACEME: Function not implemented` and
//! gives up. That is exactly what the second machine used to do: `ptrace` was
//! gated `not(aarch64)` because the request numbers and `user_regs_struct` *are*
//! x86_64's, and the gate was read as covering the whole call rather than the one
//! column of it that genuinely differs. It does not: the request words are one
//! protocol, and the machine's part is which *slot* a role lives in - which
//! `abi::uregs` answers per column, and which is why this file pins both.
//!
//! **Why `libc` and not `rustix`.** `rustix` binds no part of `ptrace`. So the
//! call is made through `libc::syscall` with `libc::SYS_ptrace` - a maintained
//! per-arch table, and the reason this file is right for both machines without
//! a `cfg` anywhere near the number: it is **117** on aarch64 and **101** on
//! x86_64, and writing either down here would be the failure the crate's rule
//! exists to prevent. The request *words* come from `libc::PTRACE_*` where the
//! binding reaches them; the two it does not bind (musl's `libc` module stops at
//! `PTRACE_SETSIGMASK`, 0x420b) are named here with the value the header gives.
//! The `-libc` suffix follows `t-unix-rustix` and `t-process-vm-libc`: the
//! family, named with the interface it is driven through.
//!
//! **What is pinned**, in the order a tracer reaches for it:
//!
//! * `PTRACE_TRACEME` writes the link, and the stop it produces arrives at the
//!   parent's `wait` as a `SIGSTOP` stop.
//! * `PTRACE_GET_SYSCALL_INFO` at that stop - a *signal* stop, not a syscall
//!   boundary - reports `NONE` with the machine's `arch` word and the tracee's
//!   own `ip`/`sp`. `strace` rejects the answer outright if either pointer is
//!   zero, and having rejected it stops asking for the structure for the rest of
//!   the session, which is what makes every later return value read as the next
//!   call's number. A buffer too small is `EINVAL`, because the structure has a
//!   union in it and half of it describes the wrong thing rather than less of
//!   the right one.
//! * `PTRACE_SETOPTIONS` takes the bits this kernel knows and refuses the rest
//!   with `EINVAL` - a silently dropped option is a tracer waiting for a stop
//!   that never comes.
//! * `PTRACE_GETREGSET` with `NT_PRSTATUS` fills the register file the machine
//!   declares, and the `iovec`'s length comes back as that size.
//! * `PTRACE_SYSCALL` resumes the tracee to its next syscall boundary, and *that*
//!   stop reports `ENTRY` with the number of the call being made.
//! * `PTRACE_PEEKDATA` reads a word of the tracee's memory - the fallback
//!   `strace` keeps for a target it cannot reach with `process_vm_readv`.
//! * `PTRACE_CONT` lets it run to completion, and the exit status is the child's.
//! * And the thing a syscall-table sandbox is built from: a `PTRACE_SETREGS`
//!   that rewrites the *number* at a `PTRACE_SYSCALL` entry stop changes which
//!   call runs - the redirect's ordinary half, next to the seccomp-stop one
//!   `t-seccomp` pins.
//! * And the same write's two other stops: the `-1` sentinel *cancels* the call
//!   at an entry stop - it does not run at all, and the tracee reads `-ENOSYS` -
//!   while the same write at the *exit* stop is a no-op, because by then the
//!   call has already run.

use std::io;
use std::os::raw::{c_long, c_void};
use std::process::exit;

/// The two request words musl's `libc` does not bind. Both are the header's:
/// `PTRACE_GET_SYSCALL_INFO` is `0x420e` in `linux/ptrace.h`, and `TRACESYSGOOD`
/// is bit 0 of the `PTRACE_O_*` set the man page lists.
const PTRACE_GET_SYSCALL_INFO: c_long = 0x420e;
const PTRACE_O_TRACESYSGOOD: c_long = 1;

/// `struct ptrace_syscall_info`'s fixed size, and the offsets the ABI gives its
/// fields. The union at the end is why this is 80 and not 24: the exit shape is
/// written into the same 56 bytes the entry shape uses.
const SYSCALL_INFO_BYTES: usize = 80;
const OP: usize = 0;
const ARCH: usize = 4;
const IP: usize = 8;
const SP: usize = 16;
const NR: usize = 24;
const ARGS: usize = 32;
const OP_NONE: u8 = 0;
const OP_ENTRY: u8 = 1;
const OP_EXIT: u8 = 2;

const NT_PRSTATUS: c_long = 1;

/// The register file's width, which is the one thing here that is the
/// machine's: `struct user_regs_struct` is 27 words on x86_64 and `struct
/// user_pt_regs` is 34 on aarch64, and the slot the program counter occupies
/// follows from the same two layouts (x86_64's `rip` is 16th, aarch64's `pc`
/// sits after its 31 registers and `sp`).
#[cfg(not(target_arch = "aarch64"))]
const REG_WORDS: usize = 27;
#[cfg(target_arch = "aarch64")]
const REG_WORDS: usize = 34;
const REG_BYTES: usize = REG_WORDS * 8;
#[cfg(not(target_arch = "aarch64"))]
const PC_SLOT: usize = 16;
#[cfg(target_arch = "aarch64")]
const PC_SLOT: usize = 32;

/// The register that carries the syscall number - the slot a tracer rewrites to
/// redirect a call. x86_64's `user_regs_struct` names it `orig_rax`, the 16th
/// word; aarch64 passes the number in `x8`, the 9th. `t-seccomp` states the same
/// two, for the same request.
#[cfg(not(target_arch = "aarch64"))]
const NUMBER_SLOT: usize = 15;
#[cfg(target_arch = "aarch64")]
const NUMBER_SLOT: usize = 8;

// The errnos, as Linux numbers them - the same on both machines.
const ESRCH: i32 = 3;
const EIO: i32 = 5;
const EINVAL: i32 = 22;
const ENOSYS: i32 = 38;

/// The report shape the std-side tests share: a name, whether it held, and a
/// failure flag the exit status is built from.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: i32, want: i32) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }

    fn check_call(&mut self, name: &str, got: Result<isize, i32>, want: isize) {
        self.check(
            &format!("{name} (got {got:?}, want {want})"),
            got == Ok(want),
        );
    }
}

/// One `ptrace` call, with the two words the request numbers do not use passed
/// as zero. The number is `libc::SYS_ptrace`, which is the ABI's per-arch table
/// rather than this file's memory of it.
fn ptrace(request: c_long, pid: libc::pid_t, addr: usize, data: usize) -> Result<isize, i32> {
    // SAFETY: `syscall` is a libc entry point; the arguments are the widths the
    // ABI's registers take, and `ptrace`'s own contract is the caller's - every
    // call below passes a pointer this process owns, or null.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ptrace,
            request,
            pid as c_long,
            addr as c_long,
            data as c_long,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        // `libc::syscall` answers `c_long`, which is `i64` on both of these
        // machines and a distinct type from `isize` in the compiler's eyes
        // even where the two are the same width.
        Ok(rc as isize)
    }
}

fn main() {
    let mut r = Report { failed: false };

    // ---- the refusals, asked from a process with nothing to trace ----
    //
    // A pid with no task behind it. The target is named by pid and not by
    // relationship, which is what makes `strace -p` possible at all.
    r.check_errno(
        "a pid past the task table names no task",
        ptrace(libc::PTRACE_PEEKDATA as c_long, 9999, 0, 0).unwrap_err(),
        ESRCH,
    );
    // A request this kernel does not implement, asked of a pid that is not a
    // tracee. The attach check comes first in Linux too (`ptrace_request` calls
    // `ptrace_check_attach` before its switch), so the honest answer here is
    // "no such tracee" rather than "no such request" - and the request-word
    // refusal is pinned below, against a child that *is* one.
    r.check_errno(
        "an unknown request against a pid that is not a tracee is ESRCH",
        ptrace(0x7fff, std::process::id() as libc::pid_t, 0, 0).unwrap_err(),
        ESRCH,
    );

    // ---- and the trace, on a child of our own ----
    //
    // SAFETY: called before this process has any other thread, so the child
    // inherits a single-threaded program. It allocates nothing after the fork;
    // it calls `ptrace`, raises a stop and exits, and every one of those is a
    // straight syscall.
    let child = unsafe { libc::fork() };
    if child == 0 {
        traced_child();
    }
    r.check("fork gave the parent a child pid", child > 0);
    if child <= 0 {
        r.check("the trace could be attempted", false);
        exit(1);
    }

    trace_from_parent(&mut r, child);
    entry_stop_rewrite_changes_the_call(&mut r);
    entry_stop_cancel_skips_the_call(&mut r);
    exit_stop_number_write_is_a_no_op(&mut r);

    if r.failed {
        println!("t-ptrace: FAILED");
        exit(1);
    }
    println!("t-ptrace: all passed");
}

/// The tracee's whole life: hand itself over, stop, make one call, leave.
///
/// Every path out is `_exit`, because this side must never run destructors or
/// flush a `std` buffer the parent would then see twice. The two exit codes
/// below are the tracee's own diagnosis: they are what the parent reports when
/// the handshake fails early, and they are distinct so that a failure says
/// *which* step it was.
fn traced_child() -> ! {
    // SAFETY: as `main`'s fork note - syscalls only, no allocation, and
    // `_exit` does not return.
    unsafe {
        // The link, written from this side because that is what the request is
        // for: a tracee offers itself and the only tracer it may name is the
        // task that spawned it.
        if ptrace(libc::PTRACE_TRACEME as c_long, 0, 0, 0) != Ok(0) {
            libc::_exit(11);
        }
        // A `SIGSTOP` of this process's own making. With `TRACEME` written, it
        // is delivered as a stop to the tracer rather than to us, and it is what
        // the tracer's first `wait` returns.
        libc::raise(libc::SIGSTOP);
        // The first syscall after the stop. The tracer resumes for one boundary
        // and reads this call's number out of the structure, so the number is
        // the assertion on that side.
        let me = libc::getpid();
        libc::_exit(if me > 0 { 0 } else { 12 });
    }
}

/// A `PTRACE_SETREGS` at an ordinary `PTRACE_SYSCALL` *entry* stop redirects the
/// call.
///
/// The tracer's own version of the redirect `t-seccomp` pins at a seccomp stop:
/// no filter, no event - just the syscall boundary any tracer asks for. The
/// tracee asks for `getpid` and the tracer rewrites the number slot to
/// `getppid`, and the call that runs is the one that was written. Measured on
/// this developer's Linux first (kernel 7.2): the judged `getpid` stopped with
/// the number slot reading 39, and rewriting it to 110 made the tracee's call
/// answer its parent's pid - and the stop after it read the new number back.
///
/// Two halves have to reach the frame for that to happen, and the pin is
/// counterfactual against both: the kernel has to read the number back out of
/// the register file *after* the stop rather than dispatching the one it arrived
/// with, and the tracer's write has to land in the slot the gate reads
/// (`orig_rax` on x86_64, which restoring the file does not carry because the
/// number and the result share `rax`).
fn entry_stop_rewrite_changes_the_call(r: &mut Report) {
    // This process's pid, which is what the child's `getppid` answers - and what
    // the `getpid` it actually asks for cannot.
    let me = std::process::id() as libc::pid_t;
    // SAFETY: as `main`'s fork note - syscalls only, no allocation, `_exit`.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: as above.
        unsafe {
            if ptrace(libc::PTRACE_TRACEME as c_long, 0, 0, 0) != Ok(0) {
                libc::_exit(11);
            }
            libc::raise(libc::SIGSTOP);
            // The judged call. Its *answer* is the verdict, so it is made
            // directly rather than through a wrapper that might cache one:
            // `getpid` answers this process's own id, and this process is not
            // its own parent, so only the rewritten `getppid` can answer `me`.
            let got = libc::syscall(libc::SYS_getpid) as libc::pid_t;
            libc::_exit(if got > 0 && got == me { 0 } else { 13 });
        }
    }
    r.check("the redirect's tracee was forked", child > 0);
    if child <= 0 {
        return;
    }
    let mut status: libc::c_int = 0;
    // SAFETY: `child` is this process's own child and `status` is a local.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the redirect's tracee stops for TRACEME",
        n == child && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );
    if n != child {
        return;
    }

    // Walk to the judged call's *entry* the way step 5 does: the tracee was
    // inside a syscall when it stopped, so the next boundary may be that call's
    // exit rather than the entry being looked for.
    let mut info = [0u8; SYSCALL_INFO_BYTES];
    let base = info.as_mut_ptr() as usize;
    let mut at_entry = false;
    for _ in 0..8 {
        if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) != Ok(0) {
            break;
        }
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n != child || !libc::WIFSTOPPED(status) {
            break;
        }
        if ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
            != Ok(SYSCALL_INFO_BYTES as isize)
        {
            break;
        }
        let nr = u64::from_le_bytes(info[NR..NR + 8].try_into().unwrap());
        if info[OP] == OP_ENTRY && nr == libc::SYS_getpid as u64 {
            at_entry = true;
            break;
        }
    }
    r.check("the judged call's entry stop arrives", at_entry);
    if !at_entry {
        return;
    }

    // The redirect itself. The slot is the machine's; the number it is given is
    // `libc`'s per-machine one, never written down here.
    let mut regs = [0u64; REG_WORDS];
    let read = ptrace(
        libc::PTRACE_GETREGS as c_long,
        child,
        0,
        regs.as_mut_ptr() as usize,
    );
    r.check_call(
        "the tracer reads the tracee's registers at the entry stop",
        read,
        0,
    );
    r.check(
        &format!(
            "the number slot reads the call the tracee asked for (got {}, want {})",
            regs[NUMBER_SLOT],
            libc::SYS_getpid
        ),
        regs[NUMBER_SLOT] == libc::SYS_getpid as u64,
    );
    regs[NUMBER_SLOT] = libc::SYS_getppid as u64;
    let wrote = ptrace(
        libc::PTRACE_SETREGS as c_long,
        child,
        0,
        regs.as_ptr() as usize,
    );
    r.check_call(
        "the tracer rewrites the number slot at the entry stop",
        wrote,
        0,
    );

    // The other half of the same call. The value it reports is the whole
    // assertion: `me` is the tracee's parent, which is the answer the rewritten
    // `getppid` gives and the `getpid` it asked for cannot.
    let mut saw_exit = None;
    if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) == Ok(0) {
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n == child
            && libc::WIFSTOPPED(status)
            && ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
                == Ok(SYSCALL_INFO_BYTES as isize)
            && info[OP] == OP_EXIT
        {
            saw_exit = Some(i64::from_le_bytes(info[NR..NR + 8].try_into().unwrap()));
        }
    }
    r.check(
        &format!("the exit stop reports the parent's pid ({saw_exit:?}, want {me})"),
        saw_exit == Some(me as i64),
    );

    r.check_call(
        "PTRACE_CONT releases the redirected tracee",
        ptrace(libc::PTRACE_CONT as c_long, child, 0, 0),
        0,
    );
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!(
            "the tracee saw the redirect too (exit {})",
            libc::WEXITSTATUS(status)
        ),
        n == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
}

/// The other thing a `PTRACE_SETREGS` at an entry stop can do with the number
/// slot: **cancel** the call - Linux's `nr = -1` sentinel - rather than replace
/// it with another number.
///
/// Measured on this developer's Linux first (kernel 7.2,
/// `.workbuddy/probe-ptrace-cancel.c`): writing `-1` into the number slot at a
/// `write`'s entry stop leaves the pipe **empty**, the tracee's `syscall()`
/// answers `-1` with `ENOSYS`, and the exit stop's result is `-ENOSYS` (`rax =
/// -38`). The same probe showed two neighbours worth stating rather than
/// assuming: an **out-of-range** number (`9999`) is the *same* answer - the call
/// does not run and the result is `-ENOSYS`, so `-1` is not special, only
/// unreachable - and a write at the **exit** stop is a no-op, because by then
/// the call has already run.
///
/// The pipe is what makes this worth pinning rather than assuming: a kernel
/// that answered `ENOSYS` *after* making the call would satisfy an errno check
/// and still have written the bytes. Only an empty pipe proves the call was
/// skipped.
fn entry_stop_cancel_skips_the_call(r: &mut Report) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a local of two ints.
    let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
    r.check("the cancel probe's pipe is made", made == 0);
    if made != 0 {
        return;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // SAFETY: as `main`'s fork note - syscalls only, no allocation, `_exit`.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: as above.
        unsafe {
            libc::close(read_fd);
            // The tracee's stdout becomes the pipe, so the only way bytes get
            // into it is the very call the tracer is about to cancel.
            if libc::dup2(write_fd, 1) < 0 {
                libc::_exit(11);
            }
            libc::close(write_fd);
            if ptrace(libc::PTRACE_TRACEME as c_long, 0, 0, 0) != Ok(0) {
                libc::_exit(11);
            }
            libc::raise(libc::SIGSTOP);
            let got = libc::syscall(libc::SYS_write, 1 as c_long, b"hello".as_ptr(), 5 as c_long);
            let enosys =
                got == -1 && std::io::Error::last_os_error().raw_os_error() == Some(ENOSYS);
            libc::_exit(if enosys { 0 } else { 13 });
        }
    }
    r.check("the cancel probe's tracee was forked", child > 0);
    // The parent's own copy of the write end has to go, or the read below never
    // reaches end-of-stream.
    // SAFETY: this process's own descriptor.
    unsafe { libc::close(write_fd) };
    if child <= 0 {
        // SAFETY: this process's own descriptor.
        unsafe { libc::close(read_fd) };
        return;
    }

    let mut status: libc::c_int = 0;
    // SAFETY: `child` is this process's own child and `status` is a local.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the cancel probe's tracee stops for TRACEME",
        n == child && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );
    if n != child {
        // SAFETY: this process's own descriptor.
        unsafe { libc::close(read_fd) };
        return;
    }

    // Walk to the judged `write`'s entry stop the way the redirect above does.
    let mut info = [0u8; SYSCALL_INFO_BYTES];
    let base = info.as_mut_ptr() as usize;
    let mut at_entry = false;
    for _ in 0..8 {
        if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) != Ok(0) {
            break;
        }
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n != child || !libc::WIFSTOPPED(status) {
            break;
        }
        if ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
            != Ok(SYSCALL_INFO_BYTES as isize)
        {
            break;
        }
        let nr = u64::from_le_bytes(info[NR..NR + 8].try_into().unwrap());
        if info[OP] == OP_ENTRY && nr == libc::SYS_write as u64 {
            at_entry = true;
            break;
        }
    }
    r.check("the judged write's entry stop arrives", at_entry);
    if !at_entry {
        // SAFETY: this process's own descriptor.
        unsafe { libc::close(read_fd) };
        return;
    }

    let mut regs = [0u64; REG_WORDS];
    let read = ptrace(
        libc::PTRACE_GETREGS as c_long,
        child,
        0,
        regs.as_mut_ptr() as usize,
    );
    r.check_call(
        "the tracer reads the registers at the judged write's entry stop",
        read,
        0,
    );
    // The sentinel, in the machine's slot. `-1` in the number slot is the
    // cancel, written as the all-ones word because that is the slot's width.
    regs[NUMBER_SLOT] = u64::MAX;
    r.check(
        "the tracer cancels the call by writing -1 into the number slot",
        ptrace(
            libc::PTRACE_SETREGS as c_long,
            child,
            0,
            regs.as_ptr() as usize,
        ) == Ok(0),
    );

    // The exit stop. At an *exit* stop the structure's second field is the
    // *result*, not the number (`struct ptrace_syscall_info`'s union), and the
    // measured answer for a cancelled call is `-ENOSYS`.
    let mut saw_result = None;
    if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) == Ok(0) {
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n == child
            && libc::WIFSTOPPED(status)
            && ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
                == Ok(SYSCALL_INFO_BYTES as isize)
            && info[OP] == OP_EXIT
        {
            saw_result = Some(i64::from_le_bytes(info[NR..NR + 8].try_into().unwrap()));
        }
    }
    r.check(
        &format!("the exit stop's result is -ENOSYS ({saw_result:?}, want -38)"),
        saw_result == Some(-(ENOSYS as i64)),
    );

    r.check_call(
        "PTRACE_CONT releases the cancelled tracee",
        ptrace(libc::PTRACE_CONT as c_long, child, 0, 0),
        0,
    );
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!(
            "the tracee read the cancel as ENOSYS (exit {})",
            libc::WEXITSTATUS(status)
        ),
        n == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // The proof that the call was *skipped* rather than failed after running.
    // Read after the tracee is gone, so its write end is closed and this cannot
    // block.
    let mut buf = [0u8; 16];
    // SAFETY: this process's own descriptor and a local buffer.
    let got = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
    r.check(
        &format!("the cancelled call wrote nothing (read {got} bytes)"),
        got == 0,
    );
    // SAFETY: this process's own descriptor.
    unsafe { libc::close(read_fd) };
}

/// The number slot's third stop: a write at the **exit** stop is a no-op.
///
/// The call has already run by then, so the number names nothing that can still
/// happen. Measured on this developer's Linux first (kernel 7.2,
/// `.workbuddy/probe-ptrace-cancel.c`, case 4): rewriting `orig_rax` at a
/// `getpid`'s exit stop left the tracee's answer as its own real pid. This is
/// worth a pin because the natural way to get the cancel wrong is to apply it
/// at the wrong stop - a kernel that treated the exit stop's `-1` as a cancel
/// would throw away a result the tracee already earned.
fn exit_stop_number_write_is_a_no_op(r: &mut Report) {
    // SAFETY: as `main`'s fork note - syscalls only, no allocation, `_exit`.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: as above.
        unsafe {
            if ptrace(libc::PTRACE_TRACEME as c_long, 0, 0, 0) != Ok(0) {
                libc::_exit(11);
            }
            libc::raise(libc::SIGSTOP);
            // The call's own answer is the verdict: a real pid means it ran and
            // its result survived.
            let got = libc::syscall(libc::SYS_getpid) as isize;
            libc::_exit(if got > 0 { 0 } else { 13 });
        }
    }
    r.check("the exit-write probe's tracee was forked", child > 0);
    if child <= 0 {
        return;
    }
    let mut status: libc::c_int = 0;
    // SAFETY: `child` is this process's own child and `status` is a local.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the exit-write probe's tracee stops for TRACEME",
        n == child && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );
    if n != child {
        return;
    }

    let mut info = [0u8; SYSCALL_INFO_BYTES];
    let base = info.as_mut_ptr() as usize;
    let mut at_entry = false;
    for _ in 0..8 {
        if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) != Ok(0) {
            break;
        }
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n != child || !libc::WIFSTOPPED(status) {
            break;
        }
        if ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
            != Ok(SYSCALL_INFO_BYTES as isize)
        {
            break;
        }
        let nr = u64::from_le_bytes(info[NR..NR + 8].try_into().unwrap());
        if info[OP] == OP_ENTRY && nr == libc::SYS_getpid as u64 {
            at_entry = true;
            break;
        }
    }
    r.check("the judged getpid's entry stop arrives", at_entry);
    if !at_entry {
        return;
    }

    // One boundary further: the judged call's exit stop, where the structure's
    // second field is the result.
    let mut saw_result = None;
    if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) == Ok(0) {
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n == child
            && libc::WIFSTOPPED(status)
            && ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
                == Ok(SYSCALL_INFO_BYTES as isize)
            && info[OP] == OP_EXIT
        {
            saw_result = Some(i64::from_le_bytes(info[NR..NR + 8].try_into().unwrap()));
        }
    }
    r.check(
        &format!("the exit stop reports the call's real result ({saw_result:?}, want {child})"),
        saw_result == Some(child as i64),
    );

    let mut regs = [0u64; REG_WORDS];
    let read = ptrace(
        libc::PTRACE_GETREGS as c_long,
        child,
        0,
        regs.as_mut_ptr() as usize,
    );
    r.check_call("the tracer reads the registers at the exit stop", read, 0);
    // The same sentinel that *cancels* at an entry stop, written here.
    regs[NUMBER_SLOT] = u64::MAX;
    r.check(
        "the tracer rewrites the number slot at the exit stop",
        ptrace(
            libc::PTRACE_SETREGS as c_long,
            child,
            0,
            regs.as_ptr() as usize,
        ) == Ok(0),
    );

    r.check_call(
        "PTRACE_CONT releases the exit-written tracee",
        ptrace(libc::PTRACE_CONT as c_long, child, 0, 0),
        0,
    );
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!(
            "the tracee kept the result the call already earned (exit {})",
            libc::WEXITSTATUS(status)
        ),
        n == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
}

/// The tracer's half, in the order a tracer reaches for it.
fn trace_from_parent(r: &mut Report, child: libc::pid_t) {
    let mut status: libc::c_int = 0;

    // ---- 1. the stop `TRACEME` produces ----
    // SAFETY: `child` is this process's own child and `status` is a local.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!(
            "the tracee's first stop arrives as a SIGSTOP \
             (waitpid {n}, status {status:#x}: stopped={} signalled={} sig={} exited={} code={})",
            libc::WIFSTOPPED(status),
            libc::WIFSIGNALED(status),
            libc::WSTOPSIG(status),
            libc::WIFEXITED(status),
            libc::WEXITSTATUS(status),
        ),
        n == child && libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );
    // And the request word this kernel does not know, now against a child that
    // is one: `EIO` is Linux's answer for a request it cannot serve, and it is
    // what a tracer's feature probe reads to decide to fall back rather than to
    // give up.
    r.check_errno(
        "an unknown request against the tracee is EIO",
        ptrace(0x7fff, child, 0, 0).unwrap_err(),
        EIO,
    );
    if n != child {
        return;
    }

    // ---- 2. strace's own startup probe: the syscall-info structure ----
    let mut info = [0u8; SYSCALL_INFO_BYTES];
    let base = info.as_mut_ptr() as usize;
    r.check_call(
        "PTRACE_GET_SYSCALL_INFO fills the structure",
        ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base),
        SYSCALL_INFO_BYTES as isize,
    );
    r.check("a signal stop reports NONE", info[OP] == OP_NONE);
    let arch = u32::from_le_bytes(info[ARCH..ARCH + 4].try_into().unwrap());
    r.check("the arch word is the machine's", arch != 0);
    let ip = u64::from_le_bytes(info[IP..IP + 8].try_into().unwrap());
    let sp = u64::from_le_bytes(info[SP..SP + 8].try_into().unwrap());
    // This is the pair `strace` refuses the whole structure over: a `NONE`
    // answer with either zero makes it stop asking, and assume the stops
    // alternate instead - one dropped stop away from reading every result as
    // the next call's number.
    r.check(
        "ip and sp are the tracee's own even at a signal stop",
        ip != 0 && sp != 0,
    );

    // A buffer the caller chose to be too small. The structure has a union in
    // it, so this is a refusal and not a short answer.
    r.check_errno(
        "a buffer too small for the structure is EINVAL",
        ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES - 1, base).unwrap_err(),
        EINVAL,
    );

    // ---- 3. the options, and the bits this kernel does not have ----
    r.check_call(
        "PTRACE_SETOPTIONS takes TRACESYSGOOD",
        ptrace(
            libc::PTRACE_SETOPTIONS as c_long,
            child,
            0,
            PTRACE_O_TRACESYSGOOD as usize,
        ),
        0,
    );
    r.check_errno(
        "an option bit this kernel does not know is EINVAL",
        ptrace(libc::PTRACE_SETOPTIONS as c_long, child, 0, 1usize << 31).unwrap_err(),
        EINVAL,
    );

    // ---- 4. the register file, by name and through an iovec ----
    let mut regs = [0u64; REG_WORDS];
    let mut iov = libc::iovec {
        iov_base: regs.as_mut_ptr().cast::<c_void>(),
        iov_len: REG_BYTES,
    };
    r.check_call(
        "PTRACE_GETREGSET(NT_PRSTATUS) fills the file",
        ptrace(
            libc::PTRACE_GETREGSET as c_long,
            child,
            NT_PRSTATUS as usize,
            &mut iov as *mut libc::iovec as usize,
        ),
        0,
    );
    // The length moved goes back into the caller's `iovec`, which is how a
    // tracer tells a full answer from a truncated one.
    r.check(
        "the iovec reports the width this machine's ABI declares",
        iov.iov_len == REG_BYTES,
    );
    // The tracee is stopped inside a syscall it made, so its own program
    // counter is a live address in its text and not a word the tracer was
    // handed a plausible-looking zero for.
    r.check(
        "the program counter names the tracee's own instruction",
        regs[PC_SLOT] != 0,
    );

    // ---- 5. resume to the next syscall boundary ----
    //
    // A tracer does not know how many boundaries stand between it and the call
    // it is looking for: the tracee was somewhere inside a syscall when it
    // stopped, and a resume may report that call's exit before the next call's
    // entry. So this walks the boundaries the way `strace` does and stops at the
    // one whose *entry* names the call the child makes next - which is exact
    // rather than tolerant: the assertion is that this stop arrives, not that it
    // is the first one.
    r.check_call(
        "PTRACE_SYSCALL resumes the tracee",
        ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0),
        0,
    );
    let mut saw_entry = None;
    for _ in 0..8 {
        // SAFETY: as above - `child` is this process's child, `status` a local.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n != child || !libc::WIFSTOPPED(status) {
            break;
        }
        // `TRACESYSGOOD` is what puts the 0x80 in the stop signal, and it is why
        // a tracer can tell a syscall boundary from a signal - the one bit that
        // makes the option worth asking for.
        r.check(
            "a syscall stop is a SIGTRAP with the 0x80 bit set",
            libc::WSTOPSIG(status) == (libc::SIGTRAP | 0x80),
        );
        let got = ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base);
        if got != Ok(SYSCALL_INFO_BYTES as isize) {
            break;
        }
        let nr = u64::from_le_bytes(info[NR..NR + 8].try_into().unwrap());
        if info[OP] == OP_ENTRY && nr == libc::SYS_getpid as u64 {
            saw_entry = Some(());
            break;
        }
        // Not the call being looked for - the tracee was mid-syscall when it
        // stopped. Walk on.
        if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) != Ok(0) {
            break;
        }
    }
    r.check(
        "a boundary stop reports ENTRY with the call the tracee is making",
        saw_entry.is_some(),
    );
    let sp = u64::from_le_bytes(info[SP..SP + 8].try_into().unwrap());

    // ---- 6. the tracee's memory, one word at a time ----
    //
    // The fallback a tracer keeps for a target `process_vm_readv` cannot reach.
    // `sp` is an address the tracee is using, so the word under it is mapped and
    // reachable: the request must answer a *word* rather than `EFAULT`. That is
    // the whole point - a libc calls this as `ptrace(PEEKDATA, pid, addr, 0)`,
    // so a kernel that treated the fourth argument as a buffer to write into
    // would refuse every read a tracer makes.
    let word = ptrace(libc::PTRACE_PEEKDATA as c_long, child, sp as usize, 0);
    r.check(
        "PTRACE_PEEKDATA answers a word out of the tracee's stack",
        word.is_ok(),
    );
    // And the read is of the *tracee*: the word under its stack pointer is not
    // this process's, so a kernel that read the tracer's own memory (the
    // mistake the request-number test next door warns about) would answer a
    // word that matches this process's stack instead.
    let mut own = [0u8; 8];
    let own_at = own.as_ptr() as usize;
    let mine = ptrace(libc::PTRACE_PEEKDATA as c_long, child, own_at, 0);
    r.check(
        "what it reads is the tracee's memory and not the tracer's",
        mine.is_ok() && mine != word,
    );

    // `PTRACE_PEEKUSER` answers the same way, and the register file it reads is
    // the one `GETREGSET` fills - so re-reading both at the *same* stop has to
    // leave them agreeing, slot for slot. (The file read before the boundary
    // walk is stale here: the tracee has run since.)
    let mut iov2 = libc::iovec {
        iov_base: regs.as_mut_ptr().cast::<c_void>(),
        iov_len: REG_BYTES,
    };
    r.check_call(
        "the register file can be re-read at this stop",
        ptrace(
            libc::PTRACE_GETREGSET as c_long,
            child,
            NT_PRSTATUS as usize,
            &mut iov2 as *mut libc::iovec as usize,
        ),
        0,
    );
    let peeked = ptrace(libc::PTRACE_PEEKUSER as c_long, child, PC_SLOT * 8, 0);
    r.check(
        &format!(
            "PTRACE_PEEKUSER answers the register, not through a pointer \
             (got {peeked:?}, want {:?})",
            regs[PC_SLOT] as isize
        ),
        peeked == Ok(regs[PC_SLOT] as isize),
    );

    // ---- 7. the exit half of the same call ----
    //
    // The union's other shape. An exit stop reports the return value at offset
    // 24 - where the entry shape keeps the number - and the value here is not a
    // stand-in: `getpid` answers with the tracee's pid, which this side knows
    // because `fork` gave it.
    r.check_call(
        "PTRACE_SYSCALL resumes to the call's other half",
        ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0),
        0,
    );
    let mut saw_exit = None;
    for _ in 0..8 {
        // SAFETY: as above.
        let n = unsafe { libc::waitpid(child, &mut status, 0) };
        if n != child || !libc::WIFSTOPPED(status) {
            break;
        }
        if ptrace(PTRACE_GET_SYSCALL_INFO, child, SYSCALL_INFO_BYTES, base)
            != Ok(SYSCALL_INFO_BYTES as isize)
        {
            break;
        }
        // The first stop after the entry is that call's exit - the exit shape
        // carries no syscall number at all, only the result, so there is
        // nothing else to match on and nothing else it can be.
        if info[OP] == OP_EXIT {
            let rval = i64::from_le_bytes(info[NR..NR + 8].try_into().unwrap());
            saw_exit = Some(rval);
            r.check(
                "the exit shape's error flag is clear for a call that ran",
                info[ARGS] == 0,
            );
            break;
        }
        if ptrace(libc::PTRACE_SYSCALL as c_long, child, 0, 0) != Ok(0) {
            break;
        }
    }
    r.check(
        &format!("the exit stop reports the call's return value ({saw_exit:?}, want {child})"),
        saw_exit == Some(child as i64),
    );

    // ---- 8. let it finish ----
    r.check_call(
        "PTRACE_CONT releases the tracee",
        ptrace(libc::PTRACE_CONT as c_long, child, 0, 0),
        0,
    );
    // SAFETY: as above.
    let n = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the tracee ran to its own exit",
        n == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
}
