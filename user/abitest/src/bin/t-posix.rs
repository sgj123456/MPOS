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
//! answers the default, seccomp is the ENOSYS of a kernel built without it
//! while its availability probe answers, the namespace pair refuses with
//! EOPNOTSUPP/EINVAL for the flags it cannot keep, and the memory advice
//! family answers for a range that is mapped.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
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
    lin::check_eq(&mut failed, b"getresuid succeeds", lin::getresuid(&mut ids), 0);
    lin::check_true(
        &mut failed,
        b"the real, effective and saved uids are root's",
        ids == [0, 0, 0],
    );
    let mut gids = [1u64; 3];
    lin::check_eq(&mut failed, b"getresgid succeeds", lin::getresgid(&mut gids), 0);
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
    lin::check_eq(&mut failed, b"getresuid still succeeds", lin::getresuid(&mut ids), 0);
    lin::check_true(
        &mut failed,
        b"the dance reads back real 0, effective 1, saved 0",
        ids == [0, 1, 0],
    );
    lin::check_eq(&mut failed, b"geteuid reads the lowered id", lin::geteuid(), 1);
    lin::check_eq(&mut failed, b"getuid still reads the real one", lin::getuid(), 0);
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
    lin::check_eq(&mut failed, b"setreuid(0, 0) succeeds", lin::setreuid(0, 0), 0);
    lin::check_eq(&mut failed, b"setreuid(-1, 0) succeeds", lin::setreuid(-1, 0), 0);
    lin::check_eq(
        &mut failed,
        b"setreuid(0, 100) succeeds for root",
        lin::setreuid(0, 100),
        0,
    );
    lin::check_eq(&mut failed, b"getresuid reads the pair back", lin::getresuid(&mut ids), 0);
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
    lin::check_eq(&mut failed, b"getresuid reads the raise back", lin::getresuid(&mut ids), 0);
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
    lin::check_eq(&mut failed, b"getresuid reads the reset", lin::getresuid(&mut ids), 0);
    lin::check_true(&mut failed, b"all three words are root's again", ids == [0, 0, 0]);

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
    lin::check_eq(&mut failed, b"setpriority 5 succeeds", lin::setpriority(0, 0, 5), 0);
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
    lin::check_eq(&mut failed, b"sched_getparam succeeds", lin::sched_getparam(0, &raw mut prio), 0);
    lin::check_eq(&mut failed, b"the SCHED_OTHER priority is zero", prio as isize, 0);
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

    // The machine: one core, node zero, the default persona.
    let mut cpu = 99u32;
    let mut node = 99u32;
    lin::check_eq(&mut failed, b"getcpu succeeds", lin::getcpu(&raw mut cpu, &raw mut node), 0);
    lin::check_true(&mut failed, b"the cpu is 0", cpu == 0);
    lin::check_true(&mut failed, b"the node is 0", node == 0);
    lin::check_eq(
        &mut failed,
        b"the persona query answers the default",
        lin::personality(0xffff_ffff),
        0,
    );
    lin::check_eq(&mut failed, b"setting the default persona succeeds", lin::personality(0), 0);
    lin::check_eq(
        &mut failed,
        b"an unknown persona answers EINVAL",
        lin::personality(0x1000),
        lin::EINVAL,
    );
    lin::check_eq(&mut failed, b"vhangup succeeds", lin::vhangup(), 0);

    // seccomp(2), as this kernel implements it (PLAN 8.2): STRICT latches
    // for real, FILTER is refused so a caller never mistakes it for being
    // confined, and the availability probe answers the empty set. The
    // strict checks are done non-destructively - once latched, this
    // program's next `getpid` is a fatal call - and the latch itself is
    // proved in a child, whose death by SIGKILL is the whole of what
    // Linux promises a strict thread that reaches for anything else.
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
        b"seccomp filter mode answers ENOSYS",
        lin::seccomp(1, 0, 0),
        lin::ENOSYS,
    );
    let mut avail = 99u32;
    lin::check_eq(
        &mut failed,
        b"the availability probe succeeds",
        lin::seccomp(2, 0, &raw mut avail as usize),
        0,
    );
    lin::check_true(&mut failed, b"no action is available", avail == 0);
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
        lin::check_true(&mut failed, b"the strict child was reaped", got == kid as isize);
        lin::check_true(
            &mut failed,
            b"the strict child died by SIGKILL on its first forbidden call",
            status & 0x7f == 9,
        );
    }

    // The namespace pair: nothing to unshare, nothing to join.
    lin::check_eq(&mut failed, b"unshare(0) succeeds", lin::unshare(0), 0);
    lin::check_eq(
        &mut failed,
        b"unshare CLONE_NEWNS answers EOPNOTSUPP",
        lin::unshare(0x0001_0000),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"unshare with a non-namespace bit answers EINVAL",
        lin::unshare(0x1),
        lin::EINVAL,
    );
    let mut pipe_fds: [i32; 2] = [-1; 2];
    lin::check_eq(&mut failed, b"the pipe pair is made", lin::pipe2(&mut pipe_fds, 0), 0);
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
    lin::check_eq(&mut failed, b"tgkill's probe on self succeeds", lin::tgkill(pid, tid, 0), 0);
    lin::check_eq(&mut failed, b"tgkill's probe on a dead thread answers ESRCH", lin::tgkill(pid, 99, 0), lin::ESRCH);
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
    lin::check_eq(&mut failed, b"mincore succeeds", lin::mincore(page, 4096, vec), 0);
    lin::check_true(&mut failed, b"the page is resident", unsafe { VEC[0] == 1 });
    lin::check_eq(
        &mut failed,
        b"mincore on an unaligned range answers EINVAL",
        lin::mincore(page + 1, 4096, vec),
        lin::EINVAL,
    );
    lin::check_eq(&mut failed, b"mlock succeeds", lin::mlock(page, 4096), 0);
    lin::check_eq(&mut failed, b"munlock succeeds", lin::munlock(page, 4096), 0);
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
