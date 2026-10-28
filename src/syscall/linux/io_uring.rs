//! The `io_uring` syscalls: `io_uring_setup(2)`, `io_uring_enter(2)` and
//! `io_uring_register(2)`.
//!
//! The ring object and its Linux-exact layout live in [`crate::vfs::io_uring`];
//! this module is the ABI surface: the argument shapes, the flag words, and the
//! per-SQE dispatch of the synchronous opcodes the kernel serves.
//!
//! **The opcode set is the measured, useful subset**: `NOP`, `READ`, `WRITE`,
//! `READV`, `WRITEV`, `FSYNC`, `CLOSE`. Each is executed inside
//! `io_uring_enter` through the very handler the corresponding syscall uses -
//! `handle_pread64`, `handle_pwrite64`, `handle_preadv`, `handle_pwritev`,
//! `handle_fsync`, `handle_close` - so a completion's `res` is the same number
//! the blocking call would have answered, including its `EBADF`/`EFAULT`/`EINVAL`
//! refusals and its `IN_MODIFY`/`IN_ACCESS` reports. An opcode this kernel does
//! not implement answers **`-EOPNOTSUPP` in its CQE** rather than an `-EINVAL`
//! Linux would not have given: the request is honestly refused per item, not
//! the whole ring.
//!
//! **What is refused, and how.** `io_uring_setup` refuses every non-zero setup
//! flag with `EOPNOTSUPP` (a poll thread, taskrun flags and 32-byte CQEs are
//! promises this implementation cannot keep - accepting them and running
//! conventionally would be the "nearly-right" answer this repository forbids);
//! a bit outside Linux's known mask is `EINVAL`, as measured. `io_uring_register`
//! is `EOPNOTSUPP` for every op, because registration exists to serve the fixed
//! buffer/file/index shapes and this ring resolves its arguments per SQE.
//! `io_uring_enter`'s `min_complete` does block when `IORING_ENTER_GETEVENTS`
//! asks it to and the CQ is empty - Linux blocks there too, and a signal ends
//! the wait with `EINTR`.

use crate::err;
use crate::syscall::ARG_COUNT;
use crate::syscall::buffer_ok;

/// `io_uring_register`'s op space, for the refusal's own record: every op is
/// answered `EOPNOTSUPP`, so the number is only validated as a number.
const _REGISTER_FILES: usize = 2;

/// The `io_uring_enter` flag this kernel honours. Linux's newer bits
/// (`SQ_WAKEUP`, `EXT_ARG`, `REGISTERED_RING`, …) describe a poll thread or an
/// extended argument layout that is not served, so they are `EINVAL` here
/// rather than silently ignored.
const IORING_ENTER_GETEVENTS: u32 = 1 << 0;

/// The SQE flags accepted: none but `IOSQE_IO_DRAIN`, whose promise - "do not
/// reorder past this one" - a synchronous execution keeps for free. Every other
/// bit (`IO_LINK`, `ASYNC`, buffer select, CQE_SKIP, …) changes the completion
/// shape this kernel does not model, and refuses with `-EOPNOTSUPP`.
const IOSQE_IO_DRAIN: u8 = 1 << 1;

/// The opcodes served, by the number the ABI gives them.
const IORING_OP_NOP: u8 = 0;
const IORING_OP_READV: u8 = 1;
const IORING_OP_WRITEV: u8 = 2;
const IORING_OP_FSYNC: u8 = 3;
const IORING_OP_CLOSE: u8 = 19;
const IORING_OP_READ: u8 = 22;
const IORING_OP_WRITE: u8 = 23;

/// The one SQE field this module needs beyond the raw bytes: the offsets are
/// Linux's `struct io_uring_sqe` and are read by hand because the struct has
/// unions whose meaning depends on the opcode.
fn sqe_u32(sqe: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([sqe[at], sqe[at + 1], sqe[at + 2], sqe[at + 3]])
}

fn sqe_u64(sqe: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&sqe[at..at + 8]);
    u64::from_le_bytes(bytes)
}

fn complete(res: isize) -> i32 {
    // The CQE's `res` is a 32-bit signed word; every answer this kernel gives
    // is a byte count or an errno, both of which fit.
    res as i32
}

