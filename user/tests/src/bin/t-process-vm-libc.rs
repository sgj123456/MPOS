//! t-process-vm-libc - `process_vm_readv`/`process_vm_writev`: the calls a
//! tracer uses to move bytes into and out of another process's address space.
//!
//! `strace` reaches for these before falling back to a word-per-call
//! `PTRACE_PEEKDATA`, because one word at a time is hopeless for a long
//! `execve` argument vector. Both calls used to be gated `not(aarch64)` along
//! with `ptrace`, and that gate was stale: nothing in them is the first
//! machine's. They walk two iovec arrays (`transfer_iovs`, always ungated), ask
//! the scheduler for the target's address-space root (`sched::space_root_of`)
//! and the permission `PTRACE_ATTACH` would ask (`sched::may_inspect`), and
//! hand each chunk to `sched::read_user_of`/`write_user_of`. The second
//! machine's number table had named them all along (270/271 against x86_64's
//! 310/311) - what was missing was a reader.
//!
//! **Why `libc` and not `rustix`.** Every other test in this crate speaks
//! `rustix`, so that the number and the errno are the ABI's rather than the
//! author's memory of it. `rustix` binds neither of these calls, so they are
//! made through `libc::syscall` with the number from `libc::SYS_*` - a
//! maintained per-arch table, which is exactly what the `rustix` rule is for;
//! writing the number down here would be the failure that rule prevents. The
//! `-libc` suffix follows `t-unix-rustix`: the family, named with the interface
//! it is driven through.
//!
//! **What is pinned.** Both halves on the calling process's own address space
//! (Linux allows it, and it is the same code path with one fewer process in the
//! way): the byte counts a completed transfer returns, the bytes that cross,
//! the *cursor* walk when the two iovec arrays are different shapes - one side
//! long and the other two short, where pairing the iovecs up by index silently
//! mangles the transfer - and the refusals (`EINVAL` for a nonzero `flags` word
//! or an iovec count past 1024, `EFAULT` for a null array, an unmapped range, or
//! a remote page that is mapped but not reachable from ring 3, `ESRCH` for a
//! pid with no task). Then the real thing across two address spaces: a forked
//! child plants a pattern in its own copy of a buffer, reports where that copy
//! is, and parks there - so the bytes cannot move underneath the transfer -
//! while the parent reads it and overwrites its front, and the child's exit
//! status carries the verdict on what landed.

use std::io;
use std::os::raw::{c_int, c_long, c_ulong, c_void};
use std::process::exit;
use std::ptr;

// The errnos a refusal here can be, as Linux numbers them - the same on both
// machines. Read back through `std::io::Error::last_os_error` rather than a
// declared `__errno_location`: `syscall` is a libc entry point, so libc's own
// errno protocol is what applies.
const ESRCH: i32 = 3;
const EFAULT: i32 = 14;
const EINVAL: i32 = 22;

/// The 20 bytes the child plants in its own copy of the buffer, and the 15 the
/// parent writes over the front of them.
const PATTERN: [u8; 20] = *b"CHILD-DATA-012345678";
const PARENT_BYTES: [u8; 15] = *b"PARENT-WROTE-IT";

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

    fn check_call(&mut self, name: &str, got: Result<usize, i32>, want: usize) {
        self.check(
            &format!("{name} (got {got:?}, want {want})"),
            got == Ok(want),
        );
    }
}

/// One `iovec` over a buffer this process owns. The kernel reads both arrays
/// out of the caller's memory, so they are stack arrays of these.
fn iov(buf: &mut [u8]) -> libc::iovec {
    libc::iovec {
        iov_base: buf.as_mut_ptr().cast::<c_void>(),
        iov_len: buf.len(),
    }
}

/// One `iovec` naming an address in *another* process, which is not a Rust
/// reference and so is built from the number the other side reported.
fn iov_at(at: usize, len: usize) -> libc::iovec {
    libc::iovec {
        iov_base: at as *mut c_void,
        iov_len: len,
    }
}

