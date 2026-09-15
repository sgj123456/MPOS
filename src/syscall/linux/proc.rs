//! Linux-compatible syscall handlers. Memory: `mmap`, `munmap`, `mremap`,
//! `mprotect`, `brk`. Time: `nanosleep`, `clock_nanosleep`, `clock_gettime`,
//! `gettimeofday`. Identity: `getpid`, `gettid`, `getppid`, `kill`/`tkill`.
//! Process: `execve`, `exit`/`exit_group`, `wait4`, `fork` (clone), `uname`,
//! `getrandom`, `prlimit64`, `futex`, `sched_getaffinity`, `set_robust_list`,
//! `rseq`.

use alloc::string::String;
use alloc::sync::Arc;

use super::nr;
use crate::sched::TaskId;
use crate::syscall::{ARG_COUNT, argv_arg, buffer_ok, cstr_arg, err, err_of};

pub(super) use crate::syscall::MMAP_BASE;

/// The register frame on the kernel stack, matching the layout pushed by the
/// `syscall` entry stub in `arch/x86_64/syscall.rs`. Fields are named after
/// the user registers they hold; offsets must match the push order exactly.
/// [`handle_execve`] patches the return address and stack pointer through it,
/// and [`handle_fork`] reads the whole thing to rebuild the child's register
/// file.
#[repr(C)]
struct RegsFrame {
    rax: usize,      // [rsp+0]   return value / syscall number
    r11: usize,      // [rsp+8]   user RFLAGS (sysretq restores from here)
    rcx: usize,      // [rsp+16]  user RIP (sysretq restores from here)
    r9: usize,       // [rsp+24]
    r8: usize,       // [rsp+32]
    r10: usize,      // [rsp+40]
    rdx: usize,      // [rsp+48]
    rsi: usize,      // [rsp+56]
    rdi: usize,      // [rsp+64]
    user_rsp: usize, // [rsp+72]  user RSP (restored from here at sysretq)
    r12: usize,      // [rsp+80]
    r15: usize,      // [rsp+88]
    r14: usize,      // [rsp+96]
    r13: usize,      // [rsp+104]
    rbx: usize,      // [rsp+112]
    rbp: usize,      // [rsp+120]
}

// ---- signals: the frame a handler runs on -------------------------------
//
// A handler cannot be entered by a `jmp`: it has to be able to `ret` back into
// the kernel, and the kernel has to be able to restore a register file that no
// longer exists by then. Both halves of that are a structure written on the
// process's own stack and read back by `rt_sigreturn`, in a layout the libc's
// trampoline knows by heart. Everything below is that layout.

/// Byte offsets of the fields of `struct sigcontext`, in the Linux x86_64
/// order. They are not a choice: `__restore_rt` hands the structure back to
/// `rt_sigreturn`, and this kernel reads the saved registers from exactly
/// where the libc expects them to be.
mod sc {
    pub const R8: usize = 0;
    pub const R9: usize = 8;
    pub const R10: usize = 16;
    pub const R12: usize = 32;
    pub const R13: usize = 40;
    pub const R14: usize = 48;
    pub const R15: usize = 56;
    pub const RDI: usize = 64;
    pub const RSI: usize = 72;
    pub const RBP: usize = 80;
    pub const RBX: usize = 88;
    pub const RDX: usize = 96;
    pub const RAX: usize = 104;
    pub const RSP: usize = 120;
    pub const RIP: usize = 128;
    pub const EFLAGS: usize = 136;
    /// The structure is padded out to here, whatever this kernel fills in.
    pub const BYTES: usize = 256;
}

/// `SI_USER`: the signal came from `kill`, not from the hardware. It is what a
/// handler reads out of `si_code` to tell a deliberate signal from a fault.
const SI_USER: i32 = 0;

fn put_word(bytes: &mut [u8], at: usize, value: usize) {
    bytes[at..at + 8].copy_from_slice(&(value as u64).to_le_bytes());
}

