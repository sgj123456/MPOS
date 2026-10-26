//! t-ptrace-syscall-info - what a stop says it is, at a signal and at a call.
//!
//! `PTRACE_GET_SYSCALL_INFO` is what a tracer builds its decoder on, and the two
//! answers pinned here are the ones its state machine hangs from:
//!
//! * at a stop that is **not** a syscall boundary - a signal delivery, the
//!   `SIGSTOP` an attach injects - the operation must be
//!   `PTRACE_SYSCALL_INFO_NONE`. `strace`'s startup probe asks exactly that and
//!   *rejects* any other answer (its check is quoted in `syscall/linux/proc.rs`),
//!   after which it stops asking and assumes the stops alternate. This kernel
//!   used to answer `ENTRY` with the last call the task had entered, and the
//!   result was a tracer one phase behind that killed its tracee on a fault it
//!   caused (measured: `ptrace_syscall_info.op == 2` at an entering stop, then a
//!   user page fault at `0x8c`);
//! * at the syscall stops themselves the structure must name the call and carry
//!   its result. The child makes one named call (`getpid`) so the number asserted
//!   is one the test chose; the walk in between expects the signal trampoline's
//!   own calls first (`rt_sigreturn`), which is why it looks for the entry by
//!   number rather than assuming it is the first stop.
//!
//! `libc` rather than a `rustix` wrapper: no wrapper exists for the request, and
//! the structure is the kernel's ABI, so it is spelled here - the crate's allowed
//! case.

use mpos_user_tests::Report;

/// `PTRACE_GET_SYSCALL_INFO`, from Linux's `ptrace.h`.
const PTRACE_GET_SYSCALL_INFO: libc::c_int = 0x420e;
const SYSCALL_INFO_NONE: u8 = 0;
const SYSCALL_INFO_ENTRY: u8 = 1;
const SYSCALL_INFO_EXIT: u8 = 2;

#[repr(C)]
#[derive(Default, Copy, Clone)]
struct SyscallEntry {
    nr: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
struct SyscallExit {
    rval: i64,
    is_error: u8,
}

#[repr(C)]
union Payload {
    entry: SyscallEntry,
    exit: SyscallExit,
    pad: [u8; 64],
}

#[repr(C)]
struct SyscallInfo {
    op: u8,
    pad: [u8; 3],
    arch: u32,
    instruction_pointer: u64,
    stack_pointer: u64,
    payload: Payload,
}

fn ptrace(
    request: libc::c_int,
    pid: libc::pid_t,
    addr: *mut libc::c_void,
    data: *mut libc::c_void,
) -> isize {
    // SAFETY: each caller passes the argument word its request documents.
    unsafe { libc::ptrace(request, pid, addr, data) as isize }
}

fn info_of(child: libc::pid_t) -> Option<SyscallInfo> {
    let mut info = SyscallInfo {
        op: 0,
        pad: [0; 3],
        arch: 0,
        instruction_pointer: 0,
        stack_pointer: 0,
        payload: Payload { pad: [0; 64] },
    };
    let size = core::mem::size_of::<SyscallInfo>();
    let answer = ptrace(
        PTRACE_GET_SYSCALL_INFO,
        child,
        size as *mut libc::c_void,
        &mut info as *mut SyscallInfo as *mut libc::c_void,
    );
    if answer <= 0 { None } else { Some(info) }
}

fn main() {
    println!("t-ptrace-syscall-info");
    let mut r = Report::new();

    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: the child calls these libc functions and exits; nothing it
        // touches outlives it.
        unsafe {
            libc::ptrace(
                libc::PTRACE_TRACEME as libc::c_int,
                0,
                core::ptr::null_mut::<libc::c_void>(),
                core::ptr::null_mut::<libc::c_void>(),
            );
            libc::raise(libc::SIGSTOP);
            libc::syscall(libc::SYS_getpid as libc::c_long);
            libc::_exit(0);
        }
    }
    r.check("a child to trace exists", child > 0);
    if child <= 0 {
        r.finish();
    }

    let mut status: libc::c_int = 0;
    let _ = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the attach stop is a SIGSTOP",
        libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == libc::SIGSTOP,
    );

    // The signal stop: strace's probe asks here, and the only operation that
    // keeps a tracer's state machine in phase is NONE.
    match info_of(child) {
        Some(info) => {
            r.check(
                "a signal stop answers PTRACE_SYSCALL_INFO_NONE",
                info.op == SYSCALL_INFO_NONE,
            );
            // The probe also rejects a NONE answer with no address: the structure
            // is the tracer's proof that the kernel filled it in.
            r.check(
                "the NONE answer still carries the tracee's ip and sp",
                info.instruction_pointer != 0 && info.stack_pointer != 0,
            );
        }
        None => r.check("PTRACE_GET_SYSCALL_INFO answers at a signal stop", false),
    }

    let _ = ptrace(
        libc::PTRACE_SETOPTIONS,
        child,
        core::ptr::null_mut::<libc::c_void>(),
        libc::PTRACE_O_TRACESYSGOOD as usize as *mut libc::c_void,
    );

    // Walk the boundaries until the call the child made. The tracer's own
    // trampoline calls come first, so the entry is found by number.
    let mut found_entry = false;
    let mut found_exit = false;
    for _ in 0..12 {
        if ptrace(
            libc::PTRACE_SYSCALL,
            child,
            core::ptr::null_mut::<libc::c_void>(),
            core::ptr::null_mut::<libc::c_void>(),
        ) < 0
        {
            break;
        }
        if unsafe { libc::waitpid(child, &mut status, 0) } != child || !libc::WIFSTOPPED(status) {
            break;
        }
        if libc::WSTOPSIG(status) & 0x80 == 0 {
            continue;
        }
        let Some(info) = info_of(child) else { continue };
        if !found_entry && info.op == SYSCALL_INFO_ENTRY {
            // SAFETY: `op` is the entry variant.
            if unsafe { info.payload.entry.nr } == libc::SYS_getpid as u64 {
                found_entry = true;
            }
        } else if found_entry && info.op == SYSCALL_INFO_EXIT {
            // SAFETY: `op` is the exit variant.
            found_exit = unsafe { info.payload.exit.rval } == child as i64;
            break;
        }
    }
    r.check(
        "the entry stop names the call the child made (getpid)",
        found_entry,
    );
    r.check(
        "the exit stop carries that call's result (the child's pid)",
        found_exit,
    );

    let _ = ptrace(
        libc::PTRACE_DETACH,
        child,
        core::ptr::null_mut::<libc::c_void>(),
        core::ptr::null_mut::<libc::c_void>(),
    );
    let _ = unsafe { libc::waitpid(child, &mut status, 0) };
    r.finish()
}
