//! The IPC families: the six POSIX message-queue calls (`mq_open`,
//! `mq_unlink`, `mq_timedsend`, `mq_timedreceive`, `mq_notify`,
//! `mq_getsetattr`) and the four System V shared-memory ones (`shmget`,
//! `shmat`, `shmdt`, `shmctl`).
//!
//! They share a module because a caller treats them as one namespace's contents
//! - `CLONE_NEWIPC` isolates both - and because the shape of the file is the
//! same twice: the objects live below (`vfs::mqueue`, `mm::sysv_shm`), and this
//! is the validating face. The split every other family here has holds: the ABI
//! layer owns the argument order, the errno vocabulary and the descriptor table,
//! and the layer below owns what a queue or a segment *is*.
//!
//! The contract is Linux's, measured on a current host before any of it was
//! written (Linux 7.2.11, `.workbuddy/probe_mqueue{,2}.c`; the evidence and the
//! decisions are `docs/notes/` §6.12). The measurements that shaped *this* file:
//!
//! * a name without its leading `/` is `EINVAL`, a second `/` is `EACCES` and a
//!   name over 256 bytes is `ENAMETOOLONG` - three different answers, so the
//!   check is three checks;
//! * `O_CREAT|O_EXCL` on a name that exists is `EEXIST`, `O_RDWR` on a name that
//!   does not and without `O_CREAT` is `ENOENT`;
//! * `mq_send` on an `O_RDONLY` descriptor and `mq_receive` on an `O_WRONLY` one
//!   are `EBADF` - the access mode is the descriptor's, not the queue's;
//! * a full queue's non-blocking send and an empty queue's non-blocking receive
//!   are both `EAGAIN`; with a timeout they are `ETIMEDOUT`, and the deadline is
//!   an **absolute `CLOCK_REALTIME`** instant;
//! * `mq_receive`'s buffer must hold `mq_msgsize` bytes whatever the queued
//!   message's length is (`EMSGSIZE` otherwise), and a message wider than
//!   `mq_msgsize` is `EMSGSIZE` at the send;
//! * `mq_getattr`'s `mq_flags` is the descriptor's `O_NONBLOCK` bit, the same bit
//!   `mq_setattr` and `fcntl(F_SETFL)` write;
//! * one notification registration per queue (`EBUSY` on a second), fired by the
//!   send that finds the queue empty and cleared by the firing, delivered as
//!   `SI_MESGQ` with the registration's `sigval`.

use alloc::string::String;

use super::abi::{O_CLOEXEC, O_CREAT, O_EXCL};
use crate::syscall::{ARG_COUNT, buffer_ok, cstr_arg, err, err_of};
use crate::vfs::file::{ACC_RDONLY, ACC_RDWR, ACC_WRONLY, FD_CLOEXEC, File, O_ACCMODE, O_NONBLOCK};
use crate::vfs::mqueue::{
    MQ_PRIO_MAX, MSG_MAX, MSGSIZE_MAX, MqError, MqueueFile, NAME_MAX, Registration,
};

/// `RLIMIT_MSGQUEUE`'s row in `getrlimit`'s table (`asm-generic/resource.h`).
const RLIMIT_MSGQUEUE: usize = 12;

/// `struct mq_attr`'s size: four `long`s, and the two machines agree on it.
const MQ_ATTR_BYTES: usize = 32;

/// `struct sigevent`'s fields this family reads, and where they sit: the
/// `sigval` at 0, `sigev_signo` at 8, `sigev_notify` at 12 on both machines.
const SIGEV_VALUE: usize = 0;
const SIGEV_SIGNO: usize = 8;
const SIGEV_NOTIFY: usize = 12;
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;

/// `MQ_PRIO_MAX`'s own value is the queue's; this names the errno mapping.
fn err_of_mq(e: MqError) -> isize {
    match e {
        MqError::Exists => err::EEXIST,
        MqError::NoEntry => err::ENOENT,
        MqError::WouldBlock => err::EAGAIN,
        MqError::MessageTooBig | MqError::BufferTooSmall => err::EMSGSIZE,
        MqError::Invalid => err::EINVAL,
        MqError::Busy => err::EBUSY,
        MqError::OutOfBudget => err::EMFILE,
    }
}

/// The descriptor's `File` and the queue inode behind it.
///
/// Returns `EBADF` for a number with nothing behind it and for a descriptor that
/// is not a message queue - Linux's answer when an `mq_*` call is handed
/// something else.
fn queue_file(fd: usize) -> Result<(alloc::sync::Arc<File>, *const MqueueFile), isize> {
    let file = crate::sched::current_fds()
        .clone_at(fd as i32)
        .map_err(|_| err::EBADF)?;
    let inode = file.inode();
    // The inode is owned by `file`, which outlives this borrow for the caller's
    // whole operation - the pointer is only ever used while `file` is alive.
    match inode.as_mqueue() {
        Some(mq) => Ok((file, mq as *const MqueueFile)),
        None => Err(err::EBADF),
    }
}

/// Read `struct mq_attr` from the caller, or answer Linux's default.
fn read_attr(ptr: usize, from_user: bool) -> Result<(usize, usize), isize> {
    if ptr == 0 {
        return Ok((MSG_MAX as usize, MSGSIZE_MAX as usize));
    }
    if !buffer_ok(ptr, MQ_ATTR_BYTES, false, from_user) {
        return Err(err::EFAULT);
    }
    let maxmsg = unsafe { ((ptr + 8) as *const i64).read_unaligned() };
    let msgsize = unsafe { ((ptr + 16) as *const i64).read_unaligned() };
    if maxmsg <= 0 || msgsize <= 0 {
        return Err(err::EINVAL);
    }
    let (maxmsg, msgsize) = (maxmsg as usize, msgsize as usize);
    // Over the sysctl limits is `EINVAL`, measured for `msg_max` - not a
    // quiet clamp to what this kernel would rather have.
    if maxmsg as u64 > MSG_MAX || msgsize as u64 > MSGSIZE_MAX {
        return Err(err::EINVAL);
    }
    Ok((maxmsg, msgsize))
}

/// The absolute `CLOCK_REALTIME` deadline an `mq_timed*` call names, as
/// nanoseconds on the scheduler's monotonic clock.
///
/// `None` is a null pointer, which Linux reads as "block indefinitely". A timeout
/// already in the past stays in the past: the block below answers `ETIMEDOUT`
/// without sleeping, which is Linux's answer too.
fn realtime_deadline(ptr: usize, from_user: bool) -> Result<Option<u64>, isize> {
    if ptr == 0 {
        return Ok(None);
    }
    if !buffer_ok(ptr, 16, false, from_user) {
        return Err(err::EFAULT);
    }
    let sec = unsafe { (ptr as *const i64).read_unaligned() };
    let nsec = unsafe { ((ptr + 8) as *const i64).read_unaligned() };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(err::EINVAL);
    }
    let realtime = (sec as u64).saturating_mul(1_000_000_000) + nsec as u64;
    let boot = crate::hal::boot_epoch().saturating_mul(1_000_000_000);
    Ok(Some(realtime.saturating_sub(boot)))
}

/// Park until `cond` holds, `deadline` passes, or a signal arrives.
///
/// The waiter is on the queue's list so the *other* side wakes it: a queue's
/// readiness changes when somebody sends or receives, and neither of those is an
/// interrupt this task would otherwise hear.
fn park(
    queue: &crate::vfs::mqueue::Queue,
    cond: impl Fn() -> bool,
    deadline: Option<u64>,
) -> Result<(), isize> {
    let me = crate::sched::current();
    queue.add_waiter(me);
    let end = crate::sched::block_until_nanos(cond, deadline.unwrap_or(u64::MAX));
    queue.remove_waiter(me);
    match end {
        crate::sched::WaitEnd::Condition => Ok(()),
        crate::sched::WaitEnd::Deadline => Err(err::ETIMEDOUT),
        crate::sched::WaitEnd::Interrupted => Err(err::EINTR),
    }
}

/// `mq_open(name, oflag, mode, attr)`.
pub fn handle_mq_open(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let name = match cstr_arg(args[0], from_user) {
        Ok(name) => name,
        Err(e) => return e,
    };
    let oflag = args[1] as u32;
    // Three name rules, three answers, all measured on the raw syscall - the
    // libc wrapper is not the ABI: both libcs strip the leading `/` the caller
    // wrote and reject a name that had none, so what reaches here is bare. An
    // empty name is `ENOENT`, **any** slash is `EACCES` (it is a path into
    // mqueuefs, which is not what this call opens), and a name past `NAME_MAX`
    // is `ENAMETOOLONG`.
    if name.is_empty() {
        return err::ENOENT;
    }
    if name.len() > NAME_MAX {
        return err::ENAMETOOLONG;
    }
    if name.contains('/') {
        return err::EACCES;
    }
    // The access mode, `O_CREAT`/`O_EXCL`, and the two flags that matter to the
    // descriptor. Any other bit is refused rather than ignored.
    let allowed = O_ACCMODE | O_CREAT as u32 | O_EXCL as u32 | O_NONBLOCK | O_CLOEXEC as u32;
    if oflag & !allowed != 0 {
        return err::EINVAL;
    }
    let (readable, writable) = match oflag & O_ACCMODE {
        ACC_RDONLY => (true, false),
        ACC_WRONLY => (false, true),
        ACC_RDWR => (true, true),
        _ => return err::EINVAL,
    };
    if args[2] as u32 > 0o777 {
        return err::EINVAL;
    }

    // POSIX queues are the fourth table `CLONE_NEWIPC` isolates: a name is the
    // namespace's, so the same name inside another namespace is a second queue
    // (measured, §6.16).
    let ns = crate::sched::current_ipc_ns();
    let queue = match crate::vfs::mqueue::lookup(ns, &name) {
        Some(queue) => {
            if oflag & O_CREAT as u32 != 0 && oflag & O_EXCL as u32 != 0 {
                return err::EEXIST;
            }
            queue
        }
        None => {
            if oflag & O_CREAT as u32 == 0 {
                return err::ENOENT;
            }
            let (maxmsg, msgsize) = match read_attr(args[3], from_user) {
                Ok(attr) => attr,
                Err(e) => return e,
            };
            let uid = crate::sched::current_identity().ruid;
            let limit = match crate::sched::current_rlimit(RLIMIT_MSGQUEUE) {
                Some(limit) if limit.cur != crate::abi::RLIM_INFINITY => limit.cur,
                // A limit this kernel has no row for is read as the machine's
                // default, which is the number Linux ships.
                _ => 819_200,
            };
            match crate::vfs::mqueue::create(
                ns,
                String::from(name.as_str()),
                maxmsg,
                msgsize,
                uid,
                limit,
            ) {
                Ok(queue) => queue,
                Err(e) => return err_of_mq(e),
            }
        }
    };

    let inode = MqueueFile::new(queue, readable, writable);
    let fd = match crate::vfs::open_inode(inode) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    if oflag & O_CLOEXEC as u32 != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, FD_CLOEXEC);
    }
    if oflag & O_NONBLOCK != 0
        && let Ok(file) = crate::sched::current_fds().clone_at(fd as i32)
    {
        file.set_status(O_NONBLOCK);
    }
    fd as isize
}