fn get_word(bytes: &[u8], at: usize) -> usize {
    usize::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Write the frame a handler will run on and point the task at it.
///
/// Returns `false` when the frame cannot be written - the stack is not mapped,
/// or not writable - which leaves the caller to take the default action rather
/// than to lose the signal.
fn build_frame(
    regs_ptr: *mut (),
    sig: u8,
    action: &crate::sched::signal::Action,
    ret: isize,
) -> bool {
    use crate::sched::signal::{
        SA_ONSTACK, SA_RESETHAND, SA_RESTORER, SIGFRAME_BYTES, SIGFRAME_SIGINFO, SIGFRAME_UCONTEXT,
        UCONTEXT_MCONTEXT, UCONTEXT_SIGMASK,
    };

    let regs = unsafe { &mut *(regs_ptr as *mut RegsFrame) };

    // A handler runs on the process's own stack, unless the disposition asked
    // for the alternate one. Either way the frame goes just below where the
    // stack pointer was: the interrupted code gets its memory back untouched
    // when the handler returns.
    let mut sp = regs.user_rsp;
    let alt = crate::sched::signal_alt_stack(crate::sched::current());
    if action.flags & SA_ONSTACK != 0
        && let Some(stack) = alt
    {
        sp = stack.sp + stack.size;
    }

    // A function is entered with its return address at `rsp % 16 == 8`; a
    // handler is entered the same way, with the restorer standing in for the
    // `call` that would have put it there.
    let frame = ((sp - SIGFRAME_BYTES) & !15) - 8;

    let mut bytes = [0u8; SIGFRAME_BYTES];
    // The restorer - where the handler's `ret` goes. Without it the handler
    // would return into whatever it found on the stack, and the process could
    // never leave the handler at all.
    if action.flags & SA_RESTORER == 0 || action.restorer == 0 {
        // The ABI makes this field mandatory on x86_64; a disposition without
        // one cannot be run.
        return false;
    }
    put_word(&mut bytes, 0, action.restorer);

    // The saved register file. `rax` is the value the interrupted syscall
    // returned, which is what `rt_sigreturn` puts back.
    let mcontext = SIGFRAME_UCONTEXT + UCONTEXT_MCONTEXT;
    put_word(&mut bytes, mcontext + sc::R8, regs.r8);
    put_word(&mut bytes, mcontext + sc::R9, regs.r9);
    put_word(&mut bytes, mcontext + sc::R10, regs.r10);
    put_word(&mut bytes, mcontext + sc::R12, regs.r12);
    put_word(&mut bytes, mcontext + sc::R13, regs.r13);
    put_word(&mut bytes, mcontext + sc::R14, regs.r14);
    put_word(&mut bytes, mcontext + sc::R15, regs.r15);
    put_word(&mut bytes, mcontext + sc::RDI, regs.rdi);
    put_word(&mut bytes, mcontext + sc::RSI, regs.rsi);
    put_word(&mut bytes, mcontext + sc::RBP, regs.rbp);
    put_word(&mut bytes, mcontext + sc::RBX, regs.rbx);
    put_word(&mut bytes, mcontext + sc::RDX, regs.rdx);
    put_word(&mut bytes, mcontext + sc::RAX, ret as usize);
    // rcx and r11 were destroyed by the `syscall` that is being interrupted:
    // the CPU puts the return address in rcx and the flags in r11 on the way
    // in, so there is nothing of the caller's left to save.
    put_word(&mut bytes, mcontext + sc::RSP, regs.user_rsp);
    put_word(&mut bytes, mcontext + sc::RIP, regs.rcx);
    put_word(&mut bytes, mcontext + sc::EFLAGS, regs.r11);

    // `uc_stack` describes the alternate stack, as the handler will see it
    // through `sigaltstack(NULL, ...)`.
    let stack_at = SIGFRAME_UCONTEXT + 16;
    if let Some(stack) = alt {
        put_word(&mut bytes, stack_at, stack.sp);
        put_word(&mut bytes, stack_at + 16, stack.size);
    }

    // The mask to put back on the way out - what the process had blocked when
    // the signal arrived.
    let blocked = crate::sched::current_blocked();
    put_word(
        &mut bytes,
        SIGFRAME_UCONTEXT + UCONTEXT_SIGMASK,
        blocked as usize,
    );

    // A `siginfo_t`: signo, errno, code. Everything else a `kill` has to say
    // is in the first three fields.
    bytes[SIGFRAME_SIGINFO..SIGFRAME_SIGINFO + 4].copy_from_slice(&(sig as i32).to_le_bytes());
    bytes[SIGFRAME_SIGINFO + 8..SIGFRAME_SIGINFO + 12].copy_from_slice(&SI_USER.to_le_bytes());

    if !crate::sched::write_user(frame, &bytes) {
        return false;
    }

    // Enter the handler with the three arguments the ABI gives it: the signal
    // number, a `siginfo_t`, and the `ucontext` to hand back.
    regs.rcx = action.handler;
    regs.user_rsp = frame;
    regs.rdi = sig as usize;
    regs.rsi = frame + SIGFRAME_SIGINFO;
    regs.rdx = frame + SIGFRAME_UCONTEXT;

    crate::sched::set_current_blocked(action.handler_mask(sig, blocked));
    if action.flags & SA_RESETHAND != 0 {
        crate::sched::reset_disposition(sig);
    }
    true
}

/// Run a pending signal's handler, or take its default action.
///
/// Called on the way out of every syscall, holding the register frame the task
/// will `sysretq` through. That frame is why this is the place: entering a
/// handler means rewriting the task's whole return context, and a syscall is
/// the only moment the kernel has it in a form it can both read and write.
/// The clock can deliver a signal with no handler, but not one with a handler,
/// for exactly this reason.
///
/// Returns `true` when a handler was entered, so the caller can report a
/// reassuring zero to the interrupted call instead of its own return value -
/// which is now the handler's to see when it returns.
pub(super) fn deliver_signal(regs_ptr: *mut (), ret: isize) -> bool {
    if regs_ptr.is_null() {
        return false;
    }
    let Some(sig) = crate::sched::take_deliverable_signal() else {
        return false;
    };
    let action = crate::sched::current_disposition(sig);
    if action.is_ignore() {
        return false;
    }
    if action.is_default() || !build_frame(regs_ptr, sig, &action, ret) {
        // Nothing to run, or nowhere to put the frame - and the default action
        // for every signal this kernel names is to die of it, with the number
        // in the exit code so the parent can tell which one it was.
        crate::sched::exit(-(sig as isize));
    }
    true
}

/// `rt_sigreturn` — the trampoline's call, and the only way out of a handler.
///
/// The libc's `__restore_rt` is entered by the handler's `ret`, so `rsp` has
/// already moved past the restorer word; the frame therefore starts eight
/// bytes below where this call finds the stack pointer.
pub(super) fn handle_rt_sigreturn(regs_ptr: *mut ()) -> isize {
    use crate::sched::signal::{SIGFRAME_UCONTEXT, UCONTEXT_MCONTEXT, UCONTEXT_SIGMASK};

    if regs_ptr.is_null() {
        return err::EFAULT;
    }
    let regs = unsafe { &mut *(regs_ptr as *mut RegsFrame) };
    let frame = regs.user_rsp.wrapping_sub(8);
    let mcontext = frame + SIGFRAME_UCONTEXT + UCONTEXT_MCONTEXT;

    let mut bytes = [0u8; sc::BYTES];
    if !crate::sched::read_user(mcontext, &mut bytes) {
        return err::EFAULT;
    }

    regs.r8 = get_word(&bytes, sc::R8);
    regs.r9 = get_word(&bytes, sc::R9);
    regs.r10 = get_word(&bytes, sc::R10);
    regs.r12 = get_word(&bytes, sc::R12);
    regs.r13 = get_word(&bytes, sc::R13);
    regs.r14 = get_word(&bytes, sc::R14);
    regs.r15 = get_word(&bytes, sc::R15);
    regs.rdi = get_word(&bytes, sc::RDI);
    regs.rsi = get_word(&bytes, sc::RSI);
    regs.rbp = get_word(&bytes, sc::RBP);
    regs.rbx = get_word(&bytes, sc::RBX);
    regs.rdx = get_word(&bytes, sc::RDX);
    // `sysretq` takes the return address from rcx and the flags from r11, and
    // no saved value can go there: whatever the process had in them before the
    // `syscall` was destroyed by the instruction itself.
    regs.rcx = get_word(&bytes, sc::RIP);
    regs.user_rsp = get_word(&bytes, sc::RSP);
    // Interrupts have to be on when the process resumes; everything else about
    // the flags is the process's own business.
    regs.r11 = get_word(&bytes, sc::EFLAGS) | 0x200;

    let mut mask = [0u8; 8];
    if crate::sched::read_user(frame + SIGFRAME_UCONTEXT + UCONTEXT_SIGMASK, &mut mask) {
        crate::sched::set_current_blocked(u32::from_le_bytes(mask[..4].try_into().unwrap()));
    }

    get_word(&bytes, sc::RAX) as isize
}

/// Terminate the current process.
pub(super) fn handle_exit(args: [usize; ARG_COUNT]) -> isize {
    crate::sched::exit(args[0] as isize);
}

/// Yield the CPU to the scheduler.
pub(super) fn handle_yield() -> isize {
    crate::sched::yield_now();
    0
}

/// Get the current process ID.
pub(super) fn handle_getpid() -> isize {
    crate::sched::current().0 as isize
}

/// `wait4(pid, status, options)` — collect a child's exit.
///
/// Linux's contract, which this follows: the *return value* is the pid of the
/// child that was collected (or a negative errno), and the exit is described by
/// the word written through `status`. That split is what lets a shell tell "a
/// child exited with status 3" from "no child exited yet".
///
/// `status` is encoded the way every libc's `WIFEXITED`/`WEXITSTATUS` read it: a
/// normal exit puts its low byte in bits 15:8, and a death by signal puts the
/// signal number in bits 6:0. A `status` of 0 is legal and means "not
/// interested", which is what init passes when it only wants children reaped.
///
/// Which child is named follows the man page:
/// * `pid > 0` - that child;
/// * `pid == 0` - any child in the caller's process group; mpos has no process
///   groups, so this widens to "any child", as does `pid == -1`;
/// * any other negative pid - the same, since the group it names does not exist.
///
/// `WNOHANG` is honoured as Linux defines it: a child that has not exited yet is
/// answered with 0, not an error, and only a caller with no child at all gets
/// `ECHILD`.
pub(super) fn handle_wait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const WNOHANG: usize = 1;
    let (pid, status, options) = (args[0] as isize, args[1], args[2]);
    let me = crate::sched::current();
    let no_hang = options & WNOHANG != 0;

    // Every child: the pid == -1 form, and every other group form this kernel
    // cannot tell apart for lack of process groups.
    if pid <= 0 {
        let collected = if no_hang {
            crate::sched::poll_any_child(me)
        } else {
            crate::sched::wait_any_child(me)
        };
        return match collected {
            Some((id, code)) => finish_wait(id, code, status, from_user),
            // Nothing to report: an error only when there is nobody left to
            // wait for, and otherwise the "none exited yet" zero.
            None if no_hang && crate::sched::has_children(me) => 0,
            None => err::ECHILD,
        };
    }

    // A named child. The bound keeps a wild pid from indexing the table.
    if pid as usize >= crate::sched::MAX_TASKS {
        return err::ECHILD;
    }
    let id = TaskId(pid as usize);

    // A code already on file means the child is a zombie: it is collected now,
    // blocking or not.
    if let Some(code) = crate::sched::take_exit_code(id) {
        return finish_wait(id, code, status, from_user);
    }
    if !crate::sched::is_child(id, me) {
        return err::ECHILD;
    }
    if no_hang {
        return 0;
    }

    crate::sched::block_until_code(id);
    match crate::sched::take_exit_code(id) {
        Some(code) => finish_wait(id, code, status, from_user),
        None => err::ECHILD,
    }
}

/// The two outputs of a successful `wait`: the child's pid as the return value,
/// and the exit encoded into `*status`. A null `status` is "not interested" and
/// writes nothing, which is what the man page says.
fn finish_wait(id: TaskId, code: isize, status: usize, from_user: bool) -> isize {
    if status != 0 {
        if !buffer_ok(status, 4, true, from_user) {
            return err::EFAULT;
        }
        unsafe { (status as *mut i32).write_volatile(wait_status(code)) };
    }
    id.0 as isize
}

/// Encode an exit the way `WIFEXITED` and friends decode it.
///
/// mpos records a death by signal as the negated signal number, because that is
/// the shape the rest of the kernel wants; the word Linux reports is different,
/// so the sign is what carries the distinction here.
fn wait_status(code: isize) -> i32 {
    if code < 0 {
        // Killed by a signal: its number fills the low byte. The core-dump bit
        // stays clear - mpos writes no core files.
        (-code & 0x7f) as i32
    } else {
        // A normal exit: the low byte of the code, in bits 15:8.
        ((code & 0xff) << 8) as i32
    }
}

/// Get the parent process ID.
///
/// The task table records who spawned whom in ring 3, so the answer is real:
/// the spawner's id, or 0 for init and the tasks nobody spawned - which is
/// also what Linux reports for init.
pub(super) fn handle_getppid() -> isize {
    crate::sched::parent_of(crate::sched::current()).map_or(0, |parent| parent.0 as isize)
}

/// `execve(path, argv, envp)` — replace the current process image.
///
/// Parses the ELF, tears down the old address space, loads the new image,
/// and patches the syscall return frame so the process enters the new image
/// on `sysretq`. This is a real execve: it replaces the caller and never
/// returns.
pub(super) fn handle_execve(
    args: [usize; ARG_COUNT],
    cwd: &Arc<dyn crate::vfs::Inode>,
    regs_ptr: *mut (),
) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let mut argv = match argv_arg(args[1]) {
        Ok(argv) => argv,
        Err(e) => return e,
    };
    if argv.is_empty() {
        let name = path.rsplit('/').next().unwrap_or(&path);
        argv.push(String::from(name));
    }
    let envp = match argv_arg(args[2]) {
        Ok(envp) => envp,
        Err(e) => return e,
    };
    let bytes = match crate::vfs::read_file_opt(&path, Some(cwd.clone())) {
        Ok(bytes) => bytes,
        Err(e) => return err_of(e),
    };

    match crate::sched::exec_replace(&bytes, &argv, &envp, &path) {
        Ok((entry, new_rsp)) => {
            // Patch the syscall return frame: when sysretq fires, RIP = entry
            // and RSP = new_rsp, so the process enters the new image.
            //
            // The Regs layout (from syscall.rs):
            //   [rsp+0]  rax (return value)
            //   [rsp+8]  r11 (user RFLAGS)
            //   [rsp+16] rcx (user RIP)  ← patch this
            //   ...args...
            //   [rsp+72] user_rsp         ← patch this
            //
            // sysretq takes RIP from rcx and RFLAGS from r11.
            if !regs_ptr.is_null() {
                let regs = unsafe { &mut *(regs_ptr as *mut RegsFrame) };
                regs.rcx = entry; // sysretq will jump here
                regs.user_rsp = new_rsp; // new stack pointer
            }
            0 // execve succeeds; the frame patches make it "not return"
        }
        Err(crate::sched::SpawnError::NotExecutable) => err::ENOEXEC,
        Err(crate::sched::SpawnError::ArgsTooBig) => err::E2BIG,
        Err(crate::sched::SpawnError::BadFd) => err::EBADF,
        // Linux reports a missing or unloadable interpreter as "the file was
        // not found", not as a malformed executable: the program itself was
        // fine, and it is the path it names that could not be followed.
        Err(crate::sched::SpawnError::NoInterpreter) => err::ENOENT,
    }
}

