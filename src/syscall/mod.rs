//! The system-call surface: Linux x86_64 numbers and the dispatch that turns
//! a register block into a call.
//!
//! The register convention is the Linux one: number in `rax`, arguments in
//! `rdi, rsi, rdx, r10, r8, r9`, result in `rax`. `rcx` and `r11` are
//! deliberately absent from the argument list because x86_64's `syscall`
//! instruction destroys them, so a later move from the interrupt gate to
//! that instruction cannot break the convention. Errors come back as
//! negative errno values in `rax`.
//!
//! The dispatch runs the handlers, which need a console and a scheduler
//! under them, so it only exists on the machine. The Linux module's *number
//! table* and its pure *ABI half* (`linux::abi`: flag values, struct
//! layouts, argument shaping) compile on the host too - that is where the
//! man-page contract is unit-tested, and where a change to a flag bit or a
//! struct offset reads as a red test instead of a mystery on the machine.

/// Linux x86_64 syscall number mapping and POSIX handlers.
pub mod linux;

/// Base address for user mmap: where every process's mmap cursor starts.
///
/// The cursor only moves downward from here, so the mmap region grows from
/// the top of the address space toward the program image. That shape -
/// heap low, huge hole, mmap high - is what Linux gives a static program,
/// and musl's allocator notices the difference: it sizes its regions from
/// what the kernel hands back, and a base in the low gigabytes makes those
/// regions collide with the break.
///
/// Defined per-architecture in `crate::mm` (an address-space layout fact, not a
/// dispatch one); re-exported here so the mmap handlers' `crate::syscall::MMAP_BASE`
/// paths keep resolving while `sched` names `crate::mm` directly (the edge that
/// used to run through this layer, `PLAN.md` §9.1).
pub use crate::mm::elf::MMAP_BASE;

/// The errno values [`linux::dispatch`] returns, already negated for `rax`.
///
/// The table itself lives at the crate root (`crate::err`) so that a layer
/// *below* this one can refuse in Linux's language without naming the dispatch
/// table underneath it - the edge every `VfsError` mapper and driver used to
/// reach up for. The re-export keeps this layer's handlers' existing
/// `crate::syscall::err::` paths working; new callers name `crate::err`.
pub use crate::err;

/// How many register arguments a syscall may pass.
pub const ARG_COUNT: usize = 6;

/// The file-creation mask every process starts with: `022`, so a file made with
/// the usual `0666` arrives `0644` and a directory made with `0777` arrives
/// `0755` - which is what init's own `umask 022` leaves behind. It lives in the
/// `abi` leaf (the scheduler seeds a task's umask with it on a path that does not
/// reach dispatch) and is re-exported here so `crate::syscall::DEFAULT_UMASK`
/// stays the name callers already use.
pub use mpos_abi::DEFAULT_UMASK;

/// The last few system calls the kernel entered, for the panic handler.
///
/// A panic names the place the damage was *noticed*, and in a kernel that is
/// routinely a whole system call after the write that caused it: the fault is
/// a collection reading its own bookkeeping wrong, and the write that broke
/// it was in whatever ran before. The one question worth answering first is
/// therefore which call the machine was actually in, and a ring of the last
/// few answers it without guessing. The entries are the number and the first
/// two arguments - enough to tell a `linkat` from a `write`, which is the
/// whole of what it is for - and the cost is a handful of relaxed stores per
/// call, cheap enough to leave on.
///
/// Machine-only, like the dispatch it records: a host build has no console to
/// print the ring to, and nothing that panics for want of it.
#[cfg(target_os = "none")]
pub mod trail {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// How many calls are remembered. Eight is deep enough to see a pattern
    /// (a loop of `openat`/`write`/`close`) and short enough to read off a
    /// serial line in one glance.
    pub const DEPTH: usize = 8;

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static NR: [AtomicUsize; DEPTH] = [const { AtomicUsize::new(0) }; DEPTH];
    static A0: [AtomicUsize; DEPTH] = [const { AtomicUsize::new(0) }; DEPTH];
    static A1: [AtomicUsize; DEPTH] = [const { AtomicUsize::new(0) }; DEPTH];

    /// Record one call, at dispatch entry - before anything can go wrong.
    pub fn note(nr: u64, args: [usize; super::ARG_COUNT]) {
        let at = NEXT.fetch_add(1, Ordering::Relaxed) % DEPTH;
        NR[at].store(nr as usize, Ordering::Relaxed);
        A0[at].store(args[0], Ordering::Relaxed);
        A1[at].store(args[1], Ordering::Relaxed);
    }

