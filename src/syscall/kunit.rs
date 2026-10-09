//! The `syscall` and `ipc` suites: the dispatch gate entered the way ring 3
//! enters it, and the pipes, tty line discipline and signal paths that go through
//! it.
//!
//! A child of `syscall` and compiled only into a test build. It is here rather
//! than beside `vfs` because that is where the edge points: a suite inside `vfs`
//! that calls [`crate::syscall::linux_dispatch`] closes a `vfs <-> syscall` mutual
//! pair the module graph does not allow, and the measurement said so the first
//! time it was tried (`docs/NOTES.md` §15.22 - "vfs names crate::syscall:: 1 times,
//! up from the recorded 0", and "9 mutual pairs, up from the recorded 8").
//!
//! Two suites live here for one reason, and it is the phase rather than the
//! module: the gate's cases run at [`Phase::Vfs`] and the ipc family at
//! [`Phase::Ipc`], and a suite declares the boot step it needs rather than a
//! position in a file - so the family is [`IPC_SUITE`] and not three more cases of
//! [`SUITE`]. Nor is it in `vfs`'s suite or `sched`'s, though what it checks is
//! pipes, the tty and signals: every claim it makes goes *through the gate*
//! (`pipe(2)`, a blocking `read(2)` on fd 0, `kill(2)`), and a suite that named the
//! gate from inside `vfs` or `sched` would close a pair the graph does not allow.
//! The cases were written in the wiring layer's checks directory, which was never
//! their home either - it had no subsystem to check of its own (`docs/NOTES.md`
//! §15.23).

use alloc::string::String;

use crate::case_check;
use crate::kunit::{Case, Phase, Suite, record_verdict, take_verdict};
use crate::println;
use crate::sched::kunit::{drain, wait_for_code};
use crate::sched::{self, Priority};
use crate::syscall::{self, err, linux::nr as lnr};
use crate::vfs;

/// The gate's suite: one entry into the dispatch table, from the kernel and from
/// a task.
///
/// `Phase::Vfs` because that is the boot step this point in the sequence needs:
/// the gate's cases call descriptors and the scheduler, both of which are up by
/// then, and they have run there since they were written.
pub const SUITE: Suite = Suite {
    name: "syscall",
    phase: Phase::Vfs,
    cases: &[
        Case {
            name: "gate",
            run: gate,
        },
        Case {
            name: "dispatch_in_task",
            run: dispatch_in_task,
        },
    ],
};

/// The ipc family's suite: pipes, the tty line discipline, and signal delivery.
///
/// `Phase::Ipc` is what it needs and also the last phase the boot does work in,
/// which is where these three have always run: the tty table is up, delivery
/// works, and the boot stack has been through everything a test build runs
/// (`boot.stack_depth` measures it here).
pub const IPC_SUITE: Suite = Suite {
    name: "ipc",
    phase: Phase::Ipc,
    cases: &[
        Case {
            name: "pipes",
            run: pipes,
        },
        Case {
            name: "tty",
            run: tty,
        },
        Case {
            name: "signals",
            run: signals,
        },
    ],
};

/// The gate's verdict slot.
///
/// The slots are the shared vocabulary's ([`crate::kunit`]), one per suite that
/// spawns a task, so two suites cannot collide - and none of them can be read by
/// the wrong case.
const VERDICT_SLOT: usize = 2;

/// The slot the pipe's writer stores what it wrote in.
const PIPE_SLOT: usize = 3;

/// One entry into the dispatch table, as a case's helper.
///
/// The six arguments spelled out rather than taken as an array because the family
/// below calls it that way, and it is the same call the gate's own cases make.
///
/// # Safety
///
/// As a handler's own: `nr` must be one the table carries, and the arguments must
/// have the shape that handler expects. `from_user` is false and the frame is
/// null - the two halves of "the kernel is calling on its own behalf" - so every
/// pointer a case passes is a *kernel* pointer, and a handler that needs a
/// register frame (an `execve`, a signal delivery) has none here.
pub unsafe fn syscall(
    nr: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    unsafe { syscall::linux_dispatch(nr, [a1, a2, a3, a4, a5, a6], false, core::ptr::null_mut()) }
}

