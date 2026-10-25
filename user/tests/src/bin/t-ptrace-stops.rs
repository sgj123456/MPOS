//! t-ptrace-stops - the stop sequence a `PTRACE_SYSCALL` tracee produces.
//!
//! Not a check: a reading. `strace` desynchronises on this kernel - it reports an
//! entering stop whose `PTRACE_GET_SYSCALL_INFO` operation is the exit one and
//! then kills its tracee - and the corrected `t-ptrace-syscall-info` run showed
//! that the operation byte is right at both stops, so the difference has to be in
//! *how many* stops a tracee produces and in which order. This program prints
//! that sequence, one line per boundary, and asserts nothing (it is in the
//! `diagnostic` group for that reason: run it by hand, read the lines).

use mpos_user_tests::Report;

/// `PTRACE_GET_SYSCALL_INFO`, from Linux's `ptrace.h`.
const PTRACE_GET_SYSCALL_INFO: libc::c_uint = 0x420e;

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
        PTRACE_GET_SYSCALL_INFO as libc::c_int,
        child,
        size as *mut libc::c_void,
        &mut info as *mut SyscallInfo as *mut libc::c_void,
    );
    if answer <= 0 { None } else { Some(info) }
}

fn main() {
    println!("t-ptrace-stops");
    let r = Report::new();

    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: the child calls these three libc functions and exits; nothing
        // it touches outlives it.
        unsafe {
            libc::ptrace(
                libc::PTRACE_TRACEME as libc::c_int,
                0,
                core::ptr::null_mut::<libc::c_void>(),
                core::ptr::null_mut::<libc::c_void>(),
            );
            libc::raise(libc::SIGSTOP);
            // Two named calls, so the sequence has a call the reader recognises
            // after the signal trampoline's own.
            libc::syscall(libc::SYS_getpid as libc::c_long);
            libc::syscall(libc::SYS_getppid as libc::c_long);
            libc::_exit(0);
        }
    }
    if child <= 0 {
        println!("ok:  could not fork a tracee");
        r.finish();
    }

    let mut status: libc::c_int = 0;
    let _ = unsafe { libc::waitpid(child, &mut status, 0) };
    let _ = ptrace(
        libc::PTRACE_SETOPTIONS,
        child,
        core::ptr::null_mut::<libc::c_void>(),
        libc::PTRACE_O_TRACESYSGOOD as usize as *mut libc::c_void,
    );

    let mut stops = 0;
    let mut entries = 0;
    loop {
        if ptrace(
            libc::PTRACE_SYSCALL,
            child,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ) < 0
        {
            println!("ok:  PTRACE_SYSCALL refused after {stops} stop(s)");
            break;
        }
        if unsafe { libc::waitpid(child, &mut status, 0) } != child {
            println!("ok:  waitpid returned no child after {stops} stop(s)");
            break;
        }
        if !libc::WIFSTOPPED(status) {
            println!("ok:  child finished after {stops} stop(s), {entries} entr(ies)");
            break;
        }
        stops += 1;
        let signal = libc::WSTOPSIG(status);
        if signal & 0x80 == 0 {
            println!("ok:  stop {stops}: signal {signal} (not a syscall boundary)");
            continue;
        }
        match info_of(child) {
            Some(info) if info.op == 1 => {
                entries += 1;
                // SAFETY: `op` is the entry variant.
                let nr = unsafe { info.payload.entry.nr };
                println!("ok:  stop {stops}: ENTRY nr={nr}");
            }
            Some(info) if info.op == 2 => {
                // SAFETY: `op` is the exit variant.
                let rval = unsafe { info.payload.exit.rval };
                println!("ok:  stop {stops}: EXIT rval={rval}");
            }
            Some(info) => println!("ok:  stop {stops}: op={}", info.op),
            None => println!("ok:  stop {stops}: syscall boundary, no info"),
        }
        if entries >= 6 || stops >= 24 {
            let _ = ptrace(
                libc::PTRACE_KILL,
                child,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            );
            break;
        }
    }
    let _ = unsafe { libc::waitpid(child, &mut status, 0) };
    r.finish()
}