/// `nanosleep(req, rem)` — req is a `struct timespec { tv_sec, tv_nsec }`.
///
/// The request is validated the way Linux validates it: the caller's `req`
/// must be readable and `tv_nsec` must stay below a billion. `rem` is never
/// written — nothing interrupts the sleep, and Linux writes the remaining
/// time only when it does.
pub(super) fn handle_nanosleep(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let req = args[0];
    if req == 0 {
        return 0;
    }
    sleep_for_timespec(req, from_user)
}

/// `clock_nanosleep(clk_id, flags, req, rem)`.
///
/// The relative form (flags = 0) sleeps exactly as `nanosleep` does, whatever
/// clock it names: mpos keeps one clock. `TIMER_ABSTIME` has no absolute time
/// to be measured against and answers `EINVAL` rather than sleeping the wrong
/// duration.
pub(super) fn handle_clock_nanosleep(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const TIMER_ABSTIME: usize = 0x1;
    let (_clk_id, flags, req) = (args[0], args[1], args[2]);
    if flags & TIMER_ABSTIME != 0 {
        return err::EINVAL;
    }
    if req == 0 {
        return 0;
    }
    sleep_for_timespec(req, from_user)
}

/// Sleep out the `struct timespec` at `req`, with Linux's validation: the
/// pointer must be readable and `tv_nsec` must stay below a second.
fn sleep_for_timespec(req: usize, from_user: bool) -> isize {
    if !buffer_ok(req, 16, false, from_user) {
        return err::EFAULT;
    }
    unsafe {
        let tv_sec = (req as *const i64).read_volatile() as u64;
        let tv_nsec = ((req + 8) as *const i64).read_volatile() as u64;
        if tv_nsec >= 1_000_000_000 {
            return err::EINVAL;
        }
        let total_ms = tv_sec.saturating_mul(1000) + tv_nsec / 1_000_000;
        if total_ms == 0 {
            return 0;
        }
        let ticks = (total_ms * crate::arch::timer::TICK_HZ as u64 / 1000).max(1);
        crate::sched::sleep_ticks(ticks);
    }
    0
}

/// `fork()` — duplicate the calling process.
///
/// The child returns 0 from this call and the parent gets the child's pid; both
/// carry on from the instruction after the `syscall`. `regs_ptr` is the gate's
/// register frame, and it is the whole reason this handler needs it: the child
/// must resume with exactly the caller's registers, so the kernel has to
/// reconstruct that file and hand it to the new task.
///
/// A null frame means the call did not come through the `syscall` gate (the
/// kernel's own in-process tests, for instance), and there is no register file
/// to resume from; that is an honest `EINVAL` rather than a half-made child.
///
/// `vfork` reaches this same handler and is answered as a `fork`. That is a
/// divergence in one visible respect only: Linux suspends the calling process
/// until the child execs or exits, and this does not. Everything else holds - the
/// child is a real process with its own image, and the `fork`-then-`execve` pair
/// every `vfork` user writes behaves identically. Suspending the parent is a
/// scheduling feature for an optimisation this kernel does not make (there is no
/// page-table sharing to avoid), so there is nothing to gain by faking it.
pub(super) fn handle_fork(_args: [usize; ARG_COUNT], regs_ptr: *mut ()) -> isize {
    let ctx = match user_context(regs_ptr, 0) {
        Some(ctx) => ctx,
        None => return err::EINVAL,
    };
    crate::sched::fork(&ctx)
}

/// Build the ring-3 register file a process would resume with, from the syscall
/// gate's frame, with `rax` set to `rax` - zero for a fork child, and the new
/// image's entry value for an `execve` that has nothing else to restore.
///
/// Returns `None` when there is no frame: a call made outside the gate has no
/// registers to name.
fn user_context(regs_ptr: *mut (), rax: usize) -> Option<crate::arch::context::UserContext> {
    if regs_ptr.is_null() {
        return None;
    }
    let regs = unsafe { &*(regs_ptr as *const RegsFrame) };
    let (code, data) = crate::arch::gdt::user_selectors();
    Some(crate::arch::context::UserContext {
        // `syscall` parked the return address in rcx and the caller's flags in
        // r11; those two are what an `iretq` needs to reproduce the state the
        // instruction left behind.
        rip: regs.rcx,
        rsp: regs.user_rsp,
        rflags: regs.r11,
        rax,
        rbx: regs.rbx,
        rbp: regs.rbp,
        cs: code.0 as usize,
        ss: data.0 as usize,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        rdi: regs.rdi,
        rsi: regs.rsi,
        rdx: regs.rdx,
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
    })
}

/// `clone(flags, child_stack, parent_tid, child_tid, tls)` — create a process or
/// a thread.
///
/// On x86_64 the arguments arrive as: `rdi` = flags, `rsi` = the child's stack,
/// `rdx` = where the parent wants the child's tid written, `r10` = where the
/// child wants its own tid written, `r8` = the child's TLS descriptor.
///
/// The call is `fork` with knobs: `fork` is `clone` with a zero flag word, and
/// each set bit names a part of the caller the child should *share* instead of
/// copying. The flag word is checked the way Linux checks it - an unknown bit,
/// or a combination whose pieces need each other (`CLONE_THREAD` implies
/// `CLONE_SIGHAND`, which implies `CLONE_VM`), is `EINVAL` before anything is
/// created.
///
/// A process-shaped request - anything without `CLONE_THREAD` - is built here in
/// full: the child returns from the call with 0 in `rax` on the stack the caller
/// chose, with the tid writes and the TLS the flags name. `CLONE_THREAD` asks for
/// a thread, and the scheduler refuses that one with `ENOSYS`: a shared address
/// space needs reference counting this kernel's owned-by-one-task address spaces
/// do not have, and a child that only *looks* like a thread would be worse than
/// an honest refusal. `CLONE_VM` without `CLONE_THREAD` is the spawn shape and is
/// honoured - see [`crate::sched::clone`].
#[allow(clippy::too_many_lines)]
pub(super) fn handle_clone(args: [usize; ARG_COUNT], regs_ptr: *mut ()) -> isize {
    let (flags, child_stack, parent_tid, child_tid, tls) =
        (args[0], args[1], args[2], args[3], args[4]);

    // The low byte of the flag word is not a flag: it is the *signal* the
    // parent should be sent when the child exits, which is why a word like
    // `CLONE_VM|CLONE_VFORK|SIGCHLD` - exactly what a libc's `posix_spawn`
    // passes - has a plain signal number mixed into it. Linux masks the byte off
    // before testing the rest, and so must this: reading `SIGCHLD` (0x11) as an
    // unknown flag bit would refuse every `posix_spawn` in the system.
    const CSIGNAL: usize = 0x0000_00ff;
    let exit_signal = flags & CSIGNAL;
    let flags = flags & !CSIGNAL;

    // The bits this kernel understands. Anything else is a request from a
    // kernel newer than this one, and honouring it half-way would be worse than
    // refusing it.
    const KNOWN: usize = nr::CLONE_VM
        | nr::CLONE_FS
        | nr::CLONE_FILES
        | nr::CLONE_SIGHAND
        | nr::CLONE_VFORK
        | nr::CLONE_PARENT
        | nr::CLONE_THREAD
        | nr::CLONE_SYSVSEM
        | nr::CLONE_SETTLS
        | nr::CLONE_PARENT_SETTID
        | nr::CLONE_CHILD_CLEARTID
        | nr::CLONE_CHILD_SETTID;
    if flags & !KNOWN != 0 {
        return err::EINVAL;
    }
    // A signal number that names nothing is not a request to deliver anything;
    // Linux refuses it, and so does this - silently accepting it would hide a
    // caller's mistake behind a working call.
    if exit_signal > 31 {
        return err::EINVAL;
    }
    // A fork child is announced by its parent's `wait`, not by a signal, so the
    // exit signal is accepted and not acted on. `wait4` is how every caller in
    // this system collects a child; the byte is kept in the ABI rather than
    // honoured because there is no delivery path a shell could not get from
    // waiting anyway.
    let _ = exit_signal;
    // The dependency chain Linux enforces: a thread shares its creator's signal
    // handlers and address space, and a signal-handler table can only be shared
    // by processes that also share memory.
    if flags & nr::CLONE_THREAD != 0
        && (flags & nr::CLONE_SIGHAND == 0 || flags & nr::CLONE_VM == 0)
    {
        return err::EINVAL;
    }
    if flags & nr::CLONE_SIGHAND != 0 && flags & nr::CLONE_VM == 0 {
        return err::EINVAL;
    }
    // A tid must land somewhere; a flag that asks for the write without a place
    // to put it is refused rather than ignored.
    if flags & nr::CLONE_PARENT_SETTID != 0 && parent_tid == 0 {
        return err::EINVAL;
    }
    if flags & (nr::CLONE_CHILD_SETTID | nr::CLONE_CHILD_CLEARTID) != 0 && child_tid == 0 {
        return err::EINVAL;
    }
    // Both tid pointers, when present, must be writable in the caller.
    if flags & nr::CLONE_PARENT_SETTID != 0 && !buffer_ok(parent_tid, 4, true, true) {
        return err::EFAULT;
    }
    if flags & (nr::CLONE_CHILD_SETTID | nr::CLONE_CHILD_CLEARTID) != 0
        && !buffer_ok(child_tid, 4, true, true)
    {
        return err::EFAULT;
    }

    let ctx = match user_context(regs_ptr, 0) {
        Some(ctx) => ctx,
        None => return err::EINVAL,
    };

    // A thread's stack is its own, so `child_stack` picks where it starts; a
    // process ignores the argument and returns from the call on its own stack,
    // exactly as `fork` does.
    let request = crate::sched::CloneRequest {
        flags,
        child_stack,
        child_tid,
        tls,
    };
    let child = crate::sched::clone(&ctx, request);
    if child < 0 {
        return child;
    }

    // CLONE_PARENT_SETTID: the child's tid goes into the *parent's* memory, so
    // the write happens here, before the child has run.
    if flags & nr::CLONE_PARENT_SETTID != 0 {
        unsafe { (parent_tid as *mut i32).write_volatile(child as i32) };
    }
    child
}

