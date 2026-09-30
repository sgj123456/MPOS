//! t-posix - the POSIX completion set's process half, on the machine.
//!
//! Pinned here, per each call's man page: a fresh process is root in all
//! three identity words, and the `setuid` family follows Linux's rules -
//! root may set any id (the dance `sudo` performs: lower the effective id,
//! raise it back through the saved one), while a caller that has lowered
//! itself may only reach ids it already holds and sees EPERM for the rest;
//! `setreuid`'s saved id follows the new effective one for root and never
//! moves for anyone else, root's capability state is everything permitted and
//! nothing inheritable, `getpriority`'s raw syscall answers in the kernel's
//! own 40..1 encoding (nice 0 reads as 20), the one policy is SCHED_OTHER
//! with the priority bounds the man page's table spells, the persona query
//! answers the default, seccomp's filter mode refuses a request that names no
//! program before it reads one, while its availability probe answers, and the
//! namespace pair refuses with
//! EOPNOTSUPP/EINVAL for the flags it cannot keep, and the memory advice
//! family answers for a range that is mapped.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// One `__user_cap_data_struct`: effective, permitted and inheritable, one
/// `u32` each - which is how `<linux/capability.h>` spells it, and the order
/// the kernel writes them in.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapRecord {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The version 3 data: *two* of those records, laid end to end. A struct of
/// three two-word fields would describe a different layout entirely - one the
/// kernel does not write - and would read the first record's inheritable
/// word as the second record's effective one.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    records: [CapRecord; 2],
}