    /// Print the ring, oldest first.
    pub fn dump() {
        let next = NEXT.load(Ordering::Relaxed);
        let count = next.min(DEPTH);
        crate::println!("  last {count} syscalls, oldest first (number, arg0, arg1):");
        for i in 0..count {
            let at = (next + DEPTH - count + i) % DEPTH;
            crate::println!(
                "    nr {:3} {:#x} {:#x}",
                NR[at].load(Ordering::Relaxed),
                A0[at].load(Ordering::Relaxed),
                A1[at].load(Ordering::Relaxed),
            );
        }
    }
}

/// The userland-boundary primitives live at the crate root (`crate::uaccess`)
/// so that a layer *below* `syscall` can validate a caller's pointer and turn a
/// `VfsError` into an errno without naming this module - the V-9 inversion
/// `ARCHITECTURE.md` §11.1.5 records. The re-exports keep the paths this layer's
/// handlers already use; new callers outside `syscall` name `crate::uaccess`.
/// `MAX_IO` and `err_of` are reached only by machine-compiled handlers, so their
/// re-exports are gated to match; `buffer_ok` is reached on the host too.
pub(crate) use crate::uaccess::buffer_ok;
#[cfg(target_os = "none")]
pub(crate) use crate::uaccess::{MAX_IO, err_of};

/// Maximum length of a path argument.
pub(crate) const MAX_PATH: usize = 255;

/// Parse a NUL-terminated string argument - the Linux ABI's path shape - into
/// an owned String.
///
/// The string's own NUL is the length word, so the kernel cannot know up
/// front how far the caller means it to run. The scan therefore validates one
/// page at a time and stops at the first terminator, which keeps a short
/// string that ends inside its last mapped page from being rejected for the
/// unmapped bytes beyond it. A string with no NUL within [`MAX_PATH`]
/// characters answers `ENAMETOOLONG`, the errno Linux uses for exactly that.
pub(crate) fn cstr_arg(ptr: usize, from_user: bool) -> Result<alloc::string::String, isize> {
    cstr_limit(ptr, from_user, MAX_PATH)
}

/// [`cstr_arg`] for the strings of an `execve`'s `envp` and `argv` arrays,
/// whose limit is not a path's. Linux caps a *pathname* at 255 bytes per
/// component, but an environment string is only bounded by the argument
/// block's total - `COLLECT_GCC_OPTIONS` and friends from a real compiler
/// run past 255 bytes easily - and refusing one is what makes `gcc` fail
/// with `posix_spawn: Filename too long` before it ever runs a subprocess.
pub(crate) const MAX_ARG_STR: usize = crate::sched::ARGS_BYTES_MAX;

fn cstr_limit(ptr: usize, from_user: bool, limit: usize) -> Result<alloc::string::String, isize> {
    if ptr == 0 {
        return Err(err::EFAULT);
    }
    let mut bytes = alloc::vec::Vec::new();
    let mut p = ptr;
    loop {
        // A page-table entry covers a whole page, so validating the first
        // byte of one validates every byte of it.
        if (p & 0xfff == 0 || p == ptr) && !buffer_ok(p, 1, false, from_user) {
            return Err(err::EFAULT);
        }
        let b = unsafe { (p as *const u8).read_unaligned() };
        if b == 0 {
            break;
        }
        bytes.push(b);
        if bytes.len() > limit {
            return Err(err::ENAMETOOLONG);
        }
        p += 1;
    }
    core::str::from_utf8(&bytes)
        .map(alloc::string::String::from)
        .map_err(|_| err::EINVAL)
}

/// Parse a NULL-terminated array of NUL-terminated strings - the Linux ABI's
/// argv shape - into an owned vector. An absent array (`NULL`) is empty.
/// `limit` is the per-string ceiling: [`MAX_PATH`] for the `argv` a caller
/// spells as paths, [`MAX_ARG_STR`] for the environment.
pub(crate) fn argv_arg(
    ptr: usize,
    limit: usize,
) -> Result<alloc::vec::Vec<alloc::string::String>, isize> {
    if ptr == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    let mut out = alloc::vec::Vec::new();
    let mut p = ptr;
    loop {
        if !buffer_ok(p, core::mem::size_of::<usize>(), false, true) {
            return Err(err::EFAULT);
        }
        let entry = unsafe { (p as *const usize).read_unaligned() };
        if entry == 0 {
            break;
        }
        if out.len() >= crate::sched::ARGV_MAX {
            return Err(err::E2BIG);
        }
        out.push(cstr_limit(entry, true, limit)?);
        p += core::mem::size_of::<usize>();
    }
    Ok(out)
}

