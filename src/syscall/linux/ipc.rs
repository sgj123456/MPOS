//! The POSIX message-queue family: `mq_open`, `mq_unlink`, `mq_timedsend`,
//! `mq_timedreceive`, `mq_notify`, `mq_getsetattr`.
//!
//! The objects are `vfs::mqueue`'s; this is the validating face, and the split
//! is the same one every other family here has: the ABI layer owns the argument
//! order, the errno vocabulary and the descriptor table, and the layer below
//! owns what a queue is.
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