/// `mq_unlink(name)`.
pub fn handle_mq_unlink(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let name = match cstr_arg(args[0], from_user) {
        Ok(name) => name,
        Err(e) => return e,
    };
    if name.is_empty() {
        return err::ENOENT;
    }
    if name.len() > NAME_MAX {
        return err::ENAMETOOLONG;
    }
    if name.contains('/') {
        return err::EACCES;
    }
    match crate::vfs::mqueue::unlink(crate::sched::current_ipc_ns(), &name) {
        Ok(()) => 0,
        Err(e) => err_of_mq(e),
    }
}

/// `flags is the descriptor's` - the one bit `mq_getattr` reports and
/// `mq_setattr` writes.
fn nonblocking(file: &File) -> bool {
    file.status() & O_NONBLOCK != 0
}

fn sender_identity() -> (i32, i32) {
    let pid = crate::sched::pid_number_of(crate::sched::current()).unwrap_or(0) as i32;
    let uid = crate::sched::current_identity().ruid as i32;
    (pid, uid)
}

/// `mq_timedsend(fd, msg, len, prio, timeout)`.
pub fn handle_mq_timedsend(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (file, mq) = match queue_file(args[0]) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    // SAFETY: `file` owns the inode for as long as this borrow is used.
    let mq = unsafe { &*mq };
    if !mq.writable() {
        return err::EBADF;
    }
    let ptr = args[1];
    let len = args[2];
    let priority = args[3] as u32;
    if !buffer_ok(ptr, len, false, from_user) {
        return err::EFAULT;
    }
    let queue = mq.queue();
    if len > queue.msgsize {
        return err::EMSGSIZE;
    }
    if priority >= MQ_PRIO_MAX {
        return err::EINVAL;
    }
    // The bytes are copied before any wait: a message the caller cannot hand
    // over is `EFAULT` even on a full queue, which is the order Linux checks in
    // (the buffer, then the room).
    let mut data = alloc::vec![0u8; len];
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, data.as_mut_ptr(), len) };

    // The enqueue is retried around the park: a waiter that woke has to compete
    // for the room with whoever else the wake reached, and the answer to "I was
    // woken but it is full again" is to park again - not to hand a *blocking*
    // caller `EAGAIN`, which is a shape only `O_NONBLOCK` may produce.
    let deadline = if queue.is_full() && !nonblocking(&file) {
        match realtime_deadline(args[4], from_user) {
            Ok(deadline) => deadline,
            Err(e) => return e,
        }
    } else {
        None
    };
    loop {
        if queue.is_full() {
            if nonblocking(&file) {
                return err::EAGAIN;
            }
            if let Err(e) = park(queue, || queue.has_room(), deadline) {
                return e;
            }
            continue;
        }
        let was_empty = queue.is_empty();
        match queue.send(priority, data.clone()) {
            Ok(()) => {
                queue.wake_waiters();
                // The notification is Linux's: the send that finds a
                // *previously empty* queue, and the registration goes with it.
                if was_empty && let Some(registration) = queue.take_registration() {
                    notify_registered(registration);
                }
                return 0;
            }
            Err(MqError::WouldBlock) => continue,
            Err(e) => return err_of_mq(e),
        }
    }
}

/// Raise the signal a registration asked for.
///
/// `SI_MESGQ` is `-3` (measured), and the `sigval` is the registration's - which
/// is why the record is built here rather than through
/// [`crate::sched::signal::SigInfo::queue`], whose code is `SI_QUEUE`'s.
fn notify_registered(registration: Registration) {
    if registration.signo == 0 {
        // A `SIGEV_NONE` registration: the slot was held, nothing is sent.
        return;
    }
    let Some(target) = crate::sched::resolve_pid(registration.pid as i64) else {
        return;
    };
    let (pid, uid) = sender_identity();
    let info = crate::sched::signal::SigInfo::signed(
        registration.signo,
        0,
        crate::sched::signal::SI_MESGQ,
        pid,
        uid,
        registration.value,
    );
    let _ = crate::sched::send_signal_info(target, registration.signo, info);
}

/// `mq_timedreceive(fd, msg, len, prio, timeout)`.
pub fn handle_mq_timedreceive(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (file, mq) = match queue_file(args[0]) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    // SAFETY: `file` owns the inode for as long as this borrow is used.
    let mq = unsafe { &*mq };
    if !mq.readable() {
        return err::EBADF;
    }
    let ptr = args[1];
    let len = args[2];
    let prio_ptr = args[3];
    let queue = mq.queue();
    // Linux compares the caller's buffer with `mq_msgsize`, not with the
    // message it is about to hand over.
    if len < queue.msgsize {
        return err::EMSGSIZE;
    }
    // Both destinations are checked before anything is taken, so a fault cannot
    // consume a message the caller never received.
    if !buffer_ok(ptr, len, true, from_user)
        || (prio_ptr != 0 && !buffer_ok(prio_ptr, 4, true, from_user))
    {
        return err::EFAULT;
    }
    let deadline = if queue.is_empty() && !nonblocking(&file) {
        match realtime_deadline(args[4], from_user) {
            Ok(deadline) => deadline,
            Err(e) => return e,
        }
    } else {
        None
    };
    loop {
        if queue.is_empty() {
            if nonblocking(&file) {
                return err::EAGAIN;
            }
            if let Err(e) = park(queue, || !queue.is_empty(), deadline) {
                return e;
            }
            continue;
        }
        match queue.receive(len) {
            Ok((data, priority)) => {
                unsafe {
                    core::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
                    if prio_ptr != 0 {
                        (prio_ptr as *mut u32).write_unaligned(priority);
                    }
                }
                queue.wake_waiters();
                return data.len() as isize;
            }
            Err(MqError::WouldBlock) => continue,
            Err(e) => return err_of_mq(e),
        }
    }
}

/// `mq_notify(fd, sigevent)`.
///
/// Linux's kernel side is `SIGEV_SIGNAL` and `SIGEV_NONE`; `SIGEV_THREAD` is a
/// libc construction (a helper thread plus a signal registration) and never
/// arrives here, so one that does is `EINVAL`. `SIGEV_NONE` is *not* the
/// unregister call, whatever POSIX says: a registration of it holds the queue's
/// slot exactly as a signal one does, and the next registration is `EBUSY` until
/// a send clears it (measured twice over, `.workbuddy/probe_mqueue2.c`).
pub fn handle_mq_notify(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (_file, mq) = match queue_file(args[0]) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    // SAFETY: `file` owns the inode for as long as this borrow is used.
    let mq = unsafe { &*mq };
    let ptr = args[1];
    if ptr == 0 || !buffer_ok(ptr, 16, false, from_user) {
        return err::EFAULT;
    }
    let value = unsafe { ((ptr + SIGEV_VALUE) as *const u64).read_unaligned() };
    let signo = unsafe { ((ptr + SIGEV_SIGNO) as *const i32).read_unaligned() };
    let notify = unsafe { ((ptr + SIGEV_NOTIFY) as *const i32).read_unaligned() };
    let signo = match notify {
        SIGEV_SIGNAL => {
            if !(1..=64).contains(&signo) {
                return err::EINVAL;
            }
            signo as u8
        }
        SIGEV_NONE => 0,
        SIGEV_THREAD => return err::EINVAL,
        _ => return err::EINVAL,
    };
    let pid = crate::sched::pid_number_of(crate::sched::current()).unwrap_or(0) as i32;
    let registration = Registration {
        signo,
        value,
        pid,
        owner: crate::sched::current().0,
    };
    match mq.queue().register(registration) {
        Ok(()) => 0,
        Err(e) => err_of_mq(e),
    }
}