/// Send a signal to a process, or to a whole process group. Used by `kill` and
/// `tkill`.
///
/// Linux reads the id's *sign*: a positive value names one process, -1 names
/// every process the caller may signal (which this kernel reads as the
/// caller's own group), any other negative value names a group by number, and
/// zero names the caller's group. A shell leans on the negative case to stop a
/// job - and on the signal-0 case, which delivers nothing and only asks
/// whether the target is there.
pub(super) fn handle_kill(args: [usize; ARG_COUNT]) -> isize {
    let (id, sig) = (args[0] as isize, args[1]);
    if sig > 31 {
        return err::EINVAL;
    }
    let delivered = match id {
        n if n < 0 => {
            let pgid = if n == -1 {
                crate::sched::current_pgid()
            } else {
                TaskId(n.unsigned_abs())
            };
            crate::sched::send_signal_group(pgid, sig as u8)
        }
        0 => crate::sched::send_signal_group(crate::sched::current_pgid(), sig as u8),
        n => crate::sched::send_signal(TaskId(n as usize), sig as u8),
    };
    if delivered { 0 } else { err::ESRCH }
}

/// `setpgid(pid, pgid)` — move a task into a process group. A `pgid` of zero
/// means "the group led by `pid`", and both `pid` and `pgid` of zero are just
/// the caller.
pub(super) fn handle_setpgid(args: [usize; ARG_COUNT]) -> isize {
    let (pid, pgid) = (args[0] as isize, args[1] as isize);
    let me = crate::sched::current();
    let target = match pid {
        0 => me,
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };
    let group = match pgid {
        0 => target,
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };
    let r = crate::sched::set_pgid(target, group);
    crate::println!("[SETPGID] pid={pid} pgid={pgid} -> {r}");
    r
}

/// `getpgid(pid)` and `getpgrp()` — which group a task is in.
pub(super) fn handle_getpgid(args: [usize; ARG_COUNT]) -> isize {
    let pid = args[0] as isize;
    let target = match pid {
        0 => crate::sched::current(),
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };
    match crate::sched::pgid_of(target) {
        Some(pgid) => pgid.0 as isize,
        None => err::ESRCH,
    }
}

// ---- init's own calls: reboot, sigtimedwait, umask, sendfile -------------
//
// None of these had a handler while the machine was handed straight to a
// test script, and the first thing that ran after the handover - busybox
// init - asked for all of them within a second of starting.

/// The `cmd` words of `reboot(2)`, Linux x86_64. They are magic numbers, not
/// flag bits, and each is its own sentence.
const RB_DISABLE_CAD: usize = 0x0000_0000;
const RB_ENABLE_CAD: usize = 0x89ab_cdef;
const RB_AUTOBOOT: usize = 0x0123_4567;
const RB_HALT_SYSTEM: usize = 0xcdef_0123;
const RB_POWER_OFF: usize = 0x4321_fedc;

