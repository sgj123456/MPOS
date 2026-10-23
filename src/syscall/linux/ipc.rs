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

    let queue = match crate::vfs::mqueue::lookup(&name) {
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
    match crate::vfs::mqueue::unlink(&name) {
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
const SHM_PERM_UID: usize = 4;
const SHM_PERM_GID: usize = 8;
const SHM_PERM_CUID: usize = 12;
const SHM_PERM_CGID: usize = 16;
const SHM_PERM_MODE: usize = 20;
const SHM_PERM_SEQ: usize = 24;

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
fn ipc_permitted(segment: &Segment, want: u32) -> bool {
    let perm = segment.perm();
    triad_grants(perm.mode, (perm.uid, perm.gid), want)
        || crate::sched::ns_capable_of(&crate::sched::current_user_ns(), crate::cred::CAP_IPC_OWNER)
}

/// Linux's `ipcctl_obtain_check`: who may `IPC_SET` or `IPC_RMID` a segment.
///
/// The **creator**, not the current owner - which is why an `IPC_SET` that hands
/// the uid to somebody else does not hand the power to change it again - or a
/// caller with `CAP_SYS_ADMIN`. A refusal is `EPERM`, measured, while the
/// read-shaped `IPC_STAT` refuses through `ipcperms` with `EACCES`.
fn may_control(segment: &Segment) -> bool {
    let perm = segment.perm();
    crate::sched::current_owner().0 == perm.cuid
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
    put32(SHM_PERM_UID, perm.uid);
    put32(SHM_PERM_GID, perm.gid);
    put32(SHM_PERM_CUID, perm.cuid);
    put32(SHM_PERM_CGID, perm.cgid);
    put32(SHM_PERM_MODE, perm.mode);
    unsafe { ((at + SHM_PERM_SEQ) as *mut u16).write_unaligned(perm.seq) };
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

    match sysv_shm::get_or_create(new, created, exclusive) {
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
    let id = args[0] as i32 as i64;
    let hint = args[1];
    let flags = args[2] as u32;
    let Some(segment) = sysv_shm::lookup(id) else {
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
    let addr = args[0];
    // The address must be the *start* of an attachment: Linux checks
    // `vma->vm_start == addr`, so an offset into a mapping names nothing.
    let Some((start, _end, id)) = crate::sched::shm_attachment_at(addr) else {
        return err::EINVAL;
    };
    if start != addr {
        return err::EINVAL;
    }
    let Some(segment) = sysv_shm::lookup(id as i64) else {
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
            sysv_shm::highest_slot() as isize
        }
        SHM_INFO => {
            if !buffer_ok(buf, SHM_INFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            let (used, pages) = sysv_shm::totals();
            let resident: usize = sysv_shm::all().iter().map(|s| s.resident_pages()).sum();
            unsafe {
                core::ptr::write_bytes(buf as *mut u8, 0, SHM_INFO_BYTES);
                (buf as *mut u32).write_unaligned(used as u32);
                ((buf + 8) as *mut u64).write_unaligned(pages as u64);
                ((buf + 16) as *mut u64).write_unaligned(resident as u64);
            }
            sysv_shm::highest_slot() as isize
        }
        // The by-index reports: the argument is a slot `ipcs` is walking, not a
        // number a caller holds. `SHM_STAT` refuses a segment the caller may not
        // read and `SHM_STAT_ANY` does not, which is the only difference.
        SHM_STAT | SHM_STAT_ANY => {
            let Some(segment) = sysv_shm::by_slot(args[0] as u64) else {
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
            let Some(segment) = sysv_shm::lookup(id) else {
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
                    if !may_control(&segment) {
                        return err::EPERM;
                    }
                    if !buffer_ok(buf, SHM_DS_BYTES, false, from_user) {
                        return err::EFAULT;
                    }
                    let uid = unsafe { ((buf + SHM_PERM_UID) as *const u32).read_unaligned() };
                    let gid = unsafe { ((buf + SHM_PERM_GID) as *const u32).read_unaligned() };
                    let mode = unsafe { ((buf + SHM_PERM_MODE) as *const u32).read_unaligned() };
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