/// The gate itself: an unknown number, a zero-length read, bad descriptors, and a
/// write whose count the dispatcher must not drop.
fn gate(_info: &crate::boot::BootInfo) -> Result<(), String> {
    // A number beyond the table: 1024 is CLEANUP_AFTER_KTHREAD.
    case_check!(
        unsafe { syscall(1024, 0, 0, 0, 0, 0, 0) } == err::ENOSYS,
        "a syscall number beyond the table was not ENOSYS"
    );
    case_check!(
        unsafe { syscall(lnr::READ, 0, 0, 0, 0, 0, 0) } == 0,
        "a zero-length read is empty, not an error"
    );
    case_check!(
        unsafe { syscall(lnr::READ, 3, 0, 0, 0, 0, 0) } == err::EBADF,
        "a read on an unopened descriptor was not EBADF"
    );
    case_check!(
        unsafe { syscall(lnr::WRITE, 7, 0, 0, 0, 0, 0) } == err::EBADF,
        "a write on an unopened descriptor was not EBADF"
    );

    let msg = b"syscall: write through dispatch\n";
    let wrote = unsafe { syscall(lnr::WRITE, 1, msg.as_ptr() as usize, msg.len(), 0, 0, 0) };
    case_check!(
        wrote as usize == msg.len(),
        "a write through dispatch moved {wrote} of {} bytes",
        msg.len()
    );
    Ok(())
}

/// The same gate, driven from inside a real task: `getpid` answers that task's id,
/// yield and sleep suspend and resume it, and `exit_group` takes it down.
fn dispatch_in_task(_info: &crate::boot::BootInfo) -> Result<(), String> {
    sched::spawn(
        || {
            let verdict = (|| -> Result<(), String> {
                let pid = unsafe { syscall(lnr::GETPID, 0, 0, 0, 0, 0, 0) };
                case_check!(
                    pid >= 2,
                    "getpid inside a task answered {pid}, which is not a task id"
                );

                let msg = b"syscall: write through dispatch in a spawned task\n";
                let wrote =
                    unsafe { syscall(lnr::WRITE, 1, msg.as_ptr() as usize, msg.len(), 0, 0, 0) };
                case_check!(
                    wrote as usize == msg.len(),
                    "dispatch dropped bytes: {wrote} of {}",
                    msg.len()
                );

                case_check!(
                    unsafe { syscall(lnr::SCHED_YIELD, 0, 0, 0, 0, 0, 0) } == 0,
                    "sched_yield through dispatch answered a failure"
                );

                // 2 ms, 1 byte, 0 padding - a request for the handler to exercise,
                // not one to time precisely.
                let req: [u64; 2] = [0, 2_000_000];
                case_check!(
                    unsafe { syscall(lnr::NANOSLEEP, req.as_ptr() as usize, 0, 0, 0, 0, 0) } == 0,
                    "a two-millisecond nanosleep through dispatch answered a failure"
                );
                Ok(())
            })();
            record_verdict(VERDICT_SLOT, verdict);
            unsafe { syscall(lnr::EXIT_GROUP, 0, 0, 0, 0, 0, 0) };
        },
        Priority::NORMAL,
    );
    drain();

    // `drain` waits for the task to be gone and this waits for it to have reached
    // an answer, so between them the claim "exit_group ended the task" is already
    // made - asserting the alive count again would compare `drain`'s own
    // postcondition with itself.
    take_verdict(VERDICT_SLOT, "the dispatch task")?;
    println!("M6_OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// The ipc family: pipes, the tty line discipline, signal delivery
// ---------------------------------------------------------------------------

/// Bytes cross a pipe between two tasks, and the reader sees EOF once the writer
/// is gone.
///
/// One case rather than two: both halves are the pipe's own contract, and the
/// second read is what tells a closed pipe from one that still has a writer.
fn pipes(_info: &crate::boot::BootInfo) -> Result<(), String> {
    let mut fds = [0i32; 2];
    case_check!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) } == 0,
        "pipe() failed"
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    case_check!(rfd != wfd, "pipe() handed out the same fd twice");

    sched::spawn(
        move || {
            let verdict = (|| -> Result<(), String> {
                sched::sleep_ticks(2);
                let msg = b"ping through a pipe\n";
                let wrote = unsafe {
                    syscall(
                        lnr::WRITE,
                        wfd as usize,
                        msg.as_ptr() as usize,
                        msg.len(),
                        0,
                        0,
                        0,
                    )
                };
                case_check!(
                    wrote as usize == msg.len(),
                    "the pipe dropped bytes: {wrote} of {} arrived",
                    msg.len()
                );
                vfs::close(wfd).map_err(|e| alloc::format!("writer: close: {e:?}"))?;
                Ok(())
            })();
            record_verdict(PIPE_SLOT, verdict);
        },
        Priority::NORMAL,
    );

    let msg = b"ping through a pipe\n";
    let mut buf = [0u8; 64];
    let got = unsafe {
        syscall(
            lnr::READ,
            rfd as usize,
            buf.as_mut_ptr() as usize,
            buf.len(),
            0,
            0,
            0,
        )
    };
    case_check!(
        got as usize == msg.len(),
        "the reader got {got} of {} bytes",
        msg.len()
    );
    case_check!(
        &buf[..got as usize] == msg,
        "the bytes read were {:?}, not {:?}",
        &buf[..got as usize],
        msg
    );
    let got = vfs::read(rfd, &mut buf)
        .map_err(|e| alloc::format!("a pipe with no writers answered {e:?}"))?;
    case_check!(got == 0, "a pipe with no writers read {got} bytes, not EOF");
    vfs::close(rfd).map_err(|e| alloc::format!("close: {e:?}"))?;
    drain();
    take_verdict(PIPE_SLOT, "the pipe writer")?;
    println!("pipe: bytes crossed between two tasks, EOF after the writer left");

    // A write with no reader is EPIPE, and the writer lives to hear about it.
    let mut fds = [0i32; 2];
    case_check!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) } == 0,
        "pipe() failed"
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    sched::spawn(
        move || {
            let _ = vfs::close(rfd);
        },
        Priority::NORMAL,
    );
    drain();
    let rc = unsafe { syscall(lnr::WRITE, wfd as usize, b"x".as_ptr() as usize, 1, 0, 0, 0) };
    case_check!(
        rc == err::EPIPE,
        "a write to a readerless pipe answered {rc}, not EPIPE"
    );
    vfs::close(wfd).map_err(|e| alloc::format!("close: {e:?}"))?;
    println!("pipe: write with no reader answered EPIPE and nobody died");
    Ok(())
}