/// `reboot(cmd)` — what init asks of the machine.
///
/// The two policy words (ctrl-alt-del's disposition) are recorded nowhere and
/// answered with success, because this kernel has no console keyboard to wire
/// them to and nobody else reads them back. The three stop words end the
/// machine: QEMU's debug exit is the closest thing to a reset this
/// environment has, and `-no-reboot` turns it into a clean shutdown either
/// way. Anything else is refused, as Linux refuses it.
pub(super) fn handle_reboot(args: [usize; ARG_COUNT]) -> isize {
    let cmd = args[0];
    match cmd {
        RB_DISABLE_CAD | RB_ENABLE_CAD => 0,
        RB_AUTOBOOT | RB_HALT_SYSTEM | RB_POWER_OFF => {
            crate::println!("mpos: reboot(0x{cmd:x}) - handing the machine back");
            crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_SUCCESS);
        }
        _ => err::EINVAL,
    }
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` — wait for one signal
/// from a set and consume it without running any handler.
///
/// This is the door a daemon's main loop walks through: busybox init blocks
/// everything but SIGCHLD and parks here, so a child's exit is noticed the
/// moment the signal arrives rather than on a poll timer. Two kernel-side
/// rules give it its shape. The signal is taken straight off the pending
/// set: no disposition is consulted, which is what lets a daemon eat its own
/// SIGTERM. And the two unblockable signals are silently stripped from the
/// set, as Linux does: init waits on a full set minus SIGCHLD, which names
/// them both, and refusing the call would stop init from ever running.
///
/// A NULL timeout waits forever; a zero one checks and answers `EAGAIN`.
/// A fatal signal that arrives from *outside* the set still kills the waiter
/// on each pass, exactly as `block_until` does for every blocking call.
pub(super) fn handle_rt_sigtimedwait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SI_USER: i32 = 0;
    /// The bytes of `siginfo_t` on x86_64.
    const SIGINFO_BYTES: usize = 128;

    let (set_ptr, info, timeout, sigsetsize) = (args[0], args[1], args[2], args[3]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    let mut wanted = 0u32;
    if set_ptr != 0 {
        if !buffer_ok(set_ptr, 8, true, from_user) {
            return err::EFAULT;
        }
        wanted = unsafe { (set_ptr as *const u32).read_volatile() };
    }
    let wanted = wanted
        & !(crate::sched::signal::bit(crate::sched::signal::SIGKILL))
        & !(crate::sched::signal::bit(crate::sched::signal::SIGSTOP));

    // The timeout is a `struct timespec`, read to a tick deadline. A zero
    // deadline is "check once" - the sleeps below never run and `EAGAIN`
    // comes back the first time round.
    let deadline = if timeout != 0 {
        if !buffer_ok(timeout, 16, true, from_user) {
            return err::EFAULT;
        }
        let sec = unsafe { (timeout as *const i64).read_volatile() };
        let nsec = unsafe { ((timeout + 8) as *const i64).read_volatile() };
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return err::EINVAL;
        }
        let ms = sec.saturating_mul(1_000) + nsec / 1_000_000;
        Some(crate::arch::timer::ticks() + ms as u64 * crate::arch::timer::TICK_HZ as u64 / 1_000)
    } else {
        None
    };

    loop {
        if let Some(sig) = crate::sched::take_signal_in_set(wanted) {
            if info != 0 {
                if !buffer_ok(info, SIGINFO_BYTES, false, from_user) {
                    return err::EFAULT;
                }
                unsafe {
                    core::ptr::write_bytes(info as *mut u8, 0, SIGINFO_BYTES);
                    (info as *mut i32).write_volatile(sig as i32); // si_signo
                    ((info + 8) as *mut i32).write_volatile(SI_USER); // si_code
                }
            }
            return sig as isize;
        }
        if let Some(sig) = crate::sched::take_fatal_signal() {
            crate::sched::exit(-(sig as isize));
        }
        if let Some(end) = deadline
            && crate::arch::timer::ticks() >= end
        {
            return err::EAGAIN;
        }
        crate::sched::sleep_ticks(1);
    }
}

/// The file-creation mask, `0o022` the way init leaves it.
static CREATION_MASK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0o022);

/// `umask(mask)` — set the creation mask, and report what it was. No path
/// records permission bits yet, so the mask is kept and handed back only;
/// init's `umask(0o22)` still gets the answer it expects.
pub(super) fn handle_umask(args: [usize; ARG_COUNT]) -> isize {
    use core::sync::atomic::Ordering;
    let old = CREATION_MASK.swap(args[0] as u32 & 0o7777, Ordering::Relaxed);
    old as isize
}

/// `sendfile(out, in, offset, count)` — copy bytes between descriptors in the
/// kernel. The in-descriptor must be a file (anything `read_at` reaches); the
/// out-descriptor gets the bytes wherever a `write` would put them.
///
/// `offset` is an in-out `loff_t *`: NULL uses and advances the in-file's own
/// cursor, otherwise the position lives in the caller's word. The one caller
/// on this system asks for a short copy from a file to the console, and that
/// is the shape this serves.
pub(super) fn handle_sendfile(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (out_fd, in_fd, offset_ptr, count) = (args[0] as i32, args[1] as i32, args[2], args[3]);
    let fds = crate::sched::current_fds();
    let out = match fds.clone_at(out_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let input = match fds.clone_at(in_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    if count == 0 {
        return 0;
    }

    let mut pos: u64 = if offset_ptr != 0 {
        if !buffer_ok(offset_ptr, 8, true, from_user) {
            return err::EFAULT;
        }
        unsafe { (offset_ptr as *const u64).read_volatile() }
    } else {
        input.pos()
    };

    let mut sent: usize = 0;
    let mut chunk = [0u8; 4096];
    while sent < count {
        let want = (count - sent).min(chunk.len());
        let read = if offset_ptr != 0 {
            input.inode().read_at(pos, &mut chunk[..want])
        } else {
            input.read(&mut chunk[..want])
        };
        let n = match read {
            Ok(n) => n,
            Err(e) => {
                if sent > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if n == 0 {
            break;
        }
        let w = match out.write(&chunk[..n]) {
            Ok(w) => w,
            Err(e) => {
                if sent > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if w == 0 {
            break;
        }
        pos += w as u64;
        sent += w;
    }

    if offset_ptr != 0 {
        if !buffer_ok(offset_ptr, 8, false, from_user) {
            return err::EFAULT;
        }
        unsafe { (offset_ptr as *mut u64).write_volatile(pos) };
    }
    sent as isize
}

/// `brk(addr)` — change the program break (heap end).
pub(super) fn handle_brk(args: [usize; ARG_COUNT]) -> isize {
    let addr = args[0];
    crate::sched::brk(addr)
}

use crate::mm::vm::MapFlags;

/// `mmap`'s flag bits, as Linux numbers them.
const MAP_SHARED: usize = 0x01;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
/// `mprotect`'s protection bits.
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
/// `mremap`'s flag bits: the move may go somewhere else, and the caller named
/// where.
const MREMAP_MAYMOVE: usize = 0x1;
const MREMAP_FIXED: usize = 0x2;
/// One past the highest address a process may map: the lower half of the
/// canonical address space, which is where every user mapping lives.
const USER_LIMIT: usize = 1 << 47;

/// `mmap(addr, length, prot, flags, fd, offset)` — map memory, from a file or
/// from nothing.
///
/// A file mapping is read in at the moment it is made: the pages are private
/// copies of what the file held, and nothing tracks the file afterwards. That
/// is the whole of `MAP_PRIVATE` for a reader, and it is what the dynamic
/// linker needs - it maps a shared object read-only, parses it, and then
/// `mprotect`s the pieces it wants writable.
///
/// `MAP_SHARED` is served by that same read-in whenever a file backs the
/// mapping. A reader cannot tell the two apart - no write of its lands back
/// in the file either way - and real programs reach for the shared spelling
/// for plain reads: apk's index reader maps the fetched `APKINDEX` with
/// `MAP_SHARED` and refuses the whole update when the kernel says that is
/// unimplemented. Only the anonymous shape is refused, because that is the
/// one whose contract - two processes watching each other's writes through
/// shared frames - mpos genuinely does not keep.
///
/// The address handling is Linux's, and two details of it are load-bearing for
/// a real libc:
///
/// * `MAP_FIXED` *takes* the range. Whatever was mapped there is dropped first,
///   so the call cannot fail because something was in the way. musl's mallocng
///   depends on this: it claims the guard page of its heap by mapping
///   `PROT_NONE` over the first page of a range it has just grown with `brk`.
///   A kernel that refuses - or worse, asserts - breaks every musl program.
/// * Without `MAP_FIXED` the address is only a hint, and a hint that is still
///   in use is simply not taken: the mapping goes wherever there is room.
///
/// `PROT_NONE` maps the pages *present but unreachable from ring 3* rather than
/// leaving them unmapped, so a later `mprotect` can hand them back with their
/// contents intact.
pub(super) fn handle_mmap(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;

    let (addr, length, prot, flags, fd, offset) =
        (args[0], args[1], args[2], args[3], args[4] as i32, args[5]);

    let anon = flags & MAP_ANONYMOUS != 0;
    let shared = flags & MAP_SHARED != 0;
    let fixed = flags & MAP_FIXED != 0;

    if length == 0 {
        return err::EINVAL;
    }
    // A shared *anonymous* mapping is the one shape this kernel cannot keep:
    // two processes seeing the same frames through. A shared *file* mapping
    // for a reader is indistinguishable from the private read-in below, and
    // apk's index reader depends on being served one.
    if shared && anon {
        return err::ENOSYS;
    }

    // What the mapping is filled from. The descriptor is ignored when the
    // mapping is anonymous, as Linux ignores it.
    let backing = if anon {
        None
    } else {
        // The offset of a file mapping is a page number, not a byte count, so
        // one that is not page-aligned has no meaning to honour.
        if !offset.is_multiple_of(PAGE_SIZE) {
            return err::EINVAL;
        }
        match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => Some(file),
            Err(_) => return err::EBADF,
        }
    };

    let len = crate::mm::pframe::align_up(length, PAGE_SIZE);

    // An address a process can actually use: inside the lower half, on a page
    // boundary, and not wrapping.
    let want = (addr != 0
        && addr.is_multiple_of(PAGE_SIZE)
        && addr.checked_add(len).is_some_and(|end| end < USER_LIMIT))
    .then_some(addr);

    let base = match (fixed, want) {
        // Linux: a fixed request *takes* the range, replacing what is there.
        (true, Some(addr)) => {
            crate::sched::release_user_frames(crate::sched::unmap_user(addr, len));
            addr
        }
        // A fixed request for an address that cannot be honoured is an error,
        // not something to paper over by mapping elsewhere: the caller is
        // relying on the address.
        (true, None) => return err::EINVAL,
        // Without `MAP_FIXED` the address is only a hint, and one that is
        // already in use is simply not taken.
        (false, Some(addr)) if crate::sched::user_range_free(addr, len) => addr,
        _ => match free_range(len, crate::sched::get_mmap_cursor()) {
            Some((base, new_cursor)) => {
                crate::sched::set_mmap_cursor(new_cursor);
                base
            }
            None => return err::ENOMEM,
        },
    };

    // Readable is what a present page is; `USER` is what makes it reachable
    // from ring 3 at all, and `PROT_NONE` is exactly the absence of that bit.
    let mut mf = MapFlags::READ;
    if prot & PROT_WRITE != 0 {
        mf |= MapFlags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        mf |= MapFlags::EXEC;
    }
    if prot != 0 {
        mf |= MapFlags::USER;
    }

    for page in (base..base + len).step_by(PAGE_SIZE) {
        // The pages of a replaced range are gone; these are fresh ones, zeroed
        // as POSIX promises for anonymous memory and as the tail of a file
        // mapping promises past the end of the file.
        let frame = match crate::mm::alloc_frames(0) {
            Some(frame) => frame,
            None => return err::ENOMEM,
        };
        let dest = unsafe {
            core::slice::from_raw_parts_mut(crate::mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
        };
        dest.fill(0);

        if let Some(file) = &backing {
            // Copy what the file has for this page. A short read ends the
            // copy rather than failing it: the last page of a mapping is
            // rarely full, and the zeros already there are the right answer
            // for the rest.
            let inode = file.inode();
            let start = offset + (page - base);
            let mut done = 0;
            while done < PAGE_SIZE {
                match inode.read_at((start + done) as u64, &mut dest[done..]) {
                    Ok(0) => break,
                    Ok(n) => done += n,
                    Err(_) => break,
                }
            }
        }

        if crate::sched::current_map_user(page, frame, mf) < 0 {
            return err::ENOMEM;
        }
    }

    base as isize
}

/// `mremap(old, old_size, new_size, flags, new_address)` — resize or move a
/// mapping.
///
/// The pages are the *same frames* before and after: a move re-maps them at the
/// new address and takes the old address away, with nothing copied and nothing
/// given back to the allocator. That is what lets a caller keep what a mapping
/// held across the call, which is the whole point of asking.
///
/// Without `MREMAP_MAYMOVE` the mapping can only be resized where it is: a
/// shrink always works, a grow works when the pages above are free, and
/// anything else answers `ENOMEM`. That last answer is load-bearing: musl's
/// `pthread_getattr_np` finds the bottom of a stack by probing with `mremap`
/// and reading `ENOMEM` as "still inside the mapping" and a failure of any
/// other kind as "here is the edge".
pub(super) fn handle_mremap(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;

    let (old_addr, old_len, new_len, flags, new_addr) =
        (args[0], args[1], args[2], args[3], args[4]);

    if old_len == 0 || new_len == 0 || !old_addr.is_multiple_of(PAGE_SIZE) {
        return err::EINVAL;
    }
    let old = crate::mm::pframe::align_up(old_len, PAGE_SIZE);
    let new = crate::mm::pframe::align_up(new_len, PAGE_SIZE);
    if old_addr
        .checked_add(old)
        .is_none_or(|end| end >= USER_LIMIT)
    {
        return err::EINVAL;
    }
    let may_move = flags & MREMAP_MAYMOVE != 0;
    let to_fixed = flags & MREMAP_FIXED != 0;
    // Linux: asking for a destination is asking to move there.
    if to_fixed && !may_move {
        return err::EINVAL;
    }

    // Every page has to be mapped: mremap moves mappings, and a hole has
    // nothing in it to move. Whether each page is *reachable from ring 3* is
    // kept, so a `PROT_NONE` page stays one after the move.
    let mut frames = alloc::vec::Vec::new();
    for page in (old_addr..old_addr + old).step_by(PAGE_SIZE) {
        let Some(frame) = crate::sched::user_frame_at(page) else {
            return err::EFAULT;
        };
        let reachable = crate::arch::paging::user_accessible(page, PAGE_SIZE, false);
        frames.push((frame, reachable));
    }

    if !to_fixed {
        // A shrink, or a grow into the pages just above.
        if new <= old {
            crate::sched::release_user_frames(crate::sched::unmap_user(old_addr + new, old - new));
            return old_addr as isize;
        }
        if crate::sched::user_range_free(old_addr + old, new - old) {
            return if add_pages(old_addr + old, new - old) {
                old_addr as isize
            } else {
                err::ENOMEM
            };
        }
        if !may_move {
            return err::ENOMEM;
        }
    }

    // Move: either into the range the caller named, which is taken the way
    // `MAP_FIXED` takes one, or into the first free run big enough.
    let dest = if to_fixed {
        if new_addr % PAGE_SIZE != 0
            || new_addr
                .checked_add(new)
                .is_none_or(|end| end >= USER_LIMIT)
            // A destination overlapping the source would have its pages freed
            // underneath the move. Linux allows it; this does not, and says so.
            || new_addr < old_addr + old && old_addr < new_addr + new
        {
            return err::EINVAL;
        }
        crate::sched::release_user_frames(crate::sched::unmap_user(new_addr, new));
        new_addr
    } else {
        match free_range(new, crate::sched::get_mmap_cursor()) {
            Some((dest, new_cursor)) => {
                crate::sched::set_mmap_cursor(new_cursor);
                dest
            }
            None => return err::ENOMEM,
        }
    };

    // The pages go down before the old ones come away, so a mapping that
    // cannot be placed leaves the original intact rather than half moved.
    for (index, (frame, reachable)) in frames.iter().enumerate() {
        let mut mf = MapFlags::READ | MapFlags::WRITE;
        if *reachable {
            mf |= MapFlags::USER;
        }
        if !crate::sched::move_user_page(dest + index * PAGE_SIZE, *frame, mf) {
            return err::ENOMEM;
        }
    }
    if !add_pages(dest + old, new - old) {
        return err::ENOMEM;
    }
    // The frames moved with the mapping, so this frees nothing.
    crate::sched::unmap_user(old_addr, old);

    dest as isize
}

/// Map `len` fresh, zeroed, process-owned pages at `virt`.
fn add_pages(virt: usize, len: usize) -> bool {
    use crate::mm::pframe::PAGE_SIZE;
    for page in (virt..virt + len).step_by(PAGE_SIZE) {
        let Some(frame) = crate::mm::alloc_frames(0) else {
            return false;
        };
        unsafe { core::ptr::write_bytes(crate::mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
        let flags = MapFlags::READ | MapFlags::WRITE | MapFlags::USER;
        if crate::sched::current_map_user(page, frame, flags) < 0 {
            return false;
        }
    }
    true
}

/// The first run of `len` bytes, page-aligned and below the mmap base, that
/// this process has nothing mapped in.
///
/// The search runs downward and the cursor is the *bottom* of the last mapping
/// placed: the next anonymous mapping goes just below the previous one, which
/// is the direction Linux's mmap region grows in. `cursor` is the per-process
/// mmap cursor; the returned value is the updated cursor to store back.
fn free_range(len: usize, cursor: usize) -> Option<(usize, usize)> {
    use crate::mm::pframe::PAGE_SIZE;

    // A stale cursor from an old layout can only ever sit above the base; the
    // clamp keeps the first placement of every process at the top.
    let mut top = cursor.min(MMAP_BASE);
    loop {
        let bottom = top.checked_sub(len)? & !(PAGE_SIZE - 1);
        if crate::sched::user_range_free(bottom, len) {
            return Some((bottom, bottom));
        }
        top = bottom;
    }
}

/// `munmap(addr, length)` — drop mappings.
///
/// Linux rounds the range outwards to whole pages and ignores holes, which is
/// what [`crate::sched::unmap_user`] does. The frames come back to the
/// allocator because nothing can reach them once the entries are gone.
pub(super) fn handle_munmap(args: [usize; ARG_COUNT]) -> isize {
    let (addr, length) = (args[0], args[1]);
    if !addr.is_multiple_of(crate::mm::pframe::PAGE_SIZE) || length == 0 {
        return err::EINVAL;
    }
    crate::sched::release_user_frames(crate::sched::unmap_user(addr, length));
    0
}

/// `mprotect(addr, len, prot)` — change what a range of pages permits.
pub(super) fn handle_mprotect(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;

    let (addr, len, prot) = (args[0], args[1], args[2]);
    if !addr.is_multiple_of(PAGE_SIZE) {
        return err::EINVAL;
    }
    let len = crate::mm::pframe::align_up(len, PAGE_SIZE);
    if len == 0 {
        return 0;
    }

    let mut mf = MapFlags::READ;
    if prot & PROT_WRITE != 0 {
        mf |= MapFlags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        mf |= MapFlags::EXEC;
    }
    if prot != 0 {
        mf |= MapFlags::USER;
    }
    crate::sched::protect_user(addr, len, mf);
    0
}

/// The x86_64 `struct itimerval`: two `struct timeval`s - `it_value` first,
/// `it_interval` second - 32 bytes in all.
const ITIMERVAL_SIZE: usize = 32;

/// Write `snapshot` out as a `struct itimerval`, zeroed for a disarmed one.
/// Second granularity is what the kernel's clock supports, so the usec
/// fields stay zero.
/// # Safety
/// `at` must be a writable user address for 32 bytes, as the callers checked.
unsafe fn write_itimerval(at: usize, snapshot: Option<crate::sched::ItimerSnapshot>) {
    let (value, interval) = match snapshot {
        Some(s) => (s.remaining_ticks, s.interval_ticks),
        None => (0, 0),
    };
    let hz = crate::arch::timer::TICK_HZ as u64;
    let words: [u64; 4] = [value * 1000 / hz, 0, interval * 1000 / hz, 0];
    unsafe {
        for (n, word) in words.iter().enumerate() {
            (at as *mut u64).add(n).write_volatile(*word);
        }
    }
}

/// `setitimer(which, new_value, old_value)`.
///
/// Only `ITIMER_REAL` ever fires: it counts the wall-clock ticks the board
/// actually has. The virtual and profiling timers would need per-task CPU
/// accounting, which does not exist yet, so they are installed and read
/// back faithfully but never expire - the shape a caller that probes and
/// then cancels cannot distinguish from the real thing.
pub(super) fn handle_setitimer(args: [usize; ARG_COUNT]) -> isize {
    let (which, new_ptr, old_ptr) = (args[0], args[1], args[2]);
    if which > crate::sched::ITIMER_PROF {
        return err::EINVAL;
    }
    if old_ptr != 0 && !buffer_ok(old_ptr, ITIMERVAL_SIZE, true, true) {
        return err::EFAULT;
    }
    let (value_ticks, interval_ticks) = if new_ptr == 0 {
        // A NULL new_value is Linux's spelling of "just tell me the old one".
        (0, 0)
    } else {
        if !buffer_ok(new_ptr, ITIMERVAL_SIZE, false, true) {
            return err::EFAULT;
        }
        unsafe {
            let word = |n: usize| (new_ptr as *const u64).add(n).read_volatile();
            let (vsec, vusec, isec, iusec) = (word(0), word(1), word(2), word(3));
            if vusec >= 1_000_000 || iusec >= 1_000_000 {
                return err::EINVAL;
            }
            let value_ms = vsec * 1000 + vusec.div_ceil(1000);
            let interval_ms = isec * 1000 + iusec.div_ceil(1000);
            let hz = crate::arch::timer::TICK_HZ as u64;
            (value_ms * hz / 1000, interval_ms * hz / 1000)
        }
    };
    let new = if value_ticks == 0 {
        // A zero it_value disarms, exactly as the man page words it.
        None
    } else {
        Some(crate::sched::ItimerState {
            interval_ticks,
            deadline: crate::arch::timer::ticks() + value_ticks.max(1),
        })
    };
    let old = crate::sched::itimer_swap(which, new);
    if old_ptr != 0 {
        unsafe { write_itimerval(old_ptr, old) };
    }
    0
}

/// `getitimer(which, curr_value)`.
pub(super) fn handle_getitimer(args: [usize; ARG_COUNT]) -> isize {
    let (which, ptr) = (args[0], args[1]);
    if which > crate::sched::ITIMER_PROF {
        return err::EINVAL;
    }
    if !buffer_ok(ptr, ITIMERVAL_SIZE, true, true) {
        return err::EFAULT;
    }
    unsafe { write_itimerval(ptr, crate::sched::itimer_snapshot(which)) };
    0
}

/// `alarm(seconds)`.
///
/// The one-shot spelling of `setitimer(ITIMER_REAL)`. The answer is the time
/// the previous alarm had left, rounded up to seconds as the man page words
/// it - zero when no alarm was set.
pub(super) fn handle_alarm(args: [usize; ARG_COUNT]) -> isize {
    let seconds = args[0] as u64;
    let hz = crate::arch::timer::TICK_HZ as u64;
    let new = if seconds == 0 {
        None
    } else {
        Some(crate::sched::ItimerState {
            interval_ticks: 0,
            deadline: crate::arch::timer::ticks() + (seconds * hz).max(1),
        })
    };
    match crate::sched::itimer_swap(crate::sched::ITIMER_REAL, new) {
        Some(s) if s.remaining_ticks > 0 => s.remaining_ticks.div_ceil(hz) as isize,
        _ => 0,
    }
}

/// `sysinfo(info)` - fill the 112-byte Linux x86_64 `struct sysinfo`.
///
/// The numbers come from counters the kernel keeps anyway: uptime from the
/// tick count, memory from the frame allocator, the process count from the
/// task table. Loads, swap and the high-memory halves report zero - nobody
/// here measures them.
pub(super) fn handle_sysinfo(args: [usize; ARG_COUNT]) -> isize {
    let buf = args[0];
    const SYSINFO_SIZE: usize = 112;
    if !buffer_ok(buf, SYSINFO_SIZE, true, true) {
        return err::EFAULT;
    }
    let hz = crate::arch::timer::TICK_HZ as u64;
    let ticks = crate::arch::timer::ticks();
    let stats = crate::mm::frame_stats();
    let page = 4096u64;
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, SYSINFO_SIZE);
        let word = |offset: usize| (buf + offset) as *mut u64;
        word(0).write_volatile(ticks / hz); // uptime
        word(32).write_volatile(stats.total as u64 * page); // totalram
        word(40).write_volatile(stats.free as u64 * page); // freeram
        (buf as *mut u16)
            .add(40)
            .write_volatile(crate::sched::task_count() as u16); // procs
        (buf as *mut u32).add(26).write_volatile(1); // mem_unit: bytes
    }
    0
}

/// `clock_gettime(clk_id, tp)` — get time.
///
/// `tp` points to a `struct timespec { tv_sec, tv_nsec }`.
///
/// Two clocks are distinguished the way Linux draws the line: the REALTIME
/// family answers with wall time - the RTC's boot anchor plus the tick count -
/// and everything monotonic answers with uptime alone. The distinction is the
/// contract TLS libraries live on: certificate validity is checked against
/// REALTIME and must see the calendar, while monotonic callers must never see
/// that anchor jump under them.
pub(super) fn handle_clock_gettime(args: [usize; ARG_COUNT]) -> isize {
    const CLOCK_MONOTONIC: usize = 1;
    const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
    const CLOCK_THREAD_CPUTIME_ID: usize = 3;
    const CLOCK_MONOTONIC_RAW: usize = 4;
    const CLOCK_MONOTONIC_COARSE: usize = 6;
    const CLOCK_BOOTTIME: usize = 7;
    let (clk_id, tp) = (args[0], args[1]);
    if tp == 0 {
        return err::EFAULT;
    }
    // Return the tick count, anchored to the RTC's wall time for the
    // REALTIME clocks.
    let ticks = crate::arch::timer::ticks();
    let freq = crate::arch::timer::TICK_HZ as u64;
    let monotonic = matches!(
        clk_id,
        CLOCK_MONOTONIC
            | CLOCK_PROCESS_CPUTIME_ID
            | CLOCK_THREAD_CPUTIME_ID
            | CLOCK_MONOTONIC_RAW
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME
    );
    let wall = if monotonic {
        0
    } else {
        crate::arch::rtc::boot_epoch()
    };
    let secs = wall + ticks / freq;
    let nsecs = (ticks % freq) * (1_000_000_000 / freq);
    unsafe {
        (tp as *mut i64).write_volatile(secs as i64);
        ((tp + 8) as *mut i64).write_volatile(nsecs as i64);
    }
    0
}

/// `gettimeofday(tv, tz)` — wall time as `struct timeval { tv_sec, tv_usec }`.
///
/// The same tick the `clock_gettime` family reports, in the microsecond unit
/// this call's struct spells: a `tv_usec` that stayed at nanosecond scale
/// would answer a million-fold too much to every caller that trusts it.
pub(super) fn handle_gettimeofday(args: [usize; ARG_COUNT]) -> isize {
    let tv = args[0];
    if tv == 0 {
        return err::EFAULT;
    }
    // gettimeofday has no monotonic variant - every caller wants the wall
    // clock, so the RTC anchor rides along here unconditionally.
    let ticks = crate::arch::timer::ticks();
    let freq = crate::arch::timer::TICK_HZ as u64;
    let secs = crate::arch::rtc::boot_epoch() + ticks / freq;
    let usecs = (ticks % freq) * (1_000_000 / freq);
    unsafe {
        (tv as *mut i64).write_volatile(secs as i64);
        ((tv + 8) as *mut i64).write_volatile(usecs as i64);
    }
    0
}

/// `uname(buf)` — write system name/version info.
///
/// `buf` points to a `struct utsname`: six 65-byte NUL-terminated strings -
/// sysname, nodename, release, version, machine, domainname - 390 bytes in
/// all, which is the size every caller passes and the size this validates.
pub(super) fn handle_uname(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let buf = args[0];
    let size = 390; // 6 fields × 65 bytes
    if !buffer_ok(buf, size, true, from_user) {
        return err::EFAULT;
    }
    unsafe {
        let base = buf as *mut u8;
        // All fields zeroed first.
        core::ptr::write_bytes(base, 0, size);
        let write_str = |offset: usize, s: &str| {
            let dst = base.add(offset);
            let bytes = s.as_bytes();
            let n = bytes.len().min(64);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
        };
        write_str(0, "mpos"); // sysname
        write_str(65, "mpos"); // nodename
        write_str(130, "0.1.0"); // release
        write_str(195, "0.1.0"); // version
        write_str(260, "x86_64"); // machine
        write_str(325, "(none)"); // domainname
    }
    0
}

/// `getrandom(buf, len, flags)` — fill `buf` with random bytes.
///
/// The same stream `/dev/urandom` reads from. `GRND_RANDOM` and
/// `GRND_NONBLOCK` are accepted and ignored: there is one generator, it is
/// always ready, and it never blocks - which is the behaviour both flags are
/// asking about.
pub(super) fn handle_getrandom(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (buf, len, _flags) = (args[0], args[1], args[2]);
    if len == 0 {
        return 0;
    }
    if !buffer_ok(buf, len, true, from_user) {
        return err::EFAULT;
    }
    // Filled straight into the caller's buffer: a kernel buffer here would be
    // a heap allocation per call for a request that is usually 32 bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    crate::rand::fill(out);
    len as isize
}

/// `prlimit64(pid, resource, new_limit, old_limit)` — get/set resource limits.
///
/// Minimal: return hard-coded defaults for RLIMIT_STACK and RLIMIT_NOFILE.
/// A resource number past `RLIM_NLIMITS` is `EINVAL`, the errno Linux uses
/// for one it has no table row for.
pub(super) fn handle_prlimit64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const RLIM_NLIMITS: usize = 16;
    let (_pid, resource, _new_ptr, old_ptr) = (args[0], args[1], args[2], args[3]);
    if resource >= RLIM_NLIMITS {
        return err::EINVAL;
    }
    if old_ptr == 0 {
        return 0;
    }
    if !buffer_ok(old_ptr, 16, true, from_user) {
        return err::EFAULT;
    }
    // struct rlimit64 { rlim_cur, rlim_max } — both u64.
    let (cur, max): (u64, u64) = match resource {
        3 => (8 * 1024 * 1024, 8 * 1024 * 1024), // RLIMIT_STACK
        5 => (1024, 1048576),                    // RLIMIT_NOFILE
        7 => (4096, 4096),                       // RLIMIT_NPROC
        _ => (u64::MAX, u64::MAX),
    };
    unsafe {
        (old_ptr as *mut u64).write_volatile(cur);
        ((old_ptr + 8) as *mut u64).write_volatile(max);
    }
    0
}

/// `futex(uaddr, op, val, ...)` — basic futex support for threading.
///
/// Minimal: only supports FUTEX_WAIT with immediate return (no actual blocking).
pub(super) fn handle_futex(args: [usize; ARG_COUNT]) -> isize {
    let (_uaddr, op, _val) = (args[0], args[1], args[2]);
    match op & 0xf {
        0 => 0, // FUTEX_WAIT: stub — just return 0
        1 => 0, // FUTEX_WAKE: stub — wake 0 threads
        _ => 0,
    }
}

/// `sched_getaffinity(pid, len, mask)` — which CPUs the caller may run on.
///
/// mpos schedules on one CPU, so the mask is always bit 0 and the answer is
/// the CPU count Linux returns: 1. Bytes of the mask beyond the first word
/// are zeroed, the way Linux clears what it does not fill.
pub(super) fn handle_sched_getaffinity(args: [usize; ARG_COUNT]) -> isize {
    let (_pid, len, mask) = (args[0], args[1], args[2]);
    if len < core::mem::size_of::<u64>() {
        return err::EINVAL;
    }
    if !buffer_ok(mask, len, true, true) {
        return err::EFAULT;
    }
    unsafe {
        (mask as *mut u64).write_volatile(1);
        if len > core::mem::size_of::<u64>() {
            core::ptr::write_bytes(
                (mask + core::mem::size_of::<u64>()) as *mut u8,
                0,
                len - core::mem::size_of::<u64>(),
            );
        }
    }
    1
}

/// `set_robust_list(head, len)` — register the robust-futex list head.
///
/// The head's contents are never touched; Linux pins the size to
/// `sizeof(struct robust_list_head)` = 24 and rejects anything else.
pub(super) fn handle_set_robust_list(args: [usize; ARG_COUNT]) -> isize {
    let (_head, len) = (args[0], args[1]);
    if len != 24 {
        return err::EINVAL;
    }
    0
}

/// `rseq(rseq, len, flags, sig)` — register the restartable-sequence block.
///
/// Registration succeeds and nothing is maintained afterwards: glibc only
/// needs to get past startup, and whether the kernel updates `cpu_id` on
/// preemption is out of scope. Linux rejects a length other than
/// `sizeof(struct rseq)` = 32 and any flag but 0.
pub(super) fn handle_rseq(args: [usize; ARG_COUNT]) -> isize {
    let (_rseq, len, flags) = (args[0], args[1], args[2]);
    if len != 32 || flags != 0 {
        return err::EINVAL;
    }
    0
}

// ---------------------------------------------------------------------------
// The calls an ordinary libc reaches for
// ---------------------------------------------------------------------------

/// `time(tloc)` — seconds since the epoch.
///
/// The original spelling of the clock, and the one a program that only
/// wants `now` still reaches for: `date(1)`, a TLS library's first sanity
/// check, a log line. Same wall clock `clock_gettime(CLOCK_REALTIME)`
/// answers - the RTC anchor plus uptime. `tloc`, when given, is written
/// through as well as returned, which is the shape the man page specifies.
pub(super) fn handle_time(args: [usize; ARG_COUNT]) -> isize {
    let tloc = args[0];
    let secs = crate::arch::rtc::boot_epoch()
        + crate::arch::timer::ticks() / crate::arch::timer::TICK_HZ as u64;
    if tloc != 0 {
        if !buffer_ok(tloc, 8, true, true) {
            return err::EFAULT;
        }
        unsafe {
            (tloc as *mut i64).write_volatile(secs as i64);
        }
    }
    secs as isize
}

/// `times(buf)` — this process's CPU time, in clock ticks.
///
/// `struct tms { clock_t tms_utime, tms_stime, tms_cutime, tms_cstime; }`.
/// This kernel does not account CPU time per task - a task runs whenever it
/// is scheduled and nothing clocks it - so all four report zero, which is
/// what a system with no accounting says. The returned value is the tick
/// count since boot: elapsed time, the thing the call is also specified to
/// hand back for a caller doing its own arithmetic.
pub(super) fn handle_times(args: [usize; ARG_COUNT]) -> isize {
    let buf = args[0];
    if buf == 0 {
        return crate::arch::timer::ticks() as isize;
    }
    if !buffer_ok(buf, 32, true, true) {
        return err::EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 32);
    }
    crate::arch::timer::ticks() as isize
}

/// `getrusage(who, usage)` — resource use for self or for reaped children.
///
/// `struct rusage` leads with two `timeval`s of CPU time and follows with
/// fourteen `long` counters. None of it is measured here, so the struct is
/// zeroed and handed back - an honest "nothing spent" rather than a number
/// invented to look plausible. `RUSAGE_CHILDREN` is answered the same way.
pub(super) fn handle_getrusage(args: [usize; ARG_COUNT]) -> isize {
    const RUSAGE_SELF: usize = 0;
    const RUSAGE_CHILDREN: usize = -1isize as usize;
    const RUSAGE_SIZE: usize = 144; // 2 timevals + 14 longs
    let (who, usage) = (args[0], args[1]);
    if who != RUSAGE_SELF && who != RUSAGE_CHILDREN {
        return err::EINVAL;
    }
    if !buffer_ok(usage, RUSAGE_SIZE, true, true) {
        return err::EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(usage as *mut u8, 0, RUSAGE_SIZE);
    }
    0
}

/// `clock_getres(clk_id, res)` — the resolution a clock can deliver.
///
/// `struct timespec { tv_sec, tv_nsec }`. The tick counter is the only
/// source of time there is, so the honest resolution is one tick - a
/// caller that asks is told what it will actually get, rather than the
/// nanosecond a modern Linux advertises. An unknown clock id is `EINVAL`.
pub(super) fn handle_clock_getres(args: [usize; ARG_COUNT]) -> isize {
    const CLOCK_REALTIME: usize = 0;
    const CLOCK_MONOTONIC: usize = 1;
    const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
    const CLOCK_THREAD_CPUTIME_ID: usize = 3;
    const CLOCK_MONOTONIC_RAW: usize = 4;
    const CLOCK_REALTIME_COARSE: usize = 5;
    const CLOCK_MONOTONIC_COARSE: usize = 6;
    const CLOCK_BOOTTIME: usize = 7;
    let (clk_id, res) = (args[0], args[1]);
    if !matches!(
        clk_id,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_PROCESS_CPUTIME_ID
            | CLOCK_THREAD_CPUTIME_ID
            | CLOCK_MONOTONIC_RAW
            | CLOCK_REALTIME_COARSE
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME
    ) {
        return err::EINVAL;
    }
    if res != 0 {
        if !buffer_ok(res, 16, true, true) {
            return err::EFAULT;
        }
        let hz = crate::arch::timer::TICK_HZ as u64;
        unsafe {
            (res as *mut i64).write_volatile(0);
            ((res + 8) as *mut i64).write_volatile((1_000_000_000 / hz) as i64);
        }
    }
    0
}

/// `waitid(idtype, id, infop, options, rusage)` — wait for one child and
/// describe it as a `siginfo_t`.
///
/// The modern wait: instead of a pid and a packed status word, `WEXITED`
/// and friends say which transitions count and the child arrives as an
/// info record - `si_pid`, `si_status`, and a `si_code` saying whether it
/// exited, was killed, or was stopped. `WNOHANG` answers zero with
/// `si_pid` cleared when no child is ready, which is how a caller polls
/// without blocking.
pub(super) fn handle_waitid(args: [usize; ARG_COUNT]) -> isize {
    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const WNOHANG: usize = 1;
    const WEXITED: usize = 4;
    const SIGCHLD: i32 = 17;
    const CLD_EXITED: i32 = 1;
    const CLD_KILLED: i32 = 2;
    /// `sizeof(siginfo_t)`: 128 bytes, the whole struct on x86_64.
    const SIGINFO_BYTES: usize = 128;

    let (idtype, _id, infop, options) = (args[0], args[1] as isize, args[2], args[3]);
    if idtype != P_ALL && idtype != P_PID {
        return err::EINVAL;
    }
    if !buffer_ok(infop, SIGINFO_BYTES, true, true) {
        return err::EFAULT;
    }
    // Nothing asked for means nothing to report - not a blank wait.
    if options & WEXITED == 0 {
        unsafe {
            core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
        }
        return 0;
    }

    let me = crate::sched::current();
    let collected = crate::sched::poll_any_child(me);
    let (pid, code) = match collected {
        Some(found) => found,
        None if options & WNOHANG != 0 => {
            // A poll that found nobody: success, with si_pid cleared so the
            // caller can tell "no child yet" from "this child".
            unsafe {
                core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
            }
            return 0;
        }
        None => {
            // Blocking, so wait for somebody. `poll_any_child` already
            // reported nobody ready; park until one of them finishes.
            match crate::sched::wait_any_child(me) {
                Some(found) => found,
                None => return err::ECHILD,
            }
        }
    };

    let (si_code, status) = if code < 0 {
        (CLD_KILLED, -code)
    } else {
        (CLD_EXITED, code)
    };
    unsafe {
        core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
        (infop as *mut i32).write_volatile(SIGCHLD); // si_signo
        ((infop + 8) as *mut i32).write_volatile(si_code); // si_code
        ((infop + 16) as *mut i32).write_volatile(pid.0 as i32); // si_pid
        ((infop + 24) as *mut i32).write_volatile(status as i32); // si_status
    }
    0
}

/// The resource limits `prlimit64` answers, in the 32-bit-`rlim_t` structure
/// the legacy pair uses. Same eight resources, same numbers: only the call
/// that reports them is older.
fn rlimit_of(resource: usize, out: usize, from_user: bool) -> isize {
    const RLIM_NLIMITS: usize = 16;
    if resource >= RLIM_NLIMITS {
        return err::EINVAL;
    }
    if out == 0 {
        return 0;
    }
    if !buffer_ok(out, 16, true, from_user) {
        return err::EFAULT;
    }
    let (cur, max): (u64, u64) = match resource {
        3 => (8 * 1024 * 1024, 8 * 1024 * 1024), // RLIMIT_STACK
        5 => (1024, 1_048_576),                  // RLIMIT_NOFILE
        7 => (4096, 4096),                       // RLIMIT_NPROC
        _ => (u64::MAX, u64::MAX),
    };
    unsafe {
        (out as *mut u64).write_volatile(cur);
        ((out + 8) as *mut u64).write_volatile(max);
    }
    0
}

/// `getrlimit(resource, rlim)` — read one resource limit.
pub(super) fn handle_getrlimit(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    rlimit_of(args[0], args[1], from_user)
}

/// `setrlimit(resource, rlim)` — set one resource limit.
///
/// The limits are advisory answers, not enforced ones, so a set is accepted
/// for any resource in range and changes nothing - the same contract
/// `prlimit64` has had. Out-of-range is `EINVAL`, an unwritable pointer
/// `EFAULT`.
pub(super) fn handle_setrlimit(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const RLIM_NLIMITS: usize = 16;
    let (resource, rlim) = (args[0], args[1]);
    if resource >= RLIM_NLIMITS {
        return err::EINVAL;
    }
    if rlim != 0 && !buffer_ok(rlim, 16, false, from_user) {
        return err::EFAULT;
    }
    0
}

/// `membarrier(cmd, flags, cpu_id)` — a memory barrier across the machine.
///
/// One core, no preemption between a load and a store a user program could
/// observe, so every command is already satisfied: the answer is success,
/// the way Linux answers for commands it can carry out. An unknown command
/// is `EINVAL`.
pub(super) fn handle_membarrier(args: [usize; ARG_COUNT]) -> isize {
    /// MEMBARRIER_CMD_QUERY and the two barriers a musl caller asks for.
    const CMD_QUERY: usize = 0;
    const CMD_GLOBAL: usize = 1;
    const CMD_PRIVATE_EXPEDITED: usize = 8;
    match args[0] {
        CMD_QUERY => 0, // no flags are needed, so nothing is reported
        CMD_GLOBAL | CMD_PRIVATE_EXPEDITED => 0,
        _ => err::EINVAL,
    }
}

/// `get_robust_list(pid, head, len)` — the read half of the robust-futex
/// list.
///
/// Threads are not implemented, so no process has a list, and Linux
/// answers `ENOSYS` for a kernel built without the call. The write half
/// still accepts a registration, which is what a glibc that probes the
/// pair needs to get past startup.
pub(super) fn handle_get_robust_list(args: [usize; ARG_COUNT]) -> isize {
    let _ = args;
    err::ENOSYS
}
