//! t-clone - one test for clone, the call `fork` and `pthread_create` are both
//! built on.
//!
//! `fork` is `clone` with nothing in its flag word but the signal that announces
//! a child, so the process-shaped half has to behave exactly like `fork`: 0 in
//! the child, the child's pid in the parent, and a private copy of memory. What
//! is new is the flag word, and the test spends
//! most of its checks there - the pieces of Linux's contract that a kernel can
//! plausibly get wrong:
//!
//! * an unknown flag bit is `EINVAL`, because a kernel that ignored what it did
//!   not understand would hand back a child unlike the one that was asked for;
//! * `CLONE_THREAD` implies `CLONE_SIGHAND`, which implies `CLONE_VM` - a thread
//!   shares its creator's handlers and memory, and a request that says otherwise
//!   is not a request this kernel can honour;
//! * a tid flag with nowhere to put the tid is `EINVAL`, not a silent no-op;
//! * `CLONE_CHILD_SETTID` really does park the tid where the child can read it,
//!   which is how a thread learns its own id before it does anything else;
//! * a real thread shares its creator's memory: it writes a word the caller can
//!   see, which is the whole difference between a thread and a fork, and it
//!   answers its own `gettid` while `getpid` stays the process's.
//!
//! The spawn shape - `CLONE_VM|CLONE_VFORK`, which is what a libc's
//! `posix_spawn` builds - is the one request this program leaves to `t-stdspawn`
//! instead of making itself; the reason is in the migration note below.
//!
//! # The migration
//!
//! `clone` is a call `rustix` does not bind at all - the flag word is the whole
//! interface and every typed wrapper that exists (musl's `clone` takes a
//! function pointer, `fork` and `pthread_create` are its two named shapes) hides
//! it - so the raw door is `libc::syscall(libc::SYS_clone, ..)`, which is the
//! call the program is about, with the number from libc's per-machine table (56
//! on the first machine, 220 on the second).
//!
//! The one machine fact in that door is the *order* of the last two words, and
//! it is why [`clone_raw`] exists rather than the call being spelled inline:
//! Linux's x86_64 `sys_clone` takes `child_tidptr` fourth and `tls` fifth, and
//! its arm64 spelling takes `tls` fourth and `child_tidptr` fifth - the
//! difference the kernel calls `CLONE_BACKWARDS`. A program that wrote either
//! order down would pin a different call on one machine than the other - the
//! shape `t-stat-buffer` and `t-ptrace` already handle for their own per-ABI
//! structures.
//!
//! This door is also how the migration found a kernel bug rather than a
//! test-side one: the second machine's handler read the *first* machine's
//! spelling, so this program's `CLONE_CHILD_SETTID` claim was refused `EINVAL`
//! there (the tid pointer sat in the slot nothing looks at), and the thread it
//! then made was given the tid word's address as its thread pointer and faulted
//! at `pc 0x3` on its way out. The fix is one swap in the dispatch
//! (`syscall::linux::mod`'s `clone_arguments`), recorded in `NOTES.md` §15.11;
//! `clone_raw` is unchanged by it, because the order it sends is the ABI's.
//!
//! The flag words come from `libc::CLONE_*` rather than from this repository's
//! `nr.rs`, for the same reason the numbers come from `libc::SYS_*`: it is a
//! maintained per-machine table this build does not have to keep a second copy
//! of, and the no-std suite's copy of it is what this migration retires.
//!
//! The shared cell is a `static AtomicI32` rather than the no-std version's
//! `static mut SHARED`: the 2024 edition refuses to reference a `static mut`,
//! and the claim - the child's store stays in the child - is about which copy of
//! the page the write lands in, which an atomic states just as plainly. Each
//! child leaves through the raw `SYS_exit_group` and the parent reaps through
//! `rustix`'s `waitpid` (`wait4` with a null `rusage`), whose `WaitStatus` is
//! where the no-std `WIFEXITED`/`WEXITSTATUS` calls went. The raw exit is
//! load-bearing: it is the syscall a process ends through, and it cannot flush
//! the child's inherited copy of the parent's buffered stdout.
//!
//! The last claim is where the migration *removes* scaffolding rather than
//! translating it. The no-std version built its own thread: it mapped a stack
//! with `mmap`, wrote the entry point at its top, and called `clone` through a
//! hand-written `global_asm` stub - so its "a stack was mapped for the thread"
//! and "the thread tid was parked where the caller named" checks were claims
//! about `lib.rs`'s scaffold, not about the kernel. A `std` program has a
//! maintained library for exactly that, so the thread is `std::thread::spawn`
//! and the two scaffold claims retire with the scaffold. What they were really
//! reaching for - `CLONE_CHILD_SETTID` parking a tid - is `t-set-tid-address`'s
//! subject on the raw side, and is exercised here on the library's side, since
//! musl's `pthread_create` uses the same flag word for every thread it makes.
//!
//! The spawn shape retires for the same kind of reason, and it is the one claim
//! this file hands to another program: `CLONE_VM` means the child shares the
//! caller's address space, so a child made by a raw syscall resumes without a
//! frame of its own to return on - the entry-point shuffle a libc's `clone`
//! wrapper exists to do. A `std` program therefore reaches that shape through
//! the library (`posix_spawn`, pinned end to end by `t-stdspawn`, whose header
//! names this flag word) rather than by hand, and the no-std version's
//! `global_asm` exec stub is not something this file should rebuild.
//!
//! Two claims are *designed divergences* and are named where they are made,
//! both measured against this host's own kernel rather than assumed: the
//! unknown-flag bit answers `EINVAL` here where a host Linux has no unknown-bit
//! check at all and starts the child (measured: 2^32, 2^44 and `CLONE_PTRACE`
//! each answer a pid, 7.2.9), and a null tid pointer is refused here where a
//! host Linux starts the child and skips the write. This kernel's answer is the
//! claim; a host's own answer is accepted only in a host run
//! (`mpos_user_tests::host_run`) and the difference is printed rather than
//! hidden.
//!
//! The namespace kinds are the third thing the flag word decides, and the
//! answer is settled *before* anything is created: the two kinds this kernel
//! serves - `CLONE_NEWUTS` and `CLONE_NEWNS` (§6.3's ladder, MNT at step 1) -
//! give the child a *copy* of the caller's names or mount table, while every
//! other kind is refused whole with `EOPNOTSUPP` rather than handed a child
//! that shares what its flags said it got its own of. The word is the scheduler's
//! to answer (`abi::clone_namespace_verdict`), which is why the syscall layer
//! must pass these bits through rather than read them as unknown.
//!
//! A third divergence is *avoided* rather than gated, because a request that
//! means different things on the two machines cannot be one claim: the no-std
//! version cloned with a literal zero flag word, and an empty word means "no
//! signal announces the child", which Linux answers by auto-reaping it (`wait4`
//! is `ECHILD`; measured). Every process-shaped clone here therefore names
//! `SIGCHLD` - the word `fork` is actually built from - so that the reaping and
//! exit-code claims below are about the same request on both machines.