impl CapData {
    fn every(&self, field: fn(&CapRecord) -> u32, want: u32) -> bool {
        self.records.iter().all(|record| field(record) == want)
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-posix\n");
    let mut failed = false;

    // Identity: a fresh process is root three words over - real, effective
    // and saved, user and group alike.
    let mut ids = [1u64; 3];
    lin::check_eq(
        &mut failed,
        b"getresuid succeeds",
        lin::getresuid(&mut ids),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the real, effective and saved uids are root's",
        ids == [0, 0, 0],
    );
    let mut gids = [1u64; 3];
    lin::check_eq(
        &mut failed,
        b"getresgid succeeds",
        lin::getresgid(&mut gids),
        0,
    );
    lin::check_true(&mut failed, b"the gids are root's", gids == [0, 0, 0]);

    // The res spelling, as the privilege dance sudo actually performs: root
    // lowers its effective id, reads its own three words back, and raises
    // itself again through the saved one - the word that never moved.
    lin::check_eq(
        &mut failed,
        b"setresuid(-1, 1, -1) succeeds for root",
        lin::setresuid(-1, 1, -1),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getresuid still succeeds",
        lin::getresuid(&mut ids),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the dance reads back real 0, effective 1, saved 0",
        ids == [0, 1, 0],
    );
    lin::check_eq(
        &mut failed,
        b"geteuid reads the lowered id",
        lin::geteuid(),
        1,
    );
    lin::check_eq(
        &mut failed,
        b"getuid still reads the real one",
        lin::getuid(),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a lowered caller cannot set an id it does not hold",
        lin::setresuid(2, -1, -1),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"the way back through the saved id succeeds",
        lin::setresuid(-1, 0, -1),
        0,
    );
    lin::check_eq(&mut failed, b"geteuid reads root again", lin::geteuid(), 0);

    // The re spelling: for root the saved id follows the new effective one,
    // and becoming what one already is always succeeds.
    lin::check_eq(
        &mut failed,
        b"setreuid(0, 0) succeeds",
        lin::setreuid(0, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setreuid(-1, 0) succeeds",
        lin::setreuid(-1, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setreuid(0, 100) succeeds for root",
        lin::setreuid(0, 100),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getresuid reads the pair back",
        lin::getresuid(&mut ids),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the saved id followed the new effective one",
        ids == [0, 100, 100],
    );
    // setuid for a caller that has lowered itself moves only the effective
    // word, to the real or the saved one - which is what the way back is.
    lin::check_eq(
        &mut failed,
        b"setuid(0) raises the effective id back",
        lin::setuid(0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getresuid reads the raise back",
        lin::getresuid(&mut ids),
        0,
    );
    lin::check_true(
        &mut failed,
        b"an unprivileged setuid moved only the effective word",
        ids == [0, 0, 100],
    );
    lin::check_eq(
        &mut failed,
        b"the saved word is root's to reset",
        lin::setresuid(-1, -1, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getresuid reads the reset",
        lin::getresuid(&mut ids),
        0,
    );
    lin::check_true(
        &mut failed,
        b"all three words are root's again",
        ids == [0, 0, 0],
    );

    // Capabilities: root's state is everything permitted and effective,
    // nothing inheritable, in the version 3 shape of two records.
    let mut header = lin::CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = CapData {
        records: [CapRecord {
            effective: 0,
            permitted: 0,
            inheritable: 0xdead_beef,
        }; 2],
    };
    lin::check_eq(
        &mut failed,
        b"capget v3 succeeds",
        lin::capget(&mut header, &raw mut data as *mut u8),
        0,
    );
    lin::check_true(
        &mut failed,
        b"every capability is effective",
        data.every(|r| r.effective, u32::MAX),
    );
    lin::check_true(
        &mut failed,
        b"every capability is permitted",
        data.every(|r| r.permitted, u32::MAX),
    );
    lin::check_true(
        &mut failed,
        b"nothing is inheritable",
        data.every(|r| r.inheritable, 0),
    );
    header.version = 0xdead_beef;
    lin::check_eq(
        &mut failed,
        b"capget with an unknown version answers EINVAL",
        lin::capget(&mut header, &raw mut data as *mut u8),
        lin::EINVAL,
    );

    // Priority: the raw syscall's encoding, the which validation, and the
    // range the man page allows root all of.
    lin::check_eq(
        &mut failed,
        b"getpriority answers 20 (nice 0, kernel encoding)",
        lin::getpriority(0, 0),
        20,
    );
    lin::check_eq(
        &mut failed,
        b"setpriority 5 succeeds",
        lin::setpriority(0, 0, 5),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setpriority out of range answers EINVAL",
        lin::setpriority(0, 0, 99),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"setpriority with a bad which answers EINVAL",
        lin::setpriority(3, 0, 0),
        lin::EINVAL,
    );

    // The one policy, and the bounds its table spells.
    lin::check_eq(
        &mut failed,
        b"sched_getscheduler answers SCHED_OTHER",
        lin::sched_getscheduler(0),
        0,
    );
    let mut prio = -1i32;
    lin::check_eq(
        &mut failed,
        b"sched_getparam succeeds",
        lin::sched_getparam(0, &raw mut prio),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the SCHED_OTHER priority is zero",
        prio as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"SCHED_OTHER's max priority is zero",
        lin::sched_get_priority_max(0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"SCHED_FIFO's max priority is 99",
        lin::sched_get_priority_max(1),
        99,
    );
    lin::check_eq(
        &mut failed,
        b"SCHED_FIFO's min priority is 1",
        lin::sched_get_priority_min(1),
        1,
    );
    lin::check_eq(
        &mut failed,
        b"an unknown policy answers EINVAL",
        lin::sched_get_priority_max(99),
        lin::EINVAL,
    );

    // sched_setscheduler/sched_setparam validate the SCHED_OTHER priority,
    // which the policy's band fixes at zero. Measured on a real host: passing
    // a nonzero priority for SCHED_OTHER is EINVAL, and this kernel used to
    // answer 0 while dropping it - the accept-and-drop this pass removes.
    let prio_zero = 0i32;
    let prio_five = 5i32;
    let prio_seven = 7i32;
    lin::check_eq(
        &mut failed,
        b"sched_setscheduler(SCHED_OTHER, prio 0) succeeds",
        lin::sched_setscheduler(0, 0, &raw const prio_zero),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"sched_setscheduler(SCHED_OTHER, prio 5) is EINVAL",
        lin::sched_setscheduler(0, 0, &raw const prio_five),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"sched_setparam(prio 0) succeeds",
        lin::sched_setparam(0, &raw const prio_zero),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"sched_setparam(prio 7) is EINVAL",
        lin::sched_setparam(0, &raw const prio_seven),
        lin::EINVAL,
    );
    // The task is still at the priority it was: a refused set left nothing in
    // force, so getparam answers 0 after the EINVAL above.
    prio = -1;
    lin::check_eq(
        &mut failed,
        b"sched_getparam still answers 0 after the refused sets",
        lin::sched_getparam(0, &raw mut prio),
        0,
    );
    lin::check_eq(&mut failed, b"the priority is 0", prio as isize, 0);

    // The machine: one core, node zero, the default persona.
    let mut cpu = 99u32;
    let mut node = 99u32;
    lin::check_eq(
        &mut failed,
        b"getcpu succeeds",
        lin::getcpu(&raw mut cpu, &raw mut node),
        0,
    );
    lin::check_true(&mut failed, b"the cpu is 0", cpu == 0);
    lin::check_true(&mut failed, b"the node is 0", node == 0);
    lin::check_eq(
        &mut failed,
        b"the persona query answers the default",
        lin::personality(0xffff_ffff),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setting the default persona succeeds",
        lin::personality(0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"an unknown persona answers EINVAL",
        lin::personality(0x1000),
        lin::EINVAL,
    );
    // vhangup: this program never acquired a controlling terminal with
    // TIOCSCTTY, so `current->signal->tty` is none and Linux's
    // `SYSCALL_DEFINE0(vhangup)` answers `ENOTTY`, not the `0` it reserves for
    // hanging up a terminal that exists. The old pin asserted `0` — that was
    // mpos's unconditional success, not Linux's answer for a caller with no
    // terminal to hang up, so it is corrected here to the measured `ENOTTY`.
    lin::check_eq(
        &mut failed,
        b"vhangup answers ENOTTY with no controlling terminal",
        lin::vhangup(),
        lin::ENOTTY,
    );

    // seccomp(2), as this kernel implements it (PLAN 8.2): STRICT latches
    // for real, FILTER installs a program, and the availability probe answers
    // which actions this kernel actually implements. What is pinned for
    // FILTER here is only that the program is *read before anything is
    // decided*: a null one is `EFAULT` rather than a success that confined
    // nobody. (A malformed program is `EINVAL`, and the filters themselves are
    // argued in `seccomp.rs`'s host tests, where a BPF program can be run
    // against made-up syscall numbers.) The strict checks are done
    // non-destructively - once latched, this program's next `getpid` is a
    // fatal call - and the latch itself is proved in a child, whose death
    // by SIGKILL is the whole of what Linux promises a strict thread that
    // reaches for anything else. The eight actions and each one's answer are
    // `t-seccomp`'s; this line pins only that a word naming no action at all
    // is refused rather than granted.
    lin::check_eq(
        &mut failed,
        b"seccomp with an unknown op answers EINVAL",
        lin::seccomp(99, 0, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"strict mode with stray flags answers EINVAL before latching",
        lin::seccomp(0, 1, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"seccomp filter mode with no program answers EFAULT",
        lin::seccomp(1, 0, 0),
        lin::EFAULT,
    );
    // 99 is nobody's action: an unknown one would kill the process that
    // relied on it, which is why a caller asks first and why the answer has
    // to be a refusal. The caller's own word is left alone - Linux's return
    // value carries the answer, and there is nothing to write back.
    let mut avail = 99u32;
    lin::check_eq(
        &mut failed,
        b"the availability probe refuses an action this kernel does not implement",
        lin::seccomp(2, 0, &raw mut avail as usize),
        lin::EOPNOTSUPP,
    );
    lin::check_true(
        &mut failed,
        b"a refused probe leaves the caller's word alone",
        avail == 99,
    );
    {
        let kid = lin::fork();
        if kid == 0 {
            // The child latches strict, then reaches for a syscall the
            // whitelist does not carry. It should not return; the exit
            // below is the failure path, reached only if the kernel let
            // the forbidden call answer like a normal one.
            let strict = lin::seccomp(0, 0, 0);
            let probe = lin::getpid();
            if strict == 0 && probe == lin::ENOSYS {
                lin::exit(0);
            }
            lin::exit(1);
        }
        let mut status = 0i32;
        let got = lin::waitpid(kid as usize, &mut status, 0);
        lin::check_true(
            &mut failed,
            b"the strict child was reaped",
            got == kid as isize,
        );
        lin::check_true(
            &mut failed,
            b"the strict child died by SIGKILL on its first forbidden call",
            status & 0x7f == 9,
        );
    }

    // The namespace pair: nothing to unshare, nothing to join. Every number
    // below is `include/uapi/linux/sched.h`'s, and each was measured on a real
    // kernel before being written down - which is what caught this block
    // asserting `0x1_0000` was `CLONE_NEWNS`: that is `CLONE_THREAD`, and Linux
    // answers 0 for it. The mount namespace is `0x2_0000`, and a program asking
    // for it was being told `EINVAL` as though the bit were undefined.
    lin::check_eq(&mut failed, b"unshare(0) succeeds", lin::unshare(0), 0);
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_NEWNS answers EOPNOTSUPP",
        lin::unshare(0x0002_0000),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_NEWTIME answers EOPNOTSUPP",
        lin::unshare(0x0000_0080),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_NEWUTS answers EOPNOTSUPP",
        lin::unshare(0x0400_0000),
        lin::EOPNOTSUPP,
    );
    // The thread-group flags: a process here is its own only thread and owns
    // all six already, so there is nothing to split off and the answer is the
    // no-op Linux gives a single-threaded caller.
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_THREAD is the no-op it is on Linux",
        lin::unshare(0x0001_0000),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_FILES|CLONE_FS is a no-op",
        lin::unshare(0x0000_0600),
        0,
    );
    // `CLONE_SETTLS` is not a namespace and not something `unshare` can do:
    // Linux answers EINVAL for it, and this is the value the file used to spell
    // `CLONE_NEWTIME`.
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_SETTLS answers EINVAL",
        lin::unshare(0x0008_0000),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_PIDFD answers EINVAL",
        lin::unshare(0x0000_1000),
        lin::EINVAL,
    );
    // An undefined bit is refused ahead of the namespaces beside it, which is
    // Linux's order (measured: `CLONE_PIDFD|CLONE_NEWNS` is EINVAL, not the
    // namespace's EPERM).
    lin::check_eq(
        &mut failed,
        b"unshare with a non-namespace bit answers EINVAL",
        lin::unshare(0x1),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"an undefined bit outranks a namespace bit",
        lin::unshare(0x0002_1000),
        lin::EINVAL,
    );
    let mut pipe_fds: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the pipe pair is made",
        lin::pipe2(&mut pipe_fds, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setns on a pipe answers EINVAL",
        lin::setns(pipe_fds[0], 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"setns on a closed number answers EBADF",
        lin::setns(99, 0),
        lin::EBADF,
    );

    // One thread's signal, named by its group and itself: the probe form
    // (sig 0) answers 0 for a live thread, ESRCH for a dead one and for a
    // group the thread does not belong to.
    let pid = lin::getpid();
    let tid = lin::gettid();
    lin::check_eq(
        &mut failed,
        b"tgkill's probe on self succeeds",
        lin::tgkill(pid, tid, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"tgkill's probe on a dead thread answers ESRCH",
        lin::tgkill(pid, 99, 0),
        lin::ESRCH,
    );
    lin::check_eq(
        &mut failed,
        b"tgkill with a wrong group answers ESRCH",
        lin::tgkill(9999, tid, 0),
        lin::ESRCH,
    );

    // The memory advice family, over a page that is mapped.
    let page = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    ) as usize;
    lin::check_true(&mut failed, b"mmap hands back a page", page != 0);
    static mut VEC: [u8; 1] = [0; 1];
    let vec = unsafe { &raw mut VEC as *mut u8 };
    lin::check_eq(
        &mut failed,
        b"mincore succeeds",
        lin::mincore(page, 4096, vec),
        0,
    );
    // Anonymous demand paging: the address is the process's from the `mmap`,
    // and nothing is behind it until it is touched. `mincore` answers from the
    // page tables, so a freshly mapped page reads as mapped and *not*
    // resident - which is the truth, and the point.
    lin::check_true(
        &mut failed,
        b"a freshly mapped page is mapped and not yet resident",
        unsafe { VEC[0] == 0 },
    );
    lin::check_eq(
        &mut failed,
        b"mincore on an unaligned range answers EINVAL",
        lin::mincore(page + 1, 4096, vec),
        lin::EINVAL,
    );
    unsafe { core::ptr::write_volatile(page as *mut u8, 1) };
    lin::check_eq(
        &mut failed,
        b"mincore after touching the page succeeds",
        lin::mincore(page, 4096, vec),
        0,
    );
    lin::check_true(&mut failed, b"the touched page is resident", unsafe {
        VEC[0] == 1
    });
    lin::check_eq(&mut failed, b"mlock succeeds", lin::mlock(page, 4096), 0);
    lin::check_eq(
        &mut failed,
        b"munlock succeeds",
        lin::munlock(page, 4096),
        0,
    );
    lin::check_eq(&mut failed, b"mlockall succeeds", lin::mlockall(1), 0);
    lin::check_eq(
        &mut failed,
        b"mlockall with an unknown flag answers EINVAL",
        lin::mlockall(0x8),
        lin::EINVAL,
    );
    lin::check_eq(&mut failed, b"munlockall succeeds", lin::munlockall(), 0);
    lin::check_eq(&mut failed, b"msync succeeds", lin::msync(page, 4096, 4), 0);
    lin::check_eq(
        &mut failed,
        b"msync with SYNC beside ASYNC answers EINVAL",
        lin::msync(page, 4096, 1 | 4),
        lin::EINVAL,
    );

    let _ = lin::close(pipe_fds[0]);
    let _ = lin::close(pipe_fds[1]);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-posix: panicked\n")
}