/// `process_vm_readv`/`process_vm_writev`, reduced to what the two have in
/// common: a number, a pid, two iovec arrays and a flags word. Answers the
/// bytes that crossed, or the errno the call failed with.
fn vm(
    number: c_long,
    pid: libc::pid_t,
    local: *const libc::iovec,
    local_count: usize,
    remote: *const libc::iovec,
    remote_count: usize,
    flags: c_ulong,
) -> Result<usize, i32> {
    // SAFETY: the two arrays are slices this process owns, and the lengths are
    // the ones the kernel checks against them. `syscall`'s variadic arguments
    // are passed as the widths the ABI's registers take.
    let rc = unsafe {
        libc::syscall(
            number,
            pid as c_long,
            local,
            local_count as c_long,
            remote,
            remote_count as c_long,
            flags as c_long,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(rc as usize)
    }
}

/// The reading half, with the flags word the shipped call always passes as 0.
fn readv(pid: libc::pid_t, local: &[libc::iovec], remote: &[libc::iovec]) -> Result<usize, i32> {
    readv_flags(pid, local, remote, 0)
}

fn readv_flags(
    pid: libc::pid_t,
    local: &[libc::iovec],
    remote: &[libc::iovec],
    flags: c_ulong,
) -> Result<usize, i32> {
    vm(
        libc::SYS_process_vm_readv,
        pid,
        local.as_ptr(),
        local.len(),
        remote.as_ptr(),
        remote.len(),
        flags,
    )
}

/// The writing half.
fn writev(pid: libc::pid_t, local: &[libc::iovec], remote: &[libc::iovec]) -> Result<usize, i32> {
    writev_flags(pid, local, remote, 0)
}

fn writev_flags(
    pid: libc::pid_t,
    local: &[libc::iovec],
    remote: &[libc::iovec],
    flags: c_ulong,
) -> Result<usize, i32> {
    vm(
        libc::SYS_process_vm_writev,
        pid,
        local.as_ptr(),
        local.len(),
        remote.as_ptr(),
        remote.len(),
        flags,
    )
}

/// Map a page and hand its address back, with the mapping gone. The address is
/// therefore one with *no* entry behind it - a refusal a walk that asked only
/// for "is anything here" would get wrong in the other direction.
fn freed_page() -> usize {
    // SAFETY: a fresh anonymous mapping asked for with the length it is freed
    // with, and never read or written.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return 0;
    }
    unsafe {
        libc::munmap(at, 4096);
    }
    at as usize
}

/// A page that is *mapped but not reachable from ring 3* - the other refusal,
/// and the one that matters: the walk has to ask for the user bit, not merely
/// for a present entry, or it would read the supervisor half of every address
/// space. Zero when the page could not be made, so the caller reports rather
/// than assumes.
fn unreachable_page() -> usize {
    // SAFETY: as `freed_page`, but the mapping is kept and never touched.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        0
    } else {
        at as usize
    }
}