/// `mq_getsetattr(fd, newattr, oldattr)`.
pub fn handle_mq_getsetattr(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (file, mq) = match queue_file(args[0]) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    // SAFETY: `file` owns the inode for as long as this borrow is used.
    let mq = unsafe { &*mq };
    let new_attr = args[1];
    let old_attr = args[2];
    let queue = mq.queue();
    let current_flags = if nonblocking(&file) {
        O_NONBLOCK as u64
    } else {
        0
    };
    if old_attr != 0 {
        if !buffer_ok(old_attr, MQ_ATTR_BYTES, true, from_user) {
            return err::EFAULT;
        }
        let curmsgs = queue.queued() as u64;
        unsafe {
            (old_attr as *mut u64).write_unaligned(current_flags);
            ((old_attr + 8) as *mut u64).write_unaligned(queue.maxmsg as u64);
            ((old_attr + 16) as *mut u64).write_unaligned(queue.msgsize as u64);
            ((old_attr + 24) as *mut u64).write_unaligned(curmsgs);
        }
    }
    if new_attr != 0 {
        if !buffer_ok(new_attr, MQ_ATTR_BYTES, false, from_user) {
            return err::EFAULT;
        }
        let flags = unsafe { (new_attr as *const u64).read_unaligned() };
        // `mq_flags` has one bit to hold here, and anything else is refused
        // rather than stored and ignored.
        if flags & !(O_NONBLOCK as u64) != 0 {
            return err::EINVAL;
        }
        let mut status = file.status();
        if flags & O_NONBLOCK as u64 != 0 {
            status |= O_NONBLOCK;
        } else {
            status &= !O_NONBLOCK;
        }
        file.set_status(status);
    }
    0
}

// ---------------------------------------------------------------------------
// System V shared memory: `shmget`, `shmat`, `shmdt`, `shmctl`
// ---------------------------------------------------------------------------
//
// The objects are `mm::sysv_shm`'s; this is the validating face, the same split
// the message queues above have. Every rule below was measured on a current host
// first (`docs/notes/` §6.13, probes under `.workbuddy/`):
//
// * `shmget`'s mode is the low nine bits of `shmflg` and **no umask** is applied
//   - a `0666` request under `umask 0777` stores `0666` (measured);
// * the errno order on an existing key is Linux's own: `IPC_CREAT|IPC_EXCL` is
//   `EEXIST`, a size larger than the segment is `EINVAL`, and only then does a
//   request that names permission bits get its `EACCES` - measured with all
//   three refused against one `0600` segment owned by another user;
// * an existing key with **no** permission bits in `shmflg` is answered without
//   any check (`shmget(key, 0, 0)` returns the id), which is how a caller finds
//   out whether a name exists;
// * `shmat` takes an explicit address only if it is free and a busy one is
//   `EINVAL` rather than relocated; `SHM_RND` rounds down to the page, `SHM_REMAP`
//   replaces what is there, and a misaligned address without `SHM_RND` is
//   `EINVAL`;
// * `shmdt` names an *attachment*, so an address that is not the start of one is
//   `EINVAL`, and so is a second detach of the same address;
// * `IPC_RMID` marks a segment rather than removing it: while it has an
//   attachment its number keeps answering `IPC_STAT` and a further `shmat`, a
//   `shmget` on its old key makes a *new* segment, its key reads back as
//   `IPC_PRIVATE`, and only after the last detach does the number become
//   `EINVAL`;
// * `IPC_STAT` on another user's segment is `EACCES`, while `IPC_SET` and
//   `IPC_RMID` are `EPERM` - two different checks with two different answers;
// * `SHM_LOCK` sets a mode bit (`02600` read back) with no capability asked on
//   this host, and `IPC_STAT` reports the size **asked for** rather than the
//   page-rounded mapping length.

use super::abi::{
    IPC_PERM_CGID, IPC_PERM_CUID, IPC_PERM_GID, IPC_PERM_MODE, IPC_PERM_SEQ, IPC_PERM_UID,
};
use crate::mm::elf::USER_LIMIT;
use crate::mm::pframe::PAGE_SIZE;
use crate::mm::sysv_shm::{self, MODE_MASK, SHMALL, SHMMAX, SHMMNI, Segment, ShmError};

/// `ipc.h`'s creation bits.
const IPC_CREAT: u32 = 0o1000;
const IPC_EXCL: u32 = 0o2000;

/// `shm.h`'s attach bits. `SHM_NORESERVE` shares its value with `SHM_RDONLY`:
/// they belong to different calls' alphabets, which is exactly why each is named
/// where it is used rather than by one shared constant.
const SHM_RDONLY: u32 = 0o10000;
const SHM_RND: u32 = 0o20000;
const SHM_REMAP: u32 = 0o40000;
const SHM_EXEC: u32 = 0o100000;
/// `shm.h`'s creation bits.
const SHM_HUGETLB: u32 = 0o4000;
const SHM_NORESERVE: u32 = 0o10000;

/// `shmctl`'s commands, from `linux/ipc.h` and `linux/shm.h`.
const IPC_RMID: usize = 0;
const IPC_SET: usize = 1;
const IPC_STAT: usize = 2;
const IPC_INFO: usize = 3;
const SHM_LOCK: usize = 11;
const SHM_UNLOCK: usize = 12;
const SHM_STAT: usize = 13;
const SHM_INFO: usize = 14;
const SHM_STAT_ANY: usize = 15;

/// `struct shmid64_ds`'s layout, measured on the host: 112 bytes, with the perm
/// record's fields at 0/4/8/12/16/20 and its `seq` at 24, the size at 48, the
/// three clocks at 56/64/72, the two pids at 80/84 and the attach count at 88.
/// Both machines use the asm-generic layout, so this is one table, not one per
/// port.
const SHM_DS_BYTES: usize = 112;
const SHM_SEGSZ: usize = 48;
const SHM_ATIME: usize = 56;
const SHM_DTIME: usize = 64;
const SHM_CTIME: usize = 72;
const SHM_CPID: usize = 80;
const SHM_LPID: usize = 84;
const SHM_NATTCH: usize = 88;

/// `struct shminfo64`: five `unsigned long`s and four reserved words.
const SHMINFO_BYTES: usize = 72;
/// `struct shm_info`: `int used_ids` then five `unsigned long`s - the int padded
/// out to the first long's alignment, 48 bytes in all.
const SHM_INFO_BYTES: usize = 48;

/// `SHMMIN`, the smallest size a segment may be made with.
const SHMMIN: usize = 1;

fn err_of_shm(e: ShmError) -> isize {
    match e {
        ShmError::NoEntry => err::ENOENT,
        ShmError::Exists => err::EEXIST,
        ShmError::BadSize | ShmError::TooSmall => err::EINVAL,
        ShmError::Full => err::ENOSPC,
    }
}

/// The epoch seconds a segment's clocks are stamped with.
fn now_secs() -> i64 {
    (crate::hal::boot_epoch() + crate::hal::ticks() / crate::hal::TICK_HZ as u64) as i64
}

/// The permission bits an `shmflg` asks for, folded into one triad the way
/// Linux's `ipcperms` folds its argument: whichever of the three triads the
/// caller put a bit in, the question is the same three bits of access.
fn asked_permissions(flags: u32) -> u32 {
    let mode = flags & MODE_MASK;
    ((mode >> 6) | (mode >> 3) | mode) & 0o7
}

/// Whether the caller's identity is granted `want` (`4` read, `2` write) by a
/// mode word.
fn triad_grants(mode: u32, owner: (u32, u32), want: u32) -> bool {
    let (uid, gid) = crate::sched::current_owner();
    let granted = if uid == owner.0 {
        (mode >> 6) & 0o7
    } else if gid == owner.1 || crate::sched::current_groups().contains(&owner.1) {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    granted & want == want
}

/// Linux's `ipcperms`: the triads, and `CAP_IPC_OWNER` behind them.
///
/// One rule for both families - a mode word, its owner pair and the capability -
/// because the object is the only thing that differs between them.
fn mode_permits(mode: u32, owner: (u32, u32), want: u32) -> bool {
    triad_grants(mode, owner, want)
        || crate::sched::ns_capable_of(&crate::sched::current_user_ns(), crate::cred::CAP_IPC_OWNER)
}

fn ipc_permitted(segment: &Segment, want: u32) -> bool {
    let perm = segment.perm();
    mode_permits(perm.mode, (perm.uid, perm.gid), want)
}

/// Linux's `ipcctl_obtain_check`: who may `IPC_SET` or `IPC_RMID` a segment.
///
/// The **creator**, not the current owner - which is why an `IPC_SET` that hands
/// the uid to somebody else does not hand the power to change it again - or a
/// caller with `CAP_SYS_ADMIN`. A refusal is `EPERM`, measured, while the
/// read-shaped `IPC_STAT` refuses through `ipcperms` with `EACCES`.
fn may_control(segment: &Segment) -> bool {
    creator_or_admin(segment.perm().cuid)
}

/// The `ipcctl_obtain_check` question, for an object whose creator is known.
fn creator_or_admin(cuid: u32) -> bool {
    crate::sched::current_owner().0 == cuid
        || crate::sched::ns_capable_of(&crate::sched::current_user_ns(), crate::cred::CAP_SYS_ADMIN)
}

/// Drop every destroyed segment whose last attachment has gone.
///
/// The attachment count is `sched`'s answer and the reap is `mm`'s, so the sweep
/// lives here - the layer above both. It runs after every operation that could
/// have been the last detach, which is what keeps a number from outliving the
/// segment behind it.
fn reap_dead() {
    for segment in sysv_shm::destroyed() {
        let attachments = crate::sched::shm_attachment_count(segment.id());
        sysv_shm::reap(&segment, attachments);
    }
}

/// Write one `shmid64_ds`. The caller has validated the range.
fn write_shmid_ds(at: usize, segment: &Segment, attachments: usize) {
    let perm = segment.perm();
    let times = segment.times();
    // The reserved words must read as zero rather than as whatever was in the
    // caller's buffer: a struct fill is a complete answer.
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, SHM_DS_BYTES) };
    let put32 = |off: usize, v: u32| unsafe { ((at + off) as *mut u32).write_unaligned(v) };
    let put64 = |off: usize, v: u64| unsafe { ((at + off) as *mut u64).write_unaligned(v) };
    put32(0, perm.key as u32);
    put32(IPC_PERM_UID, perm.uid);
    put32(IPC_PERM_GID, perm.gid);
    put32(IPC_PERM_CUID, perm.cuid);
    put32(IPC_PERM_CGID, perm.cgid);
    put32(IPC_PERM_MODE, perm.mode);
    unsafe { ((at + IPC_PERM_SEQ) as *mut u16).write_unaligned(perm.seq) };
    put64(SHM_SEGSZ, perm.size as u64);
    put64(SHM_ATIME, times.atime as u64);
    put64(SHM_DTIME, times.dtime as u64);
    put64(SHM_CTIME, times.ctime as u64);
    put32(SHM_CPID, perm.cpid);
    put32(SHM_LPID, times.lpid);
    put64(SHM_NATTCH, attachments as u64);
}