use mpos_user_tests::Report;
use rustix::process::{Pid, WaitOptions, WaitStatus, getpid, waitpid};
use rustix::thread::gettid;
use std::sync::atomic::{AtomicI32, Ordering};

/// The variable the clone child updates and the caller must not see change.
static SHARED: AtomicI32 = AtomicI32::new(5);

/// The code the process-shaped clone child leaves with.
const CHILD_CODE: i32 = 33;

/// How long the caller spins before giving up on a thread that should have run.
/// A test that waits for ever is worse than a test that fails.
const SPIN_LIMIT: usize = 200_000;

/// The refusal-probe label, built without pulling in `format!` machinery the
/// file otherwise has no use for.
fn alloc_label(kind: &str) -> String {
    let mut s = String::from("clone(CLONE_NEW");
    s.push_str(kind);
    s.push_str(") is refused whole");
    s
}

/// What a real thread writes, and what it answers for its own identity. Memory
/// is shared with a thread, so the caller reads these back.
static THREAD_WROTE: AtomicI32 = AtomicI32::new(0);
static THREAD_TID: AtomicI32 = AtomicI32::new(0);
static THREAD_PID: AtomicI32 = AtomicI32::new(0);

/// `clone(flags, stack, parent_tid, child_tid, tls)`, with the two machines'
/// argument orders reconciled here and nowhere else - see the module comment.
///
/// The return is libc's: the child's pid to the parent, `0` in the child, and
/// `-1` with the refusal in `errno` when the word is not one the kernel serves.
/// No call here is passed a stack, so no child this program starts returns on
/// one of its own - every child it makes is process-shaped, which is to say a
/// fork that comes back from this call the way its creator did.
unsafe fn clone_raw(
    flags: usize,
    stack: usize,
    parent_tid: *mut i32,
    child_tid: *mut i32,
    tls: usize,
) -> isize {
    let flags = flags as libc::c_long;
    let stack = stack as libc::c_long;
    let parent_tid = parent_tid as libc::c_long;
    let child_tid = child_tid as libc::c_long;
    let tls = tls as libc::c_long;
    #[cfg(target_arch = "x86_64")]
    let ret = unsafe { libc::syscall(libc::SYS_clone, flags, stack, parent_tid, child_tid, tls) };
    #[cfg(target_arch = "aarch64")]
    let ret = unsafe { libc::syscall(libc::SYS_clone, flags, stack, parent_tid, tls, child_tid) };
    ret as isize
}