/// The cross-address-space half: a forked child plants a pattern in its own
/// copy of a buffer and parks while the parent reads it and overwrites its
/// front. The child's exit status is the verdict on what landed.
fn cross_process(r: &mut Report) {
    // The buffer both processes will have a copy of. The child writes its
    // pattern into *its* copy - a real user-mode write, which is what takes the
    // page out of the copy-on-write share with the parent, because the kernel's
    // cross-process writer reaches a page by translation and deliberately does
    // not break one. Without that write the parent's `process_vm_writev` would
    // land in a frame the two still share.
    let mut buf = Box::new([0u8; 32]);

    let mut to_parent = [0 as c_int; 2];
    let mut to_child = [0 as c_int; 2];
    // SAFETY: two two-element arrays, which is what `pipe` writes.
    let piped = unsafe {
        libc::pipe(to_parent.as_mut_ptr()) == 0 && libc::pipe(to_child.as_mut_ptr()) == 0
    };
    r.check("the handshake pipes are made", piped);
    if !piped {
        return;
    }

    // SAFETY: called before this process has any other thread, so the child
    // inherits a single-threaded program; it allocates nothing and touches only
    // the two descriptors it was handed and its own buffer.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // ---- the child's side ----
        let mine = buf.as_mut_ptr();
        // SAFETY: only this process's own descriptors and its own buffer are
        // touched, and `_exit` never returns.
        unsafe {
            libc::close(to_parent[0]);
            libc::close(to_child[1]);
            ptr::copy_nonoverlapping(PATTERN.as_ptr(), mine, PATTERN.len());
            // The address of this process's own copy. The parent does not
            // assume it: a fork reproduces the layout today, and this is the
            // side that knows.
            let word = (mine as usize).to_ne_bytes();
            libc::write(to_parent[1], word.as_ptr().cast::<c_void>(), 8);
            // Park until the parent has finished reading and writing: a running
            // child could be changing the buffer underneath the transfer.
            let mut go = [0u8; 1];
            libc::read(to_child[0], go.as_mut_ptr().cast::<c_void>(), 1);
            let mut after = [0u8; 20];
            ptr::copy_nonoverlapping(mine, after.as_mut_ptr(), 20);
            let mut want = [0u8; 20];
            want[..15].copy_from_slice(&PARENT_BYTES);
            want[15..].copy_from_slice(&PATTERN[15..]);
            // 0 when the parent's 15 bytes landed at the front of this
            // process's own copy and the tail behind them is still its own.
            libc::_exit(if after == want { 0 } else { 1 });
        }
    }

    // ---- the parent's side ----
    r.check("fork gave the parent a child pid", child > 0);
    unsafe {
        libc::close(to_parent[1]);
        libc::close(to_child[0]);
    }
    if child <= 0 {
        return;
    }

    let mut word = [0u8; 8];
    // SAFETY: reading the 8 bytes the child writes, through a descriptor this
    // process owns.
    let taken = unsafe { libc::read(to_parent[0], word.as_mut_ptr().cast::<c_void>(), 8) };
    r.check("the child sends the address of its buffer", taken == 8);
    let remote_at = usize::from_ne_bytes(word);
    r.check("and that word is an address", remote_at > 0);

    // Read the child's buffer. The 20 bytes are its pattern, planted before it
    // parked - so what crossed is the child's, not this process's zeros.
    let mut got = [0u8; 20];
    let local = [iov(&mut got)];
    let remote = [iov_at(remote_at, 20)];
    r.check_call(
        "process_vm_readv reads the child's buffer",
        readv(child, &local, &remote),
        20,
    );
    r.check(
        "and what crossed is the pattern the child planted",
        got == PATTERN,
    );

    // Write over its front. The child checks the result itself; all this side
    // can say is that the count came back whole.
    let mut send = PARENT_BYTES;
    let local_w = [iov(&mut send)];
    let remote_w = [iov_at(remote_at, 15)];
    r.check_call(
        "process_vm_writev writes the child's buffer",
        writev(child, &local_w, &remote_w),
        15,
    );

    // A pid with no task behind it: -1 is checked before the range and 9999 is
    // past mpos's task table. Linux answers `ESRCH` to both, for the same
    // reason - there is no such process.
    r.check_errno(
        "pid -1 names no task on the read",
        readv(-1, &local, &remote).unwrap_err(),
        ESRCH,
    );
    r.check_errno(
        "and on the write",
        writev(-1, &local_w, &remote_w).unwrap_err(),
        ESRCH,
    );

    // Release the child and collect its verdict.
    let go = [1u8];
    unsafe {
        libc::write(to_child[1], go.as_ptr().cast::<c_void>(), 1);
    }
    let mut status = 0 as c_int;
    let reaped = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "the child exits 0 - the bytes landed in its copy and its tail is its own",
        reaped == child && status & 0x7f == 0 && (status >> 8) & 0xff == 0,
    );
}