/// `io_uring_setup(entries, params)` — build a ring and publish its layout.
pub fn handle_io_uring_setup(args: [usize; ARG_COUNT]) -> isize {
    let entries = args[0] as u32;
    let params = args[1];
    // The params buffer is written *and* the caller reads the words it asked
    // for; a bad one is `EFAULT` before a frame is spent.
    if !buffer_ok(params, PARAMS_BYTES, false, true) {
        return err::EFAULT;
    }
    // Linux's setup flags live in `struct io_uring_params.flags` (word 2), not
    // in an argument: `io_uring_setup` takes exactly two. Read the caller's
    // word before overwriting the struct with the layout this ring publishes.
    let mut flag_word = [0u8; 4];
    if !crate::sched::read_user(params + 8, &mut flag_word) {
        return err::EFAULT;
    }
    // Any bit outside the mask Linux knows is malformed input (`EINVAL`), and a
    // bit inside it is a feature this ring does not implement (`EOPNOTSUPP`) -
    // measured: `bit31` is `EINVAL` on Linux.
    let flags = u32::from_le_bytes(flag_word);
    if flags & !KNOWN_SETUP_FLAGS != 0 {
        return err::EINVAL;
    }
    if flags != 0 {
        return err::EOPNOTSUPP;
    }
    if crate::vfs::io_uring_abi::sq_entries_for(entries).is_none() {
        return err::EINVAL;
    }
    let Some(ring) = crate::vfs::io_uring::IoUring::build(entries) else {
        // The system-wide ring cap, or the frame allocator: the descriptor
        // table's own refusal, the shape `inotify_init1` uses for its cap.
        return err::ENOMEM;
    };
    let words = crate::vfs::io_uring_abi::params_words(ring.sq_entries(), ring.cq_entries());
    let bytes = unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, PARAMS_BYTES) };
    if !crate::sched::write_user(params, bytes) {
        return err::EFAULT;
    }
    let inode: alloc::sync::Arc<dyn crate::vfs::Inode> = ring;
    let fd = match crate::vfs::open_inode(inode) {
        Ok(fd) => fd,
        Err(e) => return err_of_vfs(e),
    };
    // Measured: a ring descriptor reports `O_RDWR` to `fcntl(F_GETFL)`.
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        file.set_access(crate::vfs::file::ACC_RDWR);
    }
    fd as isize
}

const PARAMS_BYTES: usize = 120;

/// Linux's `IORING_SETUP_*` mask: bits 0..=20 are known today. A caller's bit
/// beyond it did not come from a header this kernel knows.
const KNOWN_SETUP_FLAGS: u32 = 0x001f_ffff;

/// `io_uring_enter(fd, to_submit, min_complete, flags, sig, sigsz)` — submit
/// SQEs, execute them synchronously, and post their completions.
pub fn handle_io_uring_enter(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let to_submit = args[1] as u32;
    let min_complete = args[2] as u32;
    let flags = args[3] as u32;

    let Ok(file) = crate::sched::current_fds().clone_at(fd) else {
        return err::EBADF;
    };
    let inode = file.inode();
    let Some(ring) = inode.as_io_uring() else {
        // A valid descriptor that is not a ring: Linux answers `EOPNOTSUPP`
        // here (measured), not `EBADF`.
        return err::EOPNOTSUPP;
    };
    if flags & !IORING_ENTER_GETEVENTS != 0 {
        return err::EINVAL;
    }

    let mut submitted: u32 = 0;
    while submitted < to_submit && ring.sq_pending() > 0 && ring.cq_room() > 0 {
        let pos = ring.sq_head();
        let sqe = ring.sqe_at(pos);
        let user_data = sqe_u64(&sqe, 32);
        let res = run_sqe(&sqe);
        ring.post(user_data, res);
        ring.set_sq_head(pos.wrapping_add(1));
        submitted += 1;
    }

    if flags & IORING_ENTER_GETEVENTS != 0 && min_complete > 0 && ring.cq_pending() == 0 {
        // Waited-after-submit cannot reach here with work done (a synchronous
        // completion would already be in the CQ); this is the caller that
        // submitted nothing and asks to wait for an event only another task
        // on this ring can post.
        if !ring.wait_for_completion() {
            return err::EINTR;
        }
    }
    submitted as isize
}