/// The door returns `-1` and leaves the refusal in `errno`; the claim is about
/// what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// A refusal probe: a word this kernel does not serve answers `-1` with the
/// refusal in `errno`, and a word it *wrongly* serves hands back a child that
/// would otherwise run the rest of this program a second time. The child leaves
/// at once, so a probe measures an answer rather than spawning a second test.
/// That guard is also what keeps a host run from running the program twice,
/// because Linux really does hand back a child for two of these words (measured;
/// see the notes where they are used).
fn probe(flags: usize) -> isize {
    let ret = unsafe { clone_raw(flags, 0, core::ptr::null_mut(), core::ptr::null_mut(), 0) };
    if ret == 0 {
        leave(0);
    }
    ret
}

/// Leave through `exit_group`: the code is the claim, and a forked `std` child's
/// inherited copy of the parent's stdout must not be flushed.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        // The door does not return; `_exit` is the maintained fallback.
        libc::_exit(code)
    }
}

/// The blocking `wait4(pid)`: `Some(status)` when that child was the one
/// collected, `None` when the call answered anything else.
fn collect(pid: isize) -> Option<WaitStatus> {
    let want = Pid::from_raw(pid as i32);
    match waitpid(want, WaitOptions::empty()) {
        Ok(Some((got, status))) if Some(got) == want => Some(status),
        _ => None,
    }
}