/// `shmget(key, size, shmflg)`.
pub fn handle_shmget(args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let key = args[0] as i32;
    let size = args[1];
    let flags = args[2] as u32;
    let me = crate::sched::current_owner();

    // `SHM_HUGETLB` asks for a huge-page pool this kernel does not have. Linux's
    // two answers are measured and separate: a caller without `CAP_IPC_LOCK` is
    // `EPERM`, and one holding it is `ENOMEM` because the pool is empty.
    if flags & SHM_HUGETLB != 0 {
        let capable = crate::sched::ns_capable_of(
            &crate::sched::current_user_ns(),
            crate::cred::CAP_IPC_LOCK,
        );
        return if capable { err::ENOMEM } else { err::EPERM };
    }
    // `SHM_NORESERVE` is accepted and means nothing here: there is no swap for a
    // reservation to be against, and the segment is the same one either way.
    let _ = flags & SHM_NORESERVE;

    let new = sysv_shm::New {
        key,
        size,
        mode: flags & MODE_MASK,
        uid: me.0,
        gid: me.1,
        cpid: crate::sched::current_pid_number(),
        now: now_secs(),
    };
    let created = flags & IPC_CREAT != 0;
    let exclusive = flags & IPC_EXCL != 0;

    match sysv_shm::get_or_create(ns, new, created, exclusive) {
        Ok((segment, made_now)) => {
            // The permission question is asked of an **existing** segment only,
            // and only when the caller named permissions at all - and it comes
            // after the `EEXIST` and size refusals, which is the measured order.
            if !made_now
                && flags & MODE_MASK != 0
                && !ipc_permitted(&segment, asked_permissions(flags))
            {
                return err::EACCES;
            }
            reap_dead();
            segment.id() as isize
        }
        Err(e) => err_of_shm(e),
    }
}

/// `shmat(shmid, shmaddr, shmflg)`.
pub fn handle_shmat(args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let hint = args[1];
    let flags = args[2] as u32;
    let Some(segment) = sysv_shm::lookup(ns, id) else {
        return err::EINVAL;
    };
    let readonly = flags & SHM_RDONLY != 0;
    if !ipc_permitted(&segment, if readonly { 0o4 } else { 0o6 }) {
        return err::EACCES;
    }
    let len = segment.mapped_len();
    let remap = flags & SHM_REMAP != 0;

    let base = if hint == 0 {
        // `SHM_REMAP` with no address names nothing to take over.
        if remap {
            return err::EINVAL;
        }
        // Linux grows the attachment area **down** from the mmap base, which is
        // the walk `mmap`'s own placement uses here: a page-aligned run from a
        // descending cursor. Successive `shmat(NULL)` calls then sit one length
        // apart, each below the last - measured.
        let cursor = crate::sched::get_mmap_cursor();
        match super::proc::free_range(len, cursor) {
            Some((base, next)) => {
                crate::sched::set_mmap_cursor(next);
                base
            }
            None => return err::ENOMEM,
        }
    } else {
        // `SHM_RND` rounds down to the page; without it an unaligned address is
        // `EINVAL` rather than rounded, which is the measured answer and the one
        // a caller can rely on.
        let addr = if flags & SHM_RND != 0 {
            hint & !(PAGE_SIZE - 1)
        } else if hint.is_multiple_of(PAGE_SIZE) {
            hint
        } else {
            return err::EINVAL;
        };
        if !addr.checked_add(len).is_some_and(|end| end <= USER_LIMIT) {
            return err::EINVAL;
        }
        if remap {
            // Take the range over: whatever describes it goes, which is what
            // lets a caller replace a mapping at a fixed address.
            crate::sched::munmap_user(addr, len);
        } else if !crate::sched::vma_range_free(addr, len) {
            // A busy address is refused, not relocated: measured, Linux answers
            // `EINVAL` here where a plain `mmap` hint would move.
            return err::EINVAL;
        }
        addr
    };

    let mut prot = crate::mm::vm::MapFlags::READ | crate::mm::vm::MapFlags::USER;
    if !readonly {
        prot |= crate::mm::vm::MapFlags::WRITE;
    }
    if flags & SHM_EXEC != 0 {
        prot |= crate::mm::vm::MapFlags::EXEC;
    }
    // The name a maps line prints. Measured (Linux 7.2.11): a segment's mapping
    // reads `/SYSV%08x (deleted)`, where the hex is the key - `00000000` for a
    // private one - and the suffix is there even for a keyed segment, because
    // what is mapped is an unlinked file in the kernel's own shmem filesystem.
    let name: alloc::sync::Arc<str> = alloc::sync::Arc::from(alloc::format!(
        "/SYSV{:08x} (deleted)",
        segment.perm().key as u32
    ));
    let object: alloc::sync::Arc<dyn crate::mm::vma::MappedObject> = segment.clone();
    crate::sched::map_frames(base, len, prot, object, 0, true, Some(name));
    segment.touch_attach(crate::sched::current_pid_number(), now_secs());
    base as isize
}

/// `shmdt(shmaddr)`.
pub fn handle_shmdt(args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let addr = args[0];
    // The address must be the *start* of an attachment: Linux checks
    // `vma->vm_start == addr`, so an offset into a mapping names nothing.
    let Some((start, _end, id)) = crate::sched::shm_attachment_at(addr) else {
        return err::EINVAL;
    };
    if start != addr {
        return err::EINVAL;
    }
    let Some(segment) = sysv_shm::lookup(ns, id as i64) else {
        // An attachment whose segment has no table entry cannot happen while the
        // attachment exists - the entry is what keeps it alive - so this is a
        // table invariant, not a caller's mistake.
        return err::EINVAL;
    };
    crate::sched::munmap_user(addr, segment.mapped_len());
    segment.touch_detach(crate::sched::current_pid_number(), now_secs());
    reap_dead();
    0
}