/// Dispatch a Linux x86_64 syscall number - the numbers a musl binary
/// speaks, in `src/syscall/linux/nr.rs`.
///
/// `from_user` is `true` when the dispatch was reached through the
/// `syscall` gate from ring 3 (the normal case) and `false` when the
/// kernel is calling a handler on its own behalf: the gate validates
/// user pointers, and the kernel's own buffers do not need that walk.
///
/// `regs_ptr` is the raw pointer to the register frame on the kernel
/// stack, used by execve to patch the return address and stack pointer
/// so the process enters the new image on `sysretq`, and read through
/// `hal::user_ip` to build a seccomp filter's `seccomp_data`. It is null
/// for a call the kernel makes on its own behalf, which has no frame.
///
/// # Safety
/// `regs_ptr` must be null or the syscall gate's own frame for the calling
/// task. Nothing checks that: the frame lives on that task's kernel stack, and
/// several handlers dereference it. The only callers that pass a frame are the
/// two machine gates, which have just built it.
#[cfg(target_os = "none")]
pub unsafe fn linux_dispatch(
    nr: u64,
    args: [usize; ARG_COUNT],
    from_user: bool,
    regs_ptr: *mut (),
) -> isize {
    trail::note(nr, args);
    // Fire an interval timer that came due while the task was in ring 3. The
    // clock covers a spinning task and the scheduler covers an idle core;
    // this covers the third state - a task in and out of syscalls - so no
    // caller waits a whole slice past its deadline.
    crate::sched::check_itimer();
    // The two moments a tracer wants to see: the call before it is made and its
    // result before it is returned. Both are ordinary stops in the task table,
    // and both are no-ops - one branch on a flag - for every task no tracer has
    // touched, which is every task on a machine whose `ptrace` handler has not
    // been ported: with no tracer there is no stop to take. The operations are
    // the machine's (`hal::Gate`), so the pair runs on both machines either way.
    let args = linux::proc::syscall_entry_stop(regs_ptr, nr, args);
    // SAFETY: the caller's contract, passed through unchanged.
    let ret = unsafe { linux::dispatch(nr, args, from_user, regs_ptr) };
    let ret = linux::proc::syscall_exit_stop(regs_ptr, nr, args, ret);
    // Signals are delivered here rather than before the call: a handler has to
    // see the *result* of the syscall it interrupted, and the result does not
    // exist until the call has returned. Jumping into a handler means
    // rewriting the frame the task will `sysretq` through, which is why this
    // is also the only delivery point that can run one that needs a frame -
    // and the only one that can rewind an interrupted call, because the frame
    // it rewrites is the record of the call's own arguments.
    //
    // Deferred off aarch64 with the signal stage: delivering a handler rewrote
    // the x86 frame, and the aarch64 `sigcontext`/`rt_sigreturn` builder is a
    // named gap. A first `/init` that only runs and exits needs no handler, so
    // on this machine the call's own value is simply returned.
    #[cfg(not(target_arch = "aarch64"))]
    if linux::deliver_signal(regs_ptr, nr, ret) {
        // The interrupted call's value belongs to the handler now: it is on
        // the frame, waiting for `rt_sigreturn` to put it back.
        return 0;
    }
    ret
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::err;

    #[test]
    fn errors_match_the_errno_values_they_name() {
        assert_eq!(err::ENOENT, -2);
        assert_eq!(err::ESRCH, -3);
        assert_eq!(err::ENOEXEC, -8);
        assert_eq!(err::EBADF, -9);
        assert_eq!(err::ECHILD, -10);
        assert_eq!(err::EAGAIN, -11);
        assert_eq!(err::EFAULT, -14);
        assert_eq!(err::EBUSY, -16);
        assert_eq!(err::EEXIST, -17);
        assert_eq!(err::ENOTDIR, -20);
        assert_eq!(err::EISDIR, -21);
        assert_eq!(err::EINVAL, -22);
        assert_eq!(err::EFBIG, -27);
        assert_eq!(err::ENOSPC, -28);
        assert_eq!(err::EPIPE, -32);
        assert_eq!(err::ENOSYS, -38);
    }
}