/// The tty's line discipline: editing, echo, canonical reads, `Ctrl-D`, and a
/// blocking read on fd 0 that the gate arms.
fn tty(_info: &crate::boot::BootInfo) -> Result<(), String> {
    let tty = vfs::tty::global();
    for &b in b"hellx\x7fo\n" {
        tty.receive_byte(b);
    }
    let mut line = [0u8; 32];
    // Every one of these reads is fed before it is asked, so it never blocks -
    // and a boot task with no signal dispositions cannot be interrupted either.
    // The error answers rather than asserting: a panic here would end the machine
    // and hide the cases after this one.
    let n = tty
        .read(&mut line)
        .map_err(|e| alloc::format!("a fed tty read answered {e:?}"))?;
    case_check!(
        &line[..n] == b"hello\n",
        "backspace did not edit the line: {:?}",
        &line[..n]
    );

    for &b in b"gate\n\x04" {
        tty.receive_byte(b);
    }
    let n = tty
        .read(&mut line)
        .map_err(|e| alloc::format!("a fed tty read answered {e:?}"))?;
    case_check!(
        &line[..n] == b"gate\n",
        "the second line read back {:?}",
        &line[..n]
    );
    let n = tty
        .read(&mut line)
        .map_err(|e| alloc::format!("a fed tty read answered {e:?}"))?;
    case_check!(
        n == 0,
        "Ctrl-D on an empty line answered {n} bytes, not EOF"
    );

    // The same tty through the gate: a blocking read on fd 0, fed from a task.
    sched::spawn(
        || {
            sched::sleep_ticks(2);
            for &b in b"slow\n" {
                vfs::tty::global().receive_byte(b);
            }
        },
        Priority::NORMAL,
    );
    let n = unsafe {
        syscall(
            lnr::READ,
            0,
            line.as_mut_ptr() as usize,
            line.len(),
            0,
            0,
            0,
        )
    };
    case_check!(
        n as usize == b"slow\n".len(),
        "the blocking fd 0 read answered {n} of {} bytes",
        b"slow\n".len()
    );
    case_check!(
        &line[..n as usize] == b"slow\n",
        "the blocking fd 0 read got {:?}",
        &line[..n as usize]
    );
    drain();
    println!("tty: line editing, echo, Ctrl-D, and a blocking fd 0 read");
    Ok(())
}