fn main() {
    println!("t-clone");
    let mut r = Report::new();
    let null = core::ptr::null_mut::<i32>();

    // An unknown flag bit names something this kernel does not implement. The
    // bit has to be outside *both* vocabularies: every bit below 2^32 is either
    // the exit signal or a real Linux flag - bit 30, the obvious choice, is
    // `CLONE_NEWNET`, which this kernel now *knows* and refuses by name
    // (`EOPNOTSUPP`, pinned by the namespace checks below) - so the bit named
    // here is 2^44, above every flag either kernel defines.
    //
    // This is a *designed divergence*, not Linux's answer: measured on this
    // host (7.2.9, `.workbuddy/probe_clone_unknown.c`), Linux has no unknown-bit
    // check at all - 2^32, 2^44 and `CLONE_PTRACE` each start a child - so a
    // host run's child is accepted and the difference is printed rather than
    // hidden.
    let ret = probe(1 << 44);
    if mpos_user_tests::host_run() && !refused(ret, libc::EINVAL) {
        println!(
            "note: the host starts the child for a bit it does not know - Linux's answer, not mpos's EINVAL"
        );
    }
    r.check(
        "an unknown flag bit is EINVAL",
        refused(ret, libc::EINVAL) || mpos_user_tests::host_run(),
    );

    // CLONE_THREAD without the two flags it depends on.
    let ret = probe(libc::CLONE_THREAD as usize);
    r.check(
        "CLONE_THREAD without CLONE_SIGHAND|CLONE_VM is EINVAL",
        refused(ret, libc::EINVAL),
    );

    // CLONE_SIGHAND without CLONE_VM.
    let ret = probe(libc::CLONE_SIGHAND as usize);
    r.check(
        "CLONE_SIGHAND without CLONE_VM is EINVAL",
        refused(ret, libc::EINVAL),
    );

    // A tid flag with a null destination. This one is a *designed divergence*
    // and the only place this program pins a stricter answer than Linux's:
    // measured on this host, `clone(CLONE_PARENT_SETTID, .., NULL, ..)` starts
    // the child and simply does not write a tid, where this kernel refuses the
    // request outright - the "not a silent no-op" half of the claim below is
    // this kernel's, and the host's own answer is accepted only in a host run.
    let ret = probe(libc::CLONE_PARENT_SETTID as usize);
    if mpos_user_tests::host_run() && !refused(ret, libc::EINVAL) {
        println!("note: the host starts the child and skips the write - not mpos's EINVAL");
    }
    r.check(
        "CLONE_PARENT_SETTID with a null pointer is EINVAL",
        refused(ret, libc::EINVAL) || mpos_user_tests::host_run(),
    );

    // The namespace kinds, refused or served by whether a copy exists to hand
    // over (§6.3's ladder). PID/NET/IPC answer `EOPNOTSUPP` — "present, cannot
    // do that" — before anything is created; an unserved bit must not quietly
    // produce a child that shares what its flags said it got its own of. (On a
    // host run the word may answer `EPERM` or `EINVAL` by the host's own policy
    // before it reaches any kind, so the claim is about *this* machine's answer
    // and the host line says which it was.)
    //
    // The words come from `libc::CLONE_NEW*` like every other flag in this file,
    // and that is a correction rather than a style: the first draft wrote three
    // literals and got two of them wrong — the one it called `CLONE_NEWPID` was
    // `0x0008_0000`, which is `CLONE_SETTLS`, and the one it called
    // `CLONE_NEWIPC` was `0x2000_0000`, which is `CLONE_NEWPID`. Measured, not
    // guessed: the `CLONE_SETTLS` word *started a child* instead of refusing
    // one, and that child ran the rest of this program with whatever the
    // register happened to hold as its thread pointer. A hand-written flag table
    // is a second source of truth for a number a maintained one already has.
    let names: &[(&str, usize)] = &[
        ("PID", libc::CLONE_NEWPID as usize),
        ("NET", libc::CLONE_NEWNET as usize),
        ("IPC", libc::CLONE_NEWIPC as usize),
    ];
    for (name, bit) in names {
        let ret = probe(libc::SIGCHLD as usize | bit);
        let ok = if mpos_user_tests::host_run() {
            ret < 0 // some refusal; the host's own namespace policy decides the number
        } else {
            refused(ret, 95) // EOPNOTSUPP
        };
        let label = alloc_label(name);
        r.check(&label, ok);
    }

    // `CLONE_NEWUTS` is served: the child starts in a *copy* of its parent's
    // names (Linux's `copy_utsname` on this flag), so a rename inside never
    // reaches out. The child renames, reads its own names back, and reports
    // what it saw while the parent compares against its own view.
    //
    // The report rides a *pipe*, and that is a correction rather than a style:
    // this clone is process-shaped, so the child has its own copy of the
    // address space — a `static` the child stores into is the *child's* copy,
    // and the parent reads the one it wrote, for ever false. Measured: the
    // first draft used `static UTS_REPORT: AtomicBool`, and this claim failed
    // while the child's own read-back said the rename had landed.
    // `t-ns-mnt`/`t-ns-uts` report a child's observation through a pipe for
    // exactly this reason.
    let mut reported = [0u8; 32];
    let mut n = 0usize;
    let report = rustix::pipe::pipe();
    if let Ok((tail, head)) = report {
        let uts_pid = unsafe {
            // `CLONE_NEWUTS` is `0x0400_0000` on **both** machines — the
            // namespace bits are asm-generic's and arm64 overrides none of them
            // (checked against the installed headers), so this word means the
            // same request on x86_64 and aarch64 alike.
            clone_raw(libc::SIGCHLD as usize | 0x0400_0000, 0, null, null, 0)
        };
        if uts_pid == 0 {
            // The rename through the raw door — `sethostname(name, len)` —
            // because a child of a `std` process must not run anything that
            // forks or flushes on its way out. It then reads the name back
            // through `uname(2)`'s raw word and reports both halves in one word
            // down the pipe.
            let name = b"cloned-inside\0";
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_sethostname,
                    name.as_ptr() as libc::c_long,
                    (name.len() - 1) as libc::c_long,
                )
            };
            let mut uts = [0u8; 333]; // Linux's `struct old_utsname`: 65*5+8-ish; uname writes 65-byte fields
            let urc = unsafe { libc::syscall(libc::SYS_uname, uts.as_mut_ptr() as libc::c_long) };
            // nodename is the second field, at offset 65, NUL-terminated.
            let node = &uts[65..130];
            let node_ok =
                urc == 0 && rc == 0 && node.starts_with(b"cloned-inside") && node[13] == 0;
            let msg: &[u8] = if node_ok { b"renamed\n" } else { b"!renamed\n" };
            let _ = rustix::io::write(&head, msg);
            drop(head);
            leave(CHILD_CODE);
        }
        r.check("clone(CLONE_NEWUTS) creates the child", uts_pid > 0);
        if uts_pid <= 0 {
            println!("  clone answered {uts_pid}");
        }
        // The parent's own end of the write side goes before the drain, or EOF
        // never comes: the child holds a copy of it too, and the drain waits on
        // every writer (`t-ns-uts`'s lesson, from the same shape).
        drop(head);
        n = rustix::io::read(&tail, &mut reported).unwrap_or(0);
        if uts_pid > 0 {
            let _ = collect(uts_pid as isize);
        }
    } else {
        r.check("the report pipe is made", false);
    }
    r.check(
        "the NEWUTS child renamed only its own copy",
        reported[..n].starts_with(b"renamed"),
    );
    let mut pbuf = [0u8; 333];
    let prc = unsafe { libc::syscall(libc::SYS_uname, pbuf.as_mut_ptr() as libc::c_long) };
    r.check(
        "and the parent kept its own name",
        prc == 0 && !pbuf[65..130].starts_with(b"cloned-inside"),
    );

    // A process-shaped clone is fork: the child gets 0 and its own memory. The
    // flag word is the exit signal and nothing else - `SIGCHLD` is exactly what a
    // `fork` is built from, the low byte of the word carrying the signal that
    // announces the child rather than a sharing flag.
    //
    // The no-std version wrote a literal zero here, and that is the one place
    // the two machines part on this request rather than on its flags: measured
    // on this host, Linux *auto-reaps* the child of an empty flag word - `wait4`
    // answers `ECHILD` - where this kernel keeps it waitable and accepts the
    // byte without acting on it. A reaping check written against the empty word
    // would therefore pin one machine's answer, so this uses the word `fork`
    // actually is; nothing is lost, since the request is unchanged.
    let pid = unsafe { clone_raw(libc::SIGCHLD as usize, 0, null, null, 0) };
    if pid == 0 {
        // The child: write what the parent must not see, and leave. Nothing
        // printed here would be judged against this process's own report.
        SHARED.store(88, Ordering::Relaxed);
        leave(CHILD_CODE);
    }
    r.check(
        "a clone that names only the exit signal returns the child's pid",
        pid > 0,
    );
    if pid > 0 {
        let status = collect(pid);
        r.check("the clone child is reaped by its pid", status.is_some());
        r.check(
            "the clone child's exit code survives",
            status.and_then(WaitStatus::exit_status) == Some(CHILD_CODE),
        );
        r.check(
            "the clone child got its own copy of memory",
            SHARED.load(Ordering::Relaxed) == 5,
        );
    }

    // CLONE_CHILD_SETTID: the kernel writes the child's tid into the child's own
    // memory before the child runs, which is how a thread learns its own id. The
    // slot is the caller's own, so the write is observable twice: the child sees
    // its tid in its copy, and the caller's copy is left alone. `SIGCHLD` rides
    // along for the same reason it does above - the child has to be waitable for
    // its reading of the slot to be judged.
    let mut slot: i32 = 0;
    let pid = unsafe {
        clone_raw(
            (libc::CLONE_CHILD_SETTID | libc::SIGCHLD) as usize,
            0,
            null,
            &raw mut slot,
            0,
        )
    };
    if pid == 0 {
        leave(if slot > 0 { 0 } else { 1 });
    }
    r.check("CLONE_CHILD_SETTID clone returns the child's pid", pid > 0);
    if pid > 0 {
        let status = collect(pid);
        r.check("the CLONE_CHILD_SETTID child is reaped", status.is_some());
        r.check(
            "the child saw its own tid in the slot it named",
            status.and_then(WaitStatus::exit_status) == Some(0),
        );
        r.check("the write stayed in the child's copy of memory", slot == 0);
    }

    // The spawn shape - `CLONE_VM|CLONE_VFORK|SIGCHLD` with no `CLONE_THREAD`,
    // which is what a libc's `posix_spawn` builds - is **not** pinned here, and
    // the reason is the flag word itself rather than a gap. Under `CLONE_VM` the
    // child shares the caller's address space and therefore the caller's stack,
    // and a child that resumes inside the libc `syscall` wrapper it was called
    // from has no frame of its own to return on: making that shape by hand needs
    // the entry-point shuffle a libc's `clone` wrapper does in assembly. So a
    // `std` program does not make this call directly - it calls `posix_spawn` -
    // and the shape is pinned where the call is really made, in `t-stdspawn`,
    // whose own header names this exact flag word. (The no-std version pinned it
    // here with a hand-written thread stub, which is the scaffolding this
    // migration retires; the flags around it are named by the checks above.)

    // A real thread shares its creator's memory and runs on a stack of its own.
    // The library makes both; what this pins is what the kernel owes them - one
    // address space between the two, and an identity of the thread's own.
    let pid = getpid().as_raw_nonzero().get();
    let handle = std::thread::spawn(|| {
        THREAD_TID.store(gettid().as_raw_nonzero().get(), Ordering::Relaxed);
        THREAD_PID.store(getpid().as_raw_nonzero().get(), Ordering::Relaxed);
        // The order is the synchronization: the caller spins on `THREAD_WROTE`,
        // so it has to be the *last* word written.
        THREAD_WROTE.store(42, Ordering::Release);
    });
    let mut spins = 0usize;
    while THREAD_WROTE.load(Ordering::Acquire) == 0 && spins < SPIN_LIMIT {
        spins += 1;
        std::thread::yield_now();
    }
    let tid = THREAD_TID.load(Ordering::Relaxed);
    r.check(
        "the thread wrote into the caller's memory",
        THREAD_WROTE.load(Ordering::Acquire) == 42,
    );
    r.check(
        "the thread knows its own tid, which is not the process's pid",
        tid > 0 && tid != pid,
    );
    r.check(
        "the thread answers the process's pid",
        THREAD_PID.load(Ordering::Relaxed) == pid,
    );
    r.check("the thread is joined", handle.join().is_ok());

    r.finish()
}
