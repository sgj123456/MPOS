//! Linux-compatible syscall handlers. Memory: `mmap`, `munmap`, `mremap`,
//! `mprotect`, `brk`. Time: `nanosleep`, `clock_nanosleep`, `clock_gettime`,
//! `gettimeofday`. Identity: `getpid`, `gettid`, `getppid`, `kill`/`tkill`.
//! Process: `execve`, `exit`/`exit_group`, `wait4`, `fork` (clone), `uname`,
//! `getrandom`, `prlimit64`, `futex`, `sched_getaffinity`, `set_robust_list`,
//! `rseq`.

use alloc::string::String;
use alloc::sync::Arc;

use crate::sched::TaskId;
use crate::syscall::{ARG_COUNT, argv_arg, buffer_ok, cstr_arg, err, err_of};

pub(super) use crate::syscall::MMAP_BASE;

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

/// `wait4(pid, status_ptr, options)`. Linux writes the status through
/// `*status`; mpos returns the exit code directly - the kernel-side
/// reshape in `linux::dispatch` makes this consistent with the rest of the
/// wait4 family.
pub(super) fn handle_wait(args: [usize; ARG_COUNT]) -> isize {
    let (pid, flags) = (args[0], args[1]);
    const WNOHANG: usize = 1;
    let no_hang = flags & WNOHANG != 0;
    let me = crate::sched::current();
    if pid == usize::MAX || pid == -1i32 as usize {
        if no_hang {
            return match crate::sched::poll_any_child(me) {
                Some((_id, code)) => code,
                None => err::EAGAIN,
            };
        }
        return match crate::sched::wait_any_child(me) {
            Some((_id, code)) => code,
            None => err::ECHILD,
        };
    }
    if pid == 0 || pid >= crate::sched::MAX_TASKS {
        return err::ECHILD;
    }
    let id = TaskId(pid);
    if crate::sched::is_child(id, me) && !no_hang {
        crate::sched::block_until_code(id);
    }
    match crate::sched::take_exit_code(id) {
        Some(code) => code,
        None if no_hang => err::EAGAIN,
        None => err::ECHILD,
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
/// Linux shape: a NUL-terminated path, then NULL-terminated pointer arrays
/// for argv and envp. mpos maps the call onto its loader as a documented
/// approximation: the image is loaded as a new task that is a child of the
/// caller, the new task's id comes back, and the caller keeps running - a
/// real execve would replace the caller and never return. The child gets the
/// caller's argv and envp, both laid out on its stack the way the Linux ABI
/// does, and the spawned image's path is what `/proc/self/exe` reads
/// through.
pub(super) fn handle_execve(args: [usize; ARG_COUNT], cwd: &Arc<dyn crate::vfs::Inode>) -> isize {
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
    // envp is forwarded, not ignored: it becomes the child's initial
    // environment, what `environ`, `env::var`, and every libc getenv read
    // through.
    let envp = match argv_arg(args[2]) {
        Ok(envp) => envp,
        Err(e) => return e,
    };
    let bytes = match crate::vfs::read_file_opt(&path, Some(cwd.clone())) {
        Ok(bytes) => bytes,
        Err(e) => return err_of(e),
    };
    match crate::sched::try_spawn_user(
        &bytes,
        crate::sched::Priority::NORMAL,
        crate::sched::current(),
        &argv,
        &envp,
        &[],
        &path,
    ) {
        // Linux's `execve(2)` replaces the caller and never returns. mpos has
        // no way to swap a live task's image, so it spawns the new program as
        // a child of the caller and reports the child's pid - the kernel's
        // `posix_spawn` shape. The caller (`init`, the shell) is then expected
        // to `wait4` the pid it was given; that is what keeps the parent
        // alive and makes the child reaping work the way the rest of the
        // scheduler already does.
        Ok(id) => id.0 as isize,
        Err(crate::sched::SpawnError::NotExecutable) => err::ENOEXEC,
        Err(crate::sched::SpawnError::ArgsTooBig) => err::E2BIG,
        Err(crate::sched::SpawnError::BadFd) => err::EBADF,
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

/// `fork()` — create a copy of the calling process.
///
/// Minimal implementation: duplicate address space, fd table, and registers.
/// Returns 0 to child, child pid to parent.
pub(super) fn handle_fork(_args: [usize; ARG_COUNT]) -> isize {
    // For now, return ENOSYS — fork is complex and most coreutils don't need
    // it. The user can run each coreutil directly via execve from the shell.
    // The err:: constants are already negative (Linux ABI), so no extra sign.
    err::ENOSYS
}

/// Send a signal to a process. Used by `kill` and `tkill`.
pub(super) fn handle_kill(args: [usize; ARG_COUNT]) -> isize {
    let (id, sig) = (args[0], args[1]);
    if sig > 31 {
        return err::EINVAL;
    }
    if crate::sched::send_signal(TaskId(id), sig as u8) {
        0
    } else {
        err::ESRCH
    }
}

/// `brk(addr)` — change the program break (heap end).
pub(super) fn handle_brk(args: [usize; ARG_COUNT]) -> isize {
    let addr = args[0];
    let result = crate::sched::brk(addr);
    // Trace first few BRK calls per process for musl debug.
    static BRK_COUNTS: [core::sync::atomic::AtomicUsize; 64] =
        const { [const { core::sync::atomic::AtomicUsize::new(0) }; 64] };
    let pid = crate::sched::current().0;
    let n = BRK_COUNTS[pid].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 8 {
        crate::println!(
            "[brk #{}] pid={} req={:#x} -> {:#x}",
            n,
            pid,
            addr,
            result as usize
        );
    }
    result
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

/// `mmap(addr, length, prot, flags, fd, offset)` — map anonymous memory.
///
/// Only anonymous private mappings exist: mpos has no file mappings, and a
/// request for one is refused rather than answered with zeroed pages.
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

    let (addr, length, prot, flags, _fd, _offset) =
        (args[0], args[1], args[2], args[3], args[4] as i32, args[5]);

    let anon = flags & MAP_ANONYMOUS != 0;
    let shared = flags & MAP_SHARED != 0;
    let fixed = flags & MAP_FIXED != 0;

    if length == 0 {
        return err::EINVAL;
    }
    // Anonymous is the only kind mpos can build: a file mapping would need a
    // page cache to read the file through. The descriptor is ignored when the
    // mapping is anonymous, as Linux ignores it.
    if !anon {
        return err::ENODEV;
    }
    // A shared anonymous mapping is a mapping two processes would have to see
    // the same frames through, which mpos does not do yet.
    if shared {
        return err::ENOSYS;
    }

    let len = crate::mm::pframe::align_up(length, PAGE_SIZE);

    // Debug: trace large mmap requests that could indicate a bogus allocation.
    if len > 1024 * 1024 {
        crate::println!(
            "[mmap] size={:#x} ({} KiB), addr={:#x}, prot={:#x}, flags={:#x}, fd={}, offset={:#x}",
            len,
            len / 1024,
            addr,
            prot,
            flags,
            _fd,
            _offset
        );
    }
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
        // as POSIX promises for anonymous memory.
        let frame = match crate::mm::alloc_frames(0) {
            Some(frame) => frame,
            None => return err::ENOMEM,
        };
        unsafe { core::ptr::write_bytes(crate::mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
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

/// `clock_gettime(clk_id, tp)` — get time.
///
/// `tp` points to a `struct timespec { tv_sec, tv_nsec }`.
pub(super) fn handle_clock_gettime(args: [usize; ARG_COUNT]) -> isize {
    let (clk_id, tp) = (args[0], args[1]);
    let _ = clk_id;
    if tp == 0 {
        return err::EFAULT;
    }
    // Return a fixed time based on the timer tick count.
    let ticks = crate::arch::timer::ticks();
    let freq = crate::arch::timer::TICK_HZ as u64;
    let secs = ticks / freq;
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
    let ticks = crate::arch::timer::ticks();
    let freq = crate::arch::timer::TICK_HZ as u64;
    let secs = ticks / freq;
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

/// `getrandom(buf, len, flags)` — fill buffer with random bytes.
///
/// Uses the timer tick as a simple pseudo-random source.
pub(super) fn handle_getrandom(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (buf, len, _flags) = (args[0], args[1], args[2]);
    if len == 0 {
        return 0;
    }
    if !buffer_ok(buf, len, true, from_user) {
        return err::EFAULT;
    }
    let tick = crate::arch::timer::ticks();
    // Simple PRNG: xorshift64 seeded from the tick counter.
    let mut state = tick;
    unsafe {
        let dst = buf as *mut u8;
        for i in 0..len {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            dst.add(i).write_volatile(state as u8);
        }
    }
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