/// `SIGTERM`, `SIGKILL`, a signal a task sends itself, and the `kill(0)` probe.
fn signals(_info: &crate::boot::BootInfo) -> Result<(), String> {
    use crate::sched::signal::{SIGKILL, SIGTERM};

    let paused = sched::spawn(|| sched::pause(), Priority::NORMAL);
    sched::sleep_ticks(2);
    case_check!(
        sched::send_signal(paused, SIGTERM),
        "SIGTERM was refused for a paused task"
    );
    wait_for_code(paused);
    case_check!(
        sched::exit_code(paused) == Some(-(SIGTERM as isize)),
        "SIGTERM did not end the paused task: it exited {:?}",
        sched::exit_code(paused)
    );
    drain();

    // The same delivery into a task that is blocked *inside* the gate: a read on a
    // pipe that has no writer and, until the end of this case, one open descriptor.
    let mut fds = [0i32; 2];
    case_check!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) } == 0,
        "pipe() failed"
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    let blocked = sched::spawn(
        move || {
            let mut buf = [0u8; 8];
            let n = vfs::read(rfd, &mut buf).unwrap();
            // A task has no case to return to, so this is a scaffold rather than a
            // check: reaching it means the read returned instead of the task being
            // killed, and a machine in that state should not carry on.
            unreachable!("a read with no writer and no signal returned {n}");
        },
        Priority::NORMAL,
    );
    sched::sleep_ticks(2);
    case_check!(
        sched::send_signal(blocked, SIGKILL),
        "SIGKILL was refused for a blocked reader"
    );
    wait_for_code(blocked);
    case_check!(
        sched::exit_code(blocked) == Some(-(SIGKILL as isize)),
        "SIGKILL did not end the blocked reader: it exited {:?}",
        sched::exit_code(blocked)
    );
    drain();
    vfs::close(wfd).map_err(|e| alloc::format!("close: {e:?}"))?;

    let selfsent = sched::spawn(
        || {
            sched::send_signal(sched::current(), SIGTERM);
            sched::pause();
        },
        Priority::NORMAL,
    );
    wait_for_code(selfsent);
    case_check!(
        sched::exit_code(selfsent) == Some(-(SIGTERM as isize)),
        "a signal a task sent itself did not end it: it exited {:?}",
        sched::exit_code(selfsent)
    );
    drain();

    case_check!(
        unsafe { syscall(lnr::KILL, sched::current().0, 0, 0, 0, 0, 0) } == 0,
        "kill(0) on the calling task answered a failure"
    );
    case_check!(
        unsafe { syscall(lnr::KILL, 9999, SIGTERM as usize, 0, 0, 0, 0) } == err::ESRCH,
        "kill of a task that does not exist was not ESRCH"
    );
    // The signal-refusal boundary. Stage B opened the real-time range, so
    // `SIGRTMIN` (32) is now a valid `kill` target (as on Linux); the number
    // that must be refused is the one past `_NSIG` (64), not 32.
    case_check!(
        unsafe { syscall(lnr::KILL, 0, 65, 0, 0, 0, 0) } == err::EINVAL,
        "a signal number past _NSIG was not EINVAL"
    );
    println!("signals: SIGTERM, SIGKILL, and the kill(0) probe behave");

    // NB: this is not M8_OK. The M8 marker belongs to the init handoff, which is
    // the real "the machine is up" event. This family's marker is its own, so a
    // harness that asserts on M8_OK is asserting on init, not on the IPC paths.
    println!("M8_IPC_OK");
    Ok(())
}