/// Execute one SQE. The return value is exactly the CQE's `res`: the syscall
/// handler's answer, so the ring's completion cannot disagree with what the
/// blocking door would have said.
fn run_sqe(sqe: &[u8]) -> i32 {
    let opcode = sqe[0];
    if opcode >= crate::vfs::io_uring_abi::IORING_OP_LAST {
        // Not an opcode any Linux has: the measured answer is `EINVAL`, not
        // the `EOPNOTSUPP` a real-but-unserved request gets.
        return err::EINVAL as i32;
    }
    let flags = sqe[1];
    if flags & !IOSQE_IO_DRAIN != 0 {
        return complete(err::EOPNOTSUPP);
    }
    let fd = sqe_u32(sqe, 4) as i32;
    let off = sqe_u64(sqe, 8);
    let addr = sqe_u64(sqe, 16) as usize;
    let len = sqe_u32(sqe, 24) as usize;
    let op_flags = sqe_u32(sqe, 28);
    match opcode {
        IORING_OP_NOP => 0,
        IORING_OP_READ => {
            if off == u64::MAX {
                complete(super::fs::file::handle_read(
                    [fd as usize, addr, len, 0, 0, 0],
                    true,
                ))
            } else if off > i64::MAX as u64 {
                complete(err::EINVAL)
            } else {
                complete(super::fs::file::handle_pread64(
                    [fd as usize, addr, len, off as usize, 0, 0],
                    true,
                ))
            }
        }
        IORING_OP_WRITE => {
            if off == u64::MAX {
                complete(super::fs::file::handle_write(
                    [fd as usize, addr, len, 0, 0, 0],
                    true,
                ))
            } else if off > i64::MAX as u64 {
                complete(err::EINVAL)
            } else {
                complete(super::fs::file::handle_pwrite64(
                    [fd as usize, addr, len, off as usize, 0, 0],
                    true,
                ))
            }
        }
        IORING_OP_READV => {
            if off == u64::MAX {
                complete(super::fs::file::handle_readv(
                    [fd as usize, addr, len, 0, 0, 0],
                    true,
                ))
            } else if off > i64::MAX as u64 {
                complete(err::EINVAL)
            } else {
                complete(super::fs::file::handle_preadv(
                    [fd as usize, addr, len, off as usize, 0, 0],
                    true,
                ))
            }
        }
        IORING_OP_WRITEV => {
            if off == u64::MAX {
                complete(super::fs::file::handle_writev(
                    [fd as usize, addr, len, 0, 0, 0],
                    true,
                ))
            } else if off > i64::MAX as u64 {
                complete(err::EINVAL)
            } else {
                complete(super::fs::file::handle_pwritev(
                    [fd as usize, addr, len, off as usize, 0, 0],
                    true,
                ))
            }
        }
        IORING_OP_FSYNC => complete(super::fs::file::handle_fsync([
            fd as usize,
            op_flags as usize,
            0,
            0,
            0,
            0,
        ])),
        IORING_OP_CLOSE => complete(super::fs::file::handle_close([fd as usize, 0, 0, 0, 0, 0])),
        // Every other opcode is a promise this kernel does not keep: refused
        // per item, not as an invalid opcode.
        _ => complete(err::EOPNOTSUPP),
    }
}

/// `io_uring_register(fd, opcode, arg, nr_args)` — refused, every op.
///
/// The registration ABI exists to hand the ring fixed buffers, files and
/// eventfds. This ring resolves each SQE's descriptor and buffer when it runs
/// it, so a registration would be a promise about *later* SQEs that this
/// implementation has nowhere to keep - `EOPNOTSUPP` is the honest answer, and
/// the caller's fallback (per-op descriptors) is exactly what works.
pub fn handle_io_uring_register(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let Ok(file) = crate::sched::current_fds().clone_at(fd) else {
        return err::EBADF;
    };
    let inode = file.inode();
    if inode.as_io_uring().is_none() {
        // Measured: a non-ring descriptor is `EOPNOTSUPP`, a bad one `EBADF`.
        return err::EOPNOTSUPP;
    }
    err::EOPNOTSUPP
}

/// The VFS error vocabulary's errno for a handler that built a file.
fn err_of_vfs(error: crate::vfs::VfsError) -> isize {
    match error {
        crate::vfs::VfsError::NoSpace => err::ENOMEM,
        crate::vfs::VfsError::TooManyDescriptors => err::EMFILE,
        _ => err::EIO,
    }
}