fn main() {
    println!("t-process-vm-libc");
    let mut r = Report { failed: false };

    // SAFETY: no arguments, no preconditions.
    let me = unsafe { libc::getpid() };

    // ---- the reading half, on this process's own address space ----
    //
    // Our own pid is the one target whose memory this process owns outright.
    // Linux allows it and so does mpos - the same code path with one fewer
    // process in the way - which makes it the cheap half of the pin: that the
    // call is *served* on this machine at all.
    let mut src = [0u8; 32];
    for (i, b) in src.iter_mut().enumerate() {
        *b = i as u8;
    }
    let mut dst = [0u8; 32];
    let local = [iov(&mut dst)];
    let remote = [iov(&mut src)];
    r.check_call(
        "process_vm_readv moves the whole range",
        readv(me, &local, &remote),
        32,
    );
    r.check("and the bytes that crossed are the source's", dst == src);

    // A local range shorter than the remote one bounds the transfer: the walk
    // moves the smaller of the two remainders at each step.
    let mut small = [0u8; 8];
    let short = [iov(&mut small)];
    r.check_call(
        "a short local range bounds the transfer",
        readv(me, &short, &remote),
        8,
    );
    r.check("and only the front of the range crossed", small == src[..8]);

    // The cursors - the part a plausible-looking simplification gets wrong. The
    // arrays are walked as two cursors and the smaller remainder moves at each
    // step, so 8+8 against 4+12 pairs as 4, then 4, then 8: sixteen bytes, with
    // the local side reading as one stream. Pairing the iovecs up by index
    // would move 4 then 8, stop at twelve, and get the middle wrong.
    let mut first = [0u8; 8];
    let mut second = [0u8; 8];
    let mut head = [0u8; 4];
    let mut tail = [0u8; 12];
    head.copy_from_slice(&src[..4]);
    tail.copy_from_slice(&src[4..16]);
    let two_local = [iov(&mut first), iov(&mut second)];
    let two_remote = [iov(&mut head), iov(&mut tail)];
    r.check_call(
        "two local iovecs against two differently-sized remote ones move 16",
        readv(me, &two_local, &two_remote),
        16,
    );
    r.check(
        "and the bytes crossed as one stream, not one iovec per index",
        first == src[..8] && second == src[8..16],
    );

    // ---- the writing half, the same way round ----
    let mut wdst = [0u8; 32];
    let mut wsrc = [0u8; 32];
    for (i, b) in wsrc.iter_mut().enumerate() {
        *b = 100 + i as u8;
    }
    let local_w = [iov(&mut wsrc)];
    let remote_w = [iov(&mut wdst)];
    r.check_call(
        "process_vm_writev moves the whole range",
        writev(me, &local_w, &remote_w),
        32,
    );
    r.check("and the bytes landed in the remote range", wdst == wsrc);

    let mut back = [0u8; 8];
    let short_w = [iov(&mut back)];
    r.check_call(
        "a short remote range bounds the write",
        writev(me, &local_w, &short_w),
        8,
    );
    r.check("and only the front landed", back == wsrc[..8]);

    // ---- the refusals ----
    //
    // The one flag bit Linux defines is "none"; any other word is `EINVAL`, on
    // both halves. A count past the limit gets the same answer, before anything
    // is walked.
    r.check_errno(
        "a nonzero flags word is EINVAL on the read",
        readv_flags(me, &local, &remote, 1).unwrap_err(),
        EINVAL,
    );
    r.check_errno(
        "a nonzero flags word is EINVAL on the write",
        writev_flags(me, &local_w, &remote_w, 1).unwrap_err(),
        EINVAL,
    );
    r.check_errno(
        "an iovec count past 1024 is EINVAL",
        vm(
            libc::SYS_process_vm_readv,
            me,
            local.as_ptr(),
            1025,
            remote.as_ptr(),
            1,
            0,
        )
        .unwrap_err(),
        EINVAL,
    );

    // A null iovec array names no iovecs: `EFAULT`, not a silent zero.
    r.check_errno(
        "a null local iovec array is EFAULT",
        vm(
            libc::SYS_process_vm_readv,
            me,
            ptr::null(),
            1,
            remote.as_ptr(),
            1,
            0,
        )
        .unwrap_err(),
        EFAULT,
    );

    // A pid with no task behind it, on this machine's own table.
    r.check_errno(
        "a pid past the task table names no task",
        readv(9999, &local, &remote).unwrap_err(),
        ESRCH,
    );

    // A remote range with no page behind it: an address that was mapped and is
    // not any more, so the walk finds no entry at all.
    let mut hole = [0u8; 4];
    let unmapped = [iov(&mut hole)];
    let gone = freed_page();
    r.check(
        "a page could be mapped and freed for the no-entry case",
        gone > 0,
    );
    if gone > 0 {
        let no_page = [iov_at(gone, 20)];
        r.check_errno(
            "a remote range with no page behind it is EFAULT on the read",
            readv(me, &unmapped, &no_page).unwrap_err(),
            EFAULT,
        );
        r.check_errno(
            "and on the write",
            writev(me, &unmapped, &no_page).unwrap_err(),
            EFAULT,
        );
    }

    // The other refusal, and the one that is a *hole* rather than a formality:
    // a page that is mapped but not reachable from ring 3. A walk that asked
    // only for a present entry would read it - and the supervisor half of every
    // address space has the same shape.
    let guarded_at = unreachable_page();
    r.check("a PROT_NONE page could be made to refuse", guarded_at > 0);
    if guarded_at > 0 {
        let guarded = [iov_at(guarded_at, 20)];
        r.check_errno(
            "a remote PROT_NONE page is EFAULT on the read",
            readv(me, &unmapped, &guarded).unwrap_err(),
            EFAULT,
        );
        r.check_errno(
            "and on the write",
            writev(me, &unmapped, &guarded).unwrap_err(),
            EFAULT,
        );
        // SAFETY: the mapping this function made, unmapped once.
        unsafe {
            libc::munmap(guarded_at as *mut c_void, 4096);
        }
    }

    // ---- and the transfer that crosses two address spaces ----
    cross_process(&mut r);

    if r.failed {
        println!("t-process-vm-libc: FAILED");
        exit(1);
    }
    println!("t-process-vm-libc: all passed");
}