/// `shmctl(shmid, cmd, buf)`.
pub fn handle_shmctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let cmd = args[1];
    let buf = args[2];

    match cmd {
        // The two table-wide reports `ipcs` opens with: `buf` is a `shminfo64`
        // or a `shm_info`, and the return value is the highest slot index in use,
        // which the caller then walks with `SHM_STAT`.
        IPC_INFO => {
            if !buffer_ok(buf, SHMINFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            let info = [
                SHMMAX as u64,
                SHMMIN as u64,
                SHMMNI as u64,
                SHMMNI as u64,
                SHMALL as u64,
                0,
                0,
                0,
                0,
            ];
            unsafe {
                for (i, value) in info.iter().enumerate() {
                    ((buf + i * 8) as *mut u64).write_unaligned(*value);
                }
            }
            sysv_shm::highest_slot(ns) as isize
        }
        SHM_INFO => {
            if !buffer_ok(buf, SHM_INFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            let (used, pages) = sysv_shm::totals(ns);
            let resident: usize = sysv_shm::all(ns).iter().map(|s| s.resident_pages()).sum();
            unsafe {
                core::ptr::write_bytes(buf as *mut u8, 0, SHM_INFO_BYTES);
                (buf as *mut u32).write_unaligned(used as u32);
                ((buf + 8) as *mut u64).write_unaligned(pages as u64);
                ((buf + 16) as *mut u64).write_unaligned(resident as u64);
            }
            sysv_shm::highest_slot(ns) as isize
        }
        // The by-index reports: the argument is a slot `ipcs` is walking, not a
        // number a caller holds. `SHM_STAT` refuses a segment the caller may not
        // read and `SHM_STAT_ANY` does not, which is the only difference.
        SHM_STAT | SHM_STAT_ANY => {
            // The argument is read through `ipcid_to_idx`, the message family's
            // rule and the same open question (`MSG_STAT`'s comment above): what
            // is settled for this family is the *permission* split, and it is
            // the reverse of the message family's - measured twice with a
            // verified mode, `.workbuddy/probe_stat_perm.c`: `SHM_STAT` answers
            // a mode-`0000` segment and `SHM_STAT_ANY` refuses it `EACCES`.
            // That is this host's own oddity and it is *named* rather than
            // mirrored, because nothing explains it.
            let slot = args[0] % crate::mm::ipc_id::SEQ_MULTIPLIER as usize;
            let Some(segment) = sysv_shm::by_slot(ns, slot as u64) else {
                return err::EINVAL;
            };
            if cmd == SHM_STAT && !ipc_permitted(&segment, 0o4) {
                return err::EACCES;
            }
            if !buffer_ok(buf, SHM_DS_BYTES, true, from_user) {
                return err::EFAULT;
            }
            let attachments = crate::sched::shm_attachment_count(segment.id());
            write_shmid_ds(buf, &segment, attachments);
            segment.id() as isize
        }
        _ => {
            let Some(segment) = sysv_shm::lookup(ns, id) else {
                return err::EINVAL;
            };
            match cmd {
                IPC_STAT => {
                    if !ipc_permitted(&segment, 0o4) {
                        return err::EACCES;
                    }
                    if !buffer_ok(buf, SHM_DS_BYTES, true, from_user) {
                        return err::EFAULT;
                    }
                    let attachments = crate::sched::shm_attachment_count(segment.id());
                    write_shmid_ds(buf, &segment, attachments);
                    0
                }
                IPC_SET => {
                    // The copy comes first: measured, a non-owner whose `buf` is
                    // null hears `EFAULT` and not `EPERM`, while `IPC_STAT`
                    // answers the permission question first. Two orders in one
                    // call, which is why each is pinned
                    // (`.workbuddy/probe_ipc_order.c`, `docs/notes/` §6.14).
                    if !buffer_ok(buf, SHM_DS_BYTES, false, from_user) {
                        return err::EFAULT;
                    }
                    if !may_control(&segment) {
                        return err::EPERM;
                    }
                    let uid = unsafe { ((buf + IPC_PERM_UID) as *const u32).read_unaligned() };
                    let gid = unsafe { ((buf + IPC_PERM_GID) as *const u32).read_unaligned() };
                    let mode = unsafe { ((buf + IPC_PERM_MODE) as *const u32).read_unaligned() };
                    if !ids_storable(uid, gid) {
                        return err::EINVAL;
                    }
                    segment.set_perm(uid, gid, mode);
                    segment.touch_change(now_secs());
                    0
                }
                IPC_RMID => {
                    if !may_control(&segment) {
                        return err::EPERM;
                    }
                    // The key goes now and the segment goes when its last
                    // attachment does - measured, and the reason a `shmget` on
                    // the old key can make a new segment while this one is still
                    // mapped.
                    segment.clear_key();
                    segment.destroy();
                    reap_dead();
                    0
                }
                // `SHM_LOCK`/`SHM_UNLOCK`: the mode bit, measured with no
                // capability asked on this host (an unprivileged caller locks a
                // segment it does not own). The bit is what `ipcs` shows and what
                // a caller reads back; nothing here swaps.
                SHM_LOCK | SHM_UNLOCK => {
                    segment.set_locked(cmd == SHM_LOCK);
                    0
                }
                _ => err::EINVAL,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// System V semaphores: `semget`, `semop`, `semtimedop`, `semctl`
// ---------------------------------------------------------------------------
//
// The objects are `sched::sysv_sem`'s, and they live there rather than in `mm`
// for the reason that module's header gives: a set is a counter plus a *wait*,
// and the wait is the scheduler's - so are the `SEM_UNDO` records, which are a
// process's own cleanup at death. Every rule below was measured on a current
// host first (`docs/notes/` §6.14):
//
// * `semtimedop`'s timeout is **relative** and an expired one is `EAGAIN`;
// * an operation array is atomic, `sem_num` past the end is `EFBIG`, more than
//   `SEMOPM` entries is `E2BIG`, and an empty array is `EINVAL`;
// * unknown `sem_flg` bits are ignored, and every value stays inside
//   `0..=SEMVMX` (`ERANGE` both ways - `SETVAL(-1)` included);
// * `semget` on an existing key accepts **fewer** semaphores than it holds and
//   refuses more (`EINVAL`), and its mode is not passed through a umask;
// * `ipcperms` gates `semop` and every `GET*`/`SET*` (`EACCES`, even for the
//   owner of a `0000` set), the **creator** or `CAP_SYS_ADMIN` gates
//   `IPC_SET`/`IPC_RMID` (`EPERM`), and `IPC_SET` reads its buffer before it
//   asks who is calling (`EFAULT` first, measured);
// * a blocked `semop` wakes on another process's raise, on a handler-bearing
//   signal (`EINTR`), and on `IPC_RMID` (`EIDRM`).

use super::abi::{
    SEMBUF_BYTES, SEMID_DS_BYTES, SEMID_DS_CTIME, SEMID_DS_NSEMS, SEMID_DS_OTIME, SEMINFO_BYTES,
};
use crate::sched::sysv_sem::{self, SemBuf, SemError};

/// `semctl`'s commands, from `linux/sem.h` and `linux/ipc.h`.
const GETPID: usize = 11;
const GETVAL: usize = 12;
const GETALL: usize = 13;
const GETNCNT: usize = 14;
const GETZCNT: usize = 15;
const SETVAL: usize = 16;
const SETALL: usize = 17;
const SEM_STAT: usize = 18;
const SEM_INFO: usize = 19;
const SEM_STAT_ANY: usize = 20;

fn err_of_sem(e: SemError) -> isize {
    match e {
        SemError::NoEntry => err::ENOENT,
        SemError::Exists => err::EEXIST,
        SemError::TooSmall | SemError::BadCount => err::EINVAL,
        SemError::Full => err::ENOSPC,
        SemError::BadSem => err::EFBIG,
        SemError::Range => err::ERANGE,
        SemError::WouldBlock => err::EAGAIN,
    }
}

/// Whether the running caller may read a set - `ipcperms` with `S_IRUGO`.
fn sem_readable(set: &sysv_sem::Set) -> bool {
    let perm = set.perm();
    mode_permits(perm.mode, (perm.uid, perm.gid), 0o4)
}

/// Whether the running caller may alter a set - `ipcperms` with `S_IWUGO`,
/// which is the check `semop` and `SETVAL`/`SETALL` make.
fn sem_writable(set: &sysv_sem::Set) -> bool {
    let perm = set.perm();
    mode_permits(perm.mode, (perm.uid, perm.gid), 0o2)
}

/// Write one `semid64_ds` in the calling **machine's** layout. The caller has
/// validated the range.
///
/// The two machines disagree about this record - x86_64 pads each clock with an
/// unused word, aarch64 does not - so the offsets are `abi`'s, where the
/// measurement lives with them.
fn write_semid_ds(at: usize, set: &sysv_sem::Set) {
    let perm = set.perm();
    let (otime, ctime) = set.times();
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, SEMID_DS_BYTES) };
    let put32 = |off: usize, v: u32| unsafe { ((at + off) as *mut u32).write_unaligned(v) };
    let put64 = |off: usize, v: u64| unsafe { ((at + off) as *mut u64).write_unaligned(v) };
    put32(0, perm.key as u32);
    put32(IPC_PERM_UID, perm.uid);
    put32(IPC_PERM_GID, perm.gid);
    put32(IPC_PERM_CUID, perm.cuid);
    put32(IPC_PERM_CGID, perm.cgid);
    put32(IPC_PERM_MODE, perm.mode);
    unsafe { ((at + IPC_PERM_SEQ) as *mut u16).write_unaligned(perm.seq) };
    put64(SEMID_DS_OTIME, otime as u64);
    put64(SEMID_DS_CTIME, ctime as u64);
    put64(SEMID_DS_NSEMS, perm.nsems as u64);
}

/// `struct seminfo`'s ten `int`s, for `IPC_INFO` and `SEM_INFO`.
///
/// Both commands fill the same record; `IPC_INFO` reports the limits and
/// `SEM_INFO` the same limits with two of the fields re-purposed (Linux reuses
/// `semusz`/`semaem`/`semmnu` to report usage there), which is why one writer
/// serves both and the caller passes what the command means.
fn write_seminfo(at: usize, limits: bool) {
    // Both commands report the **limits** in the same fields; they differ only
    // in the last two, which `SEM_INFO` re-purposes as usage statistics (Linux's
    // own source calls this "cursed", and the comment there is about exactly
    // this). Measured: `IPC_INFO` and `SEM_INFO` on a current host agree on
    // `semmni`/`semmns`/`semmsl`/`semopm`/`semvmx`, while `SEM_INFO`'s
    // `semusz`/`semaem` were the undo records and lists in use.
    let (undo_records, undo_lists) = sysv_sem::undo_stats();
    let values: [i32; 10] = [
        0,                                             // semmap: unused by Linux
        sysv_sem::SEMMNI as i32,                       // semmni
        sysv_sem::SEMMNS as i32,                       // semmns
        sysv_sem::SEMMNS as i32,                       // semmnu
        sysv_sem::SEMMSL as i32,                       // semmsl
        sysv_sem::SEMOPM as i32,                       // semopm
        500,                                           // semume: undo entries per process
        if limits { 20 } else { undo_records as i32 }, // semusz
        sysv_sem::SEMVMX,                              // semvmx
        if limits {
            sysv_sem::SEMAEM
        } else {
            undo_lists as i32
        }, // semaem
    ];
    unsafe {
        for (i, value) in values.iter().enumerate() {
            ((at + i * 4) as *mut i32).write_unaligned(*value);
        }
    }
}

/// `semget(key, nsems, semflg)`.
pub fn handle_semget(args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let key = args[0] as i32;
    let nsems = args[1];
    let flags = args[2] as i32 as u32;
    let me = crate::sched::current_owner();

    let new = sysv_sem::New {
        key,
        nsems,
        mode: flags & MODE_MASK,
        uid: me.0,
        gid: me.1,
        now: now_secs(),
    };
    let created = flags & IPC_CREAT != 0;
    let exclusive = flags & IPC_EXCL != 0;

    match sysv_sem::get_or_create(ns, new, created, exclusive) {
        Ok((set, made_now)) => {
            // As with `shmget`: only an **existing** set is asked the permission
            // question, only when the caller named permissions, and after the
            // `EEXIST` and size refusals - the measured order.
            if !made_now && flags & MODE_MASK != 0 {
                let want = asked_permissions(flags);
                let perm = set.perm();
                if !mode_permits(perm.mode, (perm.uid, perm.gid), want) {
                    return err::EACCES;
                }
            }
            set.id() as isize
        }
        Err(e) => err_of_sem(e),
    }
}

/// Read a caller's `struct sembuf` array.
fn read_sembufs(
    at: usize,
    count: usize,
    from_user: bool,
) -> Result<alloc::vec::Vec<SemBuf>, isize> {
    if !buffer_ok(at, count * SEMBUF_BYTES, false, from_user) {
        return Err(err::EFAULT);
    }
    let mut ops = alloc::vec::Vec::with_capacity(count);
    for index in 0..count {
        let record = at + index * SEMBUF_BYTES;
        let num = unsafe { (record as *const u16).read_unaligned() } as usize;
        let op = unsafe { ((record + 2) as *const i16).read_unaligned() } as i32;
        let flags = unsafe { ((record + 4) as *const i16).read_unaligned() };
        ops.push(SemBuf { num, op, flags });
    }
    Ok(ops)
}

/// `semop(semid, sops, nsops)` and `semtimedop(semid, sops, nsops, timeout)`.
fn semop_common(args: [usize; ARG_COUNT], from_user: bool, timed: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let ops_at = args[1];
    let count = args[2];

    // The array's *length* is checked before its contents, and both before the
    // set: measured, an empty array is `EINVAL` and 501 entries `E2BIG`
    // whatever the id names.
    if count == 0 {
        return err::EINVAL;
    }
    if count > sysv_sem::SEMOPM {
        return err::E2BIG;
    }
    let ops = match read_sembufs(ops_at, count, from_user) {
        Ok(ops) => ops,
        Err(e) => return e,
    };
    let Some(set) = sysv_sem::lookup(ns, id) else {
        return err::EINVAL;
    };
    if !sem_writable(&set) {
        return err::EACCES;
    }

    // `semtimedop`'s timeout is a **relative** duration, measured by the clock
    // (120 ms asked, 120.5 ms observed) - the opposite of the POSIX message
    // queue's absolute instant. A malformed one is `EINVAL`, and one already
    // past wakes the attempt with `EAGAIN` on its first try.
    let deadline = if timed {
        let at = args[3];
        if !buffer_ok(at, 16, false, from_user) {
            return err::EFAULT;
        }
        let sec = unsafe { (at as *const i64).read_unaligned() };
        let nsec = unsafe { ((at + 8) as *const i64).read_unaligned() };
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return err::EINVAL;
        }
        let wait = (sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(nsec as u64);
        Some(crate::hal::since_boot_nanos().saturating_add(wait))
    } else {
        None
    };

    // An undo list is made only by a call that carries `SEM_UNDO`, so a program
    // that never asks for one never allocates it.
    let undo = if ops.iter().any(|op| op.flags & sysv_sem::SEM_UNDO != 0) {
        Some(sysv_sem::undo_list())
    } else {
        None
    };
    let me = crate::sched::current();
    let pid = crate::sched::current_pid_number();
    loop {
        match sysv_sem::semop_attempt(&set, &ops, undo.as_ref(), now_secs(), pid, Some(me)) {
            sysv_sem::Attempt::Done => {
                // A counter moved: whoever was waiting on this set looks again.
                // Waking all of them is the simple shape (they re-check and go
                // back to sleep if another got there first); a set has tens of
                // waiters, not thousands, and the check is a lock and a compare.
                sysv_sem::wake_waiters(set.handle(), |task| {
                    crate::sched::wake(task);
                });
                return 0;
            }
            sysv_sem::Attempt::Blocked => match sysv_sem::park(me, deadline) {
                crate::sched::WaitEnd::Condition => {
                    // Removed under us: the object is gone, and this is the one
                    // refusal that is about its death rather than about us.
                    if set.removed() {
                        return err::EIDRM;
                    }
                }
                crate::sched::WaitEnd::Deadline => return err::EAGAIN,
                crate::sched::WaitEnd::Interrupted => return err::EINTR,
            },
            sysv_sem::Attempt::Refused(e) => return err_of_sem(e),
        }
    }
}

/// `semop(semid, sops, nsops)`.
pub fn handle_semop(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    semop_common(args, from_user, false)
}

/// `semtimedop(semid, sops, nsops, timeout)`.
pub fn handle_semtimedop(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    semop_common(args, from_user, true)
}

/// `semctl(semid, semnum, cmd, arg)`.
pub fn handle_semctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let semnum = args[1] as i32;
    let cmd = args[2];
    let arg = args[3];

    match cmd {
        // The two table-wide reports. `semid` is ignored and `semnum` is 0, and
        // the return value is the highest slot index in use.
        IPC_INFO | SEM_INFO => {
            if !buffer_ok(arg, SEMINFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            write_seminfo(arg, cmd == IPC_INFO);
            sysv_sem::highest_slot(ns) as isize
        }
        // The by-index reports `ipcs` walks: the first argument is a slot, not a
        // number a caller holds, and the answer is the set's id.
        SEM_STAT | SEM_STAT_ANY => {
            // The argument is read through `ipcid_to_idx`, as in the other two
            // families. This host could not separate the two forms for a *set*
            // at all - every id it made came out generation 0, where a slot and
            // a full id are the same number - so the form is inherited rather
            // than claimed, and the permission split is the message family's
            // (reproduced twice: `SEM_STAT` asks read permission, `SEM_STAT_ANY`
            // does not).
            let slot = args[0] % crate::mm::ipc_id::SEQ_MULTIPLIER as usize;
            let Some(set) = sysv_sem::by_slot(ns, slot as u64) else {
                return err::EINVAL;
            };
            if cmd == SEM_STAT && !sem_readable(&set) {
                return err::EACCES;
            }
            if !buffer_ok(arg, SEMID_DS_BYTES, true, from_user) {
                return err::EFAULT;
            }
            write_semid_ds(arg, &set);
            set.id() as isize
        }
        _ => {
            let Some(set) = sysv_sem::lookup(ns, id) else {
                return err::EINVAL;
            };
            match cmd {
                IPC_STAT => {
                    // Permission first, then the copy: measured, a non-owner with
                    // a null buffer hears `EACCES` while `IPC_SET` hears
                    // `EFAULT` first.
                    if !sem_readable(&set) {
                        return err::EACCES;
                    }
                    if !buffer_ok(arg, SEMID_DS_BYTES, true, from_user) {
                        return err::EFAULT;
                    }
                    write_semid_ds(arg, &set);
                    0
                }
                IPC_SET => {
                    if !buffer_ok(arg, SEMID_DS_BYTES, false, from_user) {
                        return err::EFAULT;
                    }
                    if !creator_or_admin(set.perm().cuid) {
                        return err::EPERM;
                    }
                    let uid = unsafe { ((arg + IPC_PERM_UID) as *const u32).read_unaligned() };
                    let gid = unsafe { ((arg + IPC_PERM_GID) as *const u32).read_unaligned() };
                    let mode = unsafe { ((arg + IPC_PERM_MODE) as *const u32).read_unaligned() };
                    if !ids_storable(uid, gid) {
                        return err::EINVAL;
                    }
                    set.set_perm(uid, gid, mode, now_secs());
                    0
                }
                IPC_RMID => {
                    if !creator_or_admin(set.perm().cuid) {
                        return err::EPERM;
                    }
                    sysv_sem::remove(ns, id, |task| {
                        crate::sched::wake(task);
                    });
                    0
                }
                // The per-semaphore commands: each names one slot, and past the
                // set's end that is `EINVAL` (where `semop`'s is `EFBIG`).
                GETVAL | GETPID | GETNCNT | GETZCNT | SETVAL => {
                    let Ok(sem) = usize::try_from(semnum) else {
                        return err::EINVAL;
                    };
                    if sem >= set.nsems() {
                        return err::EINVAL;
                    }
                    if cmd == SETVAL {
                        if !sem_writable(&set) {
                            return err::EACCES;
                        }
                        return match set.set_value(sem, arg as i32, now_secs()) {
                            Ok(()) => 0,
                            Err(e) => err_of_sem(e),
                        };
                    }
                    if !sem_readable(&set) {
                        return err::EACCES;
                    }
                    match cmd {
                        GETVAL => set.value(sem).unwrap_or(0) as isize,
                        GETPID => set.lpid() as isize,
                        GETNCNT => set.waiting(sem).0 as isize,
                        _ => set.waiting(sem).1 as isize,
                    }
                }
                // `GETALL`/`SETALL` are `unsigned short` arrays, `nsems` long -
                // the one place this family's values are 16-bit on the wire.
                GETALL => {
                    if !sem_readable(&set) {
                        return err::EACCES;
                    }
                    let nsems = set.nsems();
                    if !buffer_ok(arg, nsems * 2, true, from_user) {
                        return err::EFAULT;
                    }
                    for (index, value) in set.values().iter().enumerate() {
                        unsafe {
                            ((arg + index * 2) as *mut u16).write_unaligned(*value as u16);
                        }
                    }
                    0
                }
                SETALL => {
                    if !sem_writable(&set) {
                        return err::EACCES;
                    }
                    let nsems = set.nsems();
                    if !buffer_ok(arg, nsems * 2, false, from_user) {
                        return err::EFAULT;
                    }
                    let mut values = alloc::vec::Vec::with_capacity(nsems);
                    for index in 0..nsems {
                        let value = unsafe { ((arg + index * 2) as *const u16).read_unaligned() };
                        values.push(value as i32);
                    }
                    match set.set_all(&values, now_secs()) {
                        Ok(()) => 0,
                        // A value past `SEMVMX` refuses the whole array, with
                        // nothing written - measured.
                        Err(e) => err_of_sem(e),
                    }
                }
                _ => err::EINVAL,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// System V message queues: `msgget`, `msgsnd`, `msgrcv`, `msgctl`
// ---------------------------------------------------------------------------
//
// The objects are `sched::sysv_msg`'s, and they live there for the reason that
// module's header gives: both a send on a full queue and a receive with
// nothing to hand over are *waits*, and the wait is the scheduler's. There is
// no per-process record at death - the message family has no undo list - so
// unlike its siblings there is no exit hook at all. Every rule below was
// measured on a current host first (Linux 7.2.11,
// `.workbuddy/probe_sysv_msg.c`, `probe_sysv_msg_qbytes.c`; the record is
// `docs/notes/` §6.15):
//
// * the queue is FIFO and *selection* is the receive's rule: `type > 0` names
//   the first message of that type, `type < 0` names the message of the
//   **lowest** type that is `<= -type` (with `{4, 10, 1}` queued a `-4` takes
//   the `1`), and `MSG_EXCEPT` takes the first message of another type;
// * a message too big for the buffer, without `MSG_NOERROR`, leaves the
//   message queued: `E2BIG` with `IPC_NOWAIT`, a wait for one that fits
//   otherwise - the wait is the shape that hangs, and it was found by hanging;
// * `MSG_COPY` answers by queue index without taking, needs `IPC_NOWAIT`
//   (`EINVAL` without it) and answers `ENOMSG` past the end;
// * a send's `msgsz` past `MSGMAX` (8192) is `EINVAL` and a non-positive
//   `mtype` is `EINVAL`, both before anything is queued; two `MSGMAX`
//   messages fill a fresh queue's `MSGMNB` (16384) budget and a third is
//   `EAGAIN` under `IPC_NOWAIT`;
// * a blocked send or receive is woken by its opposite number, and answers
//   `EIDRM` when the queue is removed and `EINTR` when a signal arrives;
// * `IPC_SET`'s `msg_qbytes` above `MSGMNB` is `EPERM` without
//   `CAP_SYS_RESOURCE`, while a budget *below* the queued bytes - even zero -
//   is accepted and stored (measured, `probe_sysv_msg_qbytes.c`);
// * the `msgget` rules are the segment family's: `EEXIST`, `ENOENT`, the
//   permission question asked of an existing key only when bits were named,
//   and the mode stored without a umask.

use crate::sched::sysv_msg::{self, MsgError};

/// `msgsnd`'s and `msgrcv`'s `IPC_NOWAIT` (`ipc.h`, not `semop`'s flag).
const MSG_IPC_NOWAIT: u32 = 0o4000;

/// `msgctl`'s commands, from `linux/msg.h` and `linux/ipc.h`.
const MSG_STAT: usize = 11;
const MSG_INFO: usize = 12;
const MSG_STAT_ANY: usize = 13;

/// `struct msqid64_ds`'s layout, measured on the host: 120 bytes, with the
/// three clocks at 48/56/64, `cbytes`/`qnum`/`qbytes` at 72/80/88 and the two
/// pids at 96/100. Unlike `semid64_ds` - which x86_64 pads between its clocks
/// and asm-generic does not - the message record is the same on both machines:
/// x86_64's own `asm/msgbuf.h` already carries the time64 padding, so one
/// table serves both ports.
const MSQID_DS_BYTES: usize = 120;
const MSQ_STIME: usize = 48;
const MSQ_RTIME: usize = 56;
const MSQ_CTIME: usize = 64;
const MSQ_CBYTES: usize = 72;
const MSQ_QNUM: usize = 80;
const MSQ_QBYTES: usize = 88;
const MSQ_LSPID: usize = 96;
const MSQ_LRPID: usize = 100;

/// `struct msginfo`'s eight `int`s, for `IPC_INFO` and `MSG_INFO`.
const MSGINFO_BYTES: usize = 32;

/// Whether an `IPC_SET` may **store** the two ids its record names: Linux's
/// `ipc_update_perm` maps each through the **caller's** user namespace and
/// refuses one that namespace does not cover with `EINVAL` (`make_kuid`'s
/// validity check).
///
/// Measured on a current host (`.workbuddy/probe_ipc_set_map.c`, run as root
/// inside a user namespace whose map is a single id): `uid=0`/`gid=0` is
/// accepted, `uid=1`, `uid=9` and `uid=65534` are `EINVAL`, and the answer is
/// the same from `msgctl`, `shmctl` and `semctl` - so **both** ids are asked,
/// and 65534 is an ordinary id rather than a sentinel. Before this the three
/// doors stored whatever `u32` the record carried, which is the divergence
/// §6.15 named and this closes.
///
/// The check sits **after** the owner and `CAP_SYS_RESOURCE` questions, which is
/// Linux's own order (`ipcctl_obtain_check` → qbytes → `ipc_update_perm`): a
/// caller that is not the creator hears `EPERM` whatever ids it named. That
/// order is not measured here - this host maps one id, so there is no second
/// identity to ask it from - and `docs/notes/` §6.15 records it as the one half
/// left to a two-id map.
fn ids_storable(uid: u32, gid: u32) -> bool {
    let ns = crate::sched::current_user_ns();
    ns.uid_is_mapped(uid) && ns.gid_is_mapped(gid)
}

fn err_of_msg(e: MsgError) -> isize {
    match e {
        MsgError::NoEntry => err::ENOENT,
        MsgError::Exists => err::EEXIST,
        MsgError::Full => err::ENOSPC,
        MsgError::TooBig | MsgError::BadType | MsgError::Invalid => err::EINVAL,
        MsgError::NoMsg => err::ENOMSG,
        MsgError::TooSmall => err::E2BIG,
        MsgError::QueueFull => err::EAGAIN,
    }
}

/// Whether the running caller may read a queue - `ipcperms` with `S_IRUGO`.
fn msg_readable(queue: &sysv_msg::Queue) -> bool {
    let perm = queue.perm();
    mode_permits(perm.mode, (perm.uid, perm.gid), 0o4)
}

/// Whether the running caller may write one - `ipcperms` with `S_IWUGO`.
fn msg_writable(queue: &sysv_msg::Queue) -> bool {
    let perm = queue.perm();
    mode_permits(perm.mode, (perm.uid, perm.gid), 0o2)
}

/// Write one `msqid64_ds`. The caller has validated the range.
fn write_msqid_ds(at: usize, queue: &sysv_msg::Queue) {
    let perm = queue.perm();
    let (cbytes, qnum, qbytes, lspid, lrpid, stime, rtime, ctime) = queue.stats();
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, MSQID_DS_BYTES) };
    let put32 = |off: usize, v: u32| unsafe { ((at + off) as *mut u32).write_unaligned(v) };
    let put64 = |off: usize, v: u64| unsafe { ((at + off) as *mut u64).write_unaligned(v) };
    put32(0, perm.key as u32);
    put32(IPC_PERM_UID, perm.uid);
    put32(IPC_PERM_GID, perm.gid);
    put32(IPC_PERM_CUID, perm.cuid);
    put32(IPC_PERM_CGID, perm.cgid);
    put32(IPC_PERM_MODE, perm.mode);
    unsafe { ((at + IPC_PERM_SEQ) as *mut u16).write_unaligned(perm.seq) };
    put64(MSQ_STIME, stime as u64);
    put64(MSQ_RTIME, rtime as u64);
    put64(MSQ_CTIME, ctime as u64);
    put64(MSQ_CBYTES, cbytes as u64);
    put64(MSQ_QNUM, qnum as u64);
    put64(MSQ_QBYTES, qbytes as u64);
    put32(MSQ_LSPID, lspid);
    put32(MSQ_LRPID, lrpid);
}

/// `struct msginfo`'s eight `int`s. The two commands differ in the first two
/// fields only, the message family's own "cursed" pair (the semaphore family's
/// `write_seminfo` documents the same shape): `IPC_INFO` reports the limits -
/// `msgpool` as the whole pool in KiB, `msgmap` as `msgmnb` - while `MSG_INFO`
/// re-purposes them as queues and messages in use. The measured host values
/// for the shared six: `msgmax` 8192, `msgmnb` 16384, `msgssz` 16, `msgtql`
/// 16384, `msgseg` 65535; `msgmni` is this kernel's own bound, as `SEMMNI` is
/// the semaphore family's.
fn write_msginfo(ns: Option<u64>, at: usize, limits: bool) {
    let (used, messages) = if limits { (0, 0) } else { sysv_msg::totals(ns) };
    let values: [i32; 8] = [
        if limits {
            (sysv_msg::MSGMNI * sysv_msg::MSGMNB / 1024) as i32
        } else {
            used as i32
        }, // msgpool
        if limits {
            sysv_msg::MSGMNB as i32
        } else {
            messages as i32
        }, // msgmap
        sysv_msg::MSGMAX as i32, // msgmax
        sysv_msg::MSGMNB as i32, // msgmnb
        sysv_msg::MSGMNI as i32, // msgmni
        16,                      // msgssz
        sysv_msg::MSGMNB as i32, // msgtql
        65_535,                  // msgseg
    ];
    unsafe {
        for (i, value) in values.iter().enumerate() {
            ((at + i * 4) as *mut i32).write_unaligned(*value);
        }
    }
}

/// Wake every task asleep on a queue, named by its **handle** - the callback the
/// syscall layer uses, where the lock the object layer avoids (`crate::sched::wake`
/// takes the task table, which the object layer's callers may hold). The handle
/// and not the id: two namespaces can hold queues numbered alike.
fn wake_all(handle: u64) {
    sysv_msg::wake_waiters(handle, |task| {
        let _ = crate::sched::wake(task);
    });
}

/// `msgget(key, msgflg)`.
pub fn handle_msgget(args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    let key = args[0] as i32;
    let flags = args[1] as i32 as u32;
    let me = crate::sched::current_owner();
    // The namespace is the caller's, and it is the whole of `CLONE_NEWIPC`'s
    // effect on this family: a key resolves inside one namespace, so the same
    // key inside another names a different queue (measured, §6.16).
    let ns = crate::sched::current_ipc_ns();

    let new = sysv_msg::New {
        key,
        mode: flags & MODE_MASK,
        uid: me.0,
        gid: me.1,
        now: now_secs(),
    };
    let created = flags & IPC_CREAT != 0;
    let exclusive = flags & IPC_EXCL != 0;

    match sysv_msg::get_or_create(ns, new, created, exclusive) {
        Ok((queue, made_now)) => {
            // The measured order, the segment family's: only an existing
            // queue is asked the permission question, and only when the
            // caller named bits at all.
            if !made_now && flags & MODE_MASK != 0 {
                let want = asked_permissions(flags);
                let perm = queue.perm();
                if !mode_permits(perm.mode, (perm.uid, perm.gid), want) {
                    return err::EACCES;
                }
            }
            queue.id() as isize
        }
        Err(e) => err_of_msg(e),
    }
}

/// `msgsnd(msqid, msgp, msgsz, msgflg)`.
pub fn handle_msgsnd(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let msgp = args[1];
    let msgsz = args[2];
    let flags = args[3] as u32;

    // Linux's own order, measured: the size refusal comes before anything the
    // queue could say (`8193` bytes is `EINVAL` even on a queue the caller may
    // not write to).
    if msgsz > sysv_msg::MSGMAX {
        return err::EINVAL;
    }
    if !buffer_ok(msgp, 8 + msgsz, false, from_user) {
        return err::EFAULT;
    }
    let mtype = unsafe { (msgp as *const i64).read_unaligned() };
    if mtype <= 0 {
        return err::EINVAL;
    }
    let Some(queue) = sysv_msg::lookup(ns, id) else {
        return err::EINVAL;
    };
    if !msg_writable(&queue) {
        return err::EACCES;
    }
    let mut data = alloc::vec![0u8; msgsz];
    unsafe { core::ptr::copy_nonoverlapping((msgp + 8) as *const u8, data.as_mut_ptr(), msgsz) };

    let me = crate::sched::current();
    let pid = crate::sched::current_pid_number();
    let park = (flags & MSG_IPC_NOWAIT == 0).then_some(me);
    loop {
        match sysv_msg::send_attempt(&queue, mtype, data.clone(), now_secs(), pid, park) {
            sysv_msg::SendAttempt::Done => {
                // A receiver waiting on this queue looks again.
                wake_all(queue.handle());
                return 0;
            }
            sysv_msg::SendAttempt::Blocked => match sysv_msg::park(me) {
                crate::sched::WaitEnd::Condition => {
                    if queue.removed() {
                        return err::EIDRM;
                    }
                }
                crate::sched::WaitEnd::Deadline => return err::EAGAIN,
                crate::sched::WaitEnd::Interrupted => return err::EINTR,
            },
            sysv_msg::SendAttempt::Refused(e) => return err_of_msg(e),
        }
    }
}

/// `msgrcv(msqid, msgp, msgsz, msgtyp, msgflg)`.
pub fn handle_msgrcv(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let msgp = args[1];
    let msgsz = args[2];
    let msgtyp = args[3] as i64;
    let flags = args[4] as u32;

    // Linux's own gate, and it is the sign of the `size_t` rather than any
    // size cap: `do_msgrcv` opens with `if (msqid < 0 || (long) msgsz < 0)
    // return -EINVAL`, so `LONG_MAX` is accepted and `LONG_MAX + 1` is refused
    // with the message still queued (measured, `.workbuddy/
    // probe_msgrcv_bound2.c`). It also has to come first here: the destination
    // range below is `8 + msgsz`, which wraps for a value whose sign bit is
    // set and would then validate almost nothing before writing a whole
    // message through it.
    if (msgsz as isize) < 0 {
        return err::EINVAL;
    }
    // The one flag-combination Linux refuses outright, before the queue is
    // looked at: copy mode is a snapshot and cannot wait (measured).
    if flags & sysv_msg::MSG_COPY != 0 && flags & MSG_IPC_NOWAIT == 0 {
        return err::EINVAL;
    }
    let Some(queue) = sysv_msg::lookup(ns, id) else {
        return err::EINVAL;
    };
    if !msg_readable(&queue) {
        return err::EACCES;
    }

    let me = crate::sched::current();
    let pid = crate::sched::current_pid_number();
    let park = (flags & MSG_IPC_NOWAIT == 0).then_some(me);
    loop {
        match sysv_msg::recv_attempt(&queue, msgtyp, msgsz, flags, now_secs(), pid, park) {
            sysv_msg::RecvAttempt::Got(mtype, data) => {
                // A sender waiting for budget looks again whatever happens
                // next: the message has already left the queue by the time the
                // copy can fail.
                wake_all(queue.handle());
                // Linux copies the type word and then the body *after* the
                // message has been unlinked, so a destination it cannot write
                // answers `EFAULT` with the message already gone (measured:
                // `qnum` 1 -> 0 across the failed call). This kernel writes
                // through a checked range rather than a fault-tolerant copy,
                // so it validates the same bytes before touching them - and
                // still drops the message, which is what the caller observes.
                // The range is the body's actual length, not `msgsz`: past
                // `MSGMAX` the caller's `msgsz` may be any value up to
                // `LONG_MAX` and Linux only ever copies the message's own
                // bytes.
                if !buffer_ok(msgp, 8 + data.len(), true, from_user) {
                    return err::EFAULT;
                }
                unsafe {
                    (msgp as *mut i64).write_unaligned(mtype);
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        (msgp + 8) as *mut u8,
                        data.len(),
                    );
                }
                return data.len() as isize;
            }
            sysv_msg::RecvAttempt::Blocked => match sysv_msg::park(me) {
                crate::sched::WaitEnd::Condition => {
                    if queue.removed() {
                        return err::EIDRM;
                    }
                }
                crate::sched::WaitEnd::Deadline => return err::EAGAIN,
                crate::sched::WaitEnd::Interrupted => return err::EINTR,
            },
            sysv_msg::RecvAttempt::Refused(e) => return err_of_msg(e),
        }
    }
}

/// `msgctl(msqid, cmd, buf)`.
pub fn handle_msgctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let ns = crate::sched::current_ipc_ns();
    let id = args[0] as i32 as i64;
    let cmd = args[1];
    let buf = args[2];

    match cmd {
        IPC_INFO | MSG_INFO => {
            if !buffer_ok(buf, MSGINFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            write_msginfo(ns, buf, cmd == IPC_INFO);
            sysv_msg::highest_slot(ns) as isize
        }
        MSG_STAT | MSG_STAT_ANY => {
            // Linux reads the argument through `ipcid_to_idx` - the number
            // modulo the generation stride - so a caller may hand it a slot
            // index (what `ipcs` walks) or a full id; the answer is the id
            // either way. The first draft indexed it raw, which passed on the
            // host by luck (every queue there was generation 0) and failed in
            // the guest, where the sweep reuses slots across programs.
            //
            // **The argument form is not settled by measurement.** Two probes on
            // the same host disagreed about whether a *slot* is accepted by the
            // plain `MSG_STAT` at all - `.workbuddy/probe_stat_gen.c` had it
            // `EINVAL` on a generation-nonzero queue while
            // `probe_stat_key.c` had the same call answer the full id - and
            // neither explains the other, so this keeps the reduction that
            // accepts both forms and names the question open in `docs/notes/`
            // §6.16 rather than claiming an answer. What *is* settled and pinned
            // is the permission split below.
            let slot = args[0] % crate::mm::ipc_id::SEQ_MULTIPLIER as usize;
            let Some(queue) = sysv_msg::by_slot(ns, slot as u64) else {
                return err::EINVAL;
            };
            if cmd == MSG_STAT && !msg_readable(&queue) {
                return err::EACCES;
            }
            if !buffer_ok(buf, MSQID_DS_BYTES, true, from_user) {
                return err::EFAULT;
            }
            write_msqid_ds(buf, &queue);
            queue.id() as isize
        }
        _ => {
            let Some(queue) = sysv_msg::lookup(ns, id) else {
                return err::EINVAL;
            };
            match cmd {
                IPC_STAT => {
                    if !msg_readable(&queue) {
                        return err::EACCES;
                    }
                    if !buffer_ok(buf, MSQID_DS_BYTES, true, from_user) {
                        return err::EFAULT;
                    }
                    write_msqid_ds(buf, &queue);
                    0
                }
                IPC_SET => {
                    // The copy before the question, the measured order the
                    // other two families keep too.
                    if !buffer_ok(buf, MSQID_DS_BYTES, false, from_user) {
                        return err::EFAULT;
                    }
                    if !creator_or_admin(queue.perm().cuid) {
                        return err::EPERM;
                    }
                    let uid = unsafe { ((buf + IPC_PERM_UID) as *const u32).read_unaligned() };
                    let gid = unsafe { ((buf + IPC_PERM_GID) as *const u32).read_unaligned() };
                    let mode = unsafe { ((buf + IPC_PERM_MODE) as *const u32).read_unaligned() };
                    let qbytes =
                        unsafe { ((buf + MSQ_QBYTES) as *const u64).read_unaligned() } as usize;
                    if qbytes > sysv_msg::MSGMNB {
                        // Raising the budget past the boot default is the one
                        // `IPC_SET` ask a capability gates - and the only
                        // direction it gates: lowering, even below what is
                        // queued, is every owner's to make (measured).
                        let capable = crate::sched::ns_capable_of(
                            &crate::sched::current_user_ns(),
                            crate::cred::CAP_SYS_RESOURCE,
                        );
                        if !capable {
                            return err::EPERM;
                        }
                    }
                    if !ids_storable(uid, gid) {
                        return err::EINVAL;
                    }
                    queue.set_perm(uid, gid, mode, now_secs());
                    queue.set_qbytes(qbytes, now_secs());
                    0
                }
                IPC_RMID => {
                    if !creator_or_admin(queue.perm().cuid) {
                        return err::EPERM;
                    }
                    sysv_msg::remove(ns, id, |task| {
                        crate::sched::wake(task);
                    });
                    0
                }
                _ => err::EINVAL,
            }
        }
    }
}
