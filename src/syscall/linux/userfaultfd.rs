//! `userfaultfd(2)` and its `UFFDIO_*` requests.
//!
//! The descriptor is an anon inode whose `Inode` face is
//! [`crate::vfs::userfaultfd::UserfaultFd`]; the machinery behind it (ranges,
//! the pagefault queue, the parking) is [`crate::sched::userfault`], because
//! the fault path calls it and `sched` may not reach up into `vfs`.
//!
//! Every shape below is Linux's, measured as real root (`build/scratch/uffd/`
//! probes; the record is `docs/notes/06-process-management.md` §6.33), and the
//! arithmetic that decides it lives in [`crate::vfs::userfaultfd_abi`], where a
//! host test holds it to the measurement.

use crate::syscall::linux::abi;
use crate::syscall::{ARG_COUNT, buffer_ok, err, err_of};
use crate::vfs::userfaultfd::UserfaultFd;
use crate::vfs::userfaultfd_abi::*;

/// `userfaultfd(2)`: a descriptor that will carry this process's pagefaults.
///
/// The flags are `O_CLOEXEC`, `O_NONBLOCK` and `UFFD_USER_MODE_ONLY`, and
/// nothing else: measured, `0x2` and `0x40000000` are `EINVAL` while `0x1` and
/// `0x801` are accepted. `UFFD_USER_MODE_ONLY` is a promise about *which*
/// faults to forward (user-mode only, never a kernel copy); this kernel only
/// ever forwards a user task's own faults, so honouring it is automatic - and
/// it is still refused when combined with anything unknown, as measured.
pub(super) fn handle_userfaultfd(args: [usize; ARG_COUNT]) -> isize {
    let flags = args[0];
    const O_CLOEXEC: usize = 0o200_0000;
    const O_NONBLOCK: usize = 0o4000;
    /// `UFFD_USER_MODE_ONLY`.
    const USER_MODE_ONLY: usize = 1 << 0;
    const KNOWN: usize = O_CLOEXEC | O_NONBLOCK | USER_MODE_ONLY;
    if flags & !KNOWN != 0 {
        return err::EINVAL;
    }
    let nonblocking = flags & O_NONBLOCK != 0;
    let Some(inode) = UserfaultFd::new(nonblocking) else {
        // The machine's cap on live descriptors, which is a resource answer
        // rather than a shape one.
        return err::EMFILE;
    };
    let fd = match crate::vfs::open_inode(inode) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    // `F_GETFD` answers CLOEXEC and `F_GETFL` answers O_RDWR for every shape
    // measured, so the descriptor is read-write and the flag is the caller's.
    if flags & O_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, abi::FD_CLOEXEC);
    }
    fd as isize
}

/// `UFFDIO_API`: the handshake that says which API and features the caller
/// speaks. `EINVAL` for a wrong API number, for a feature bit this kernel does
/// not report, and for a second handshake - all three measured.
pub(super) fn api_ioctl(inode: &UserfaultFd, buf: usize, from_user: bool) -> isize {
    if !buffer_ok(buf, API_BYTES, true, from_user) {
        return err::EFAULT;
    }
    let mut bytes = [0u8; API_BYTES];
    if !crate::sched::read_user(buf, &mut bytes) {
        return err::EFAULT;
    }
    let api = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let features = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if !inode.object().mark_api_done() {
        return err::EINVAL;
    }
    if !api_accepts(api, features) {
        return err::EINVAL;
    }
    // The answer: the API the kernel speaks, the features it reports, and the
    // request words it serves on this descriptor.
    let mut out = [0u8; API_BYTES];
    out[0..8].copy_from_slice(&UFFD_API.to_le_bytes());
    out[8..16].copy_from_slice(&FEATURES.to_le_bytes());
    out[16..24].copy_from_slice(&API_IOCTLS.to_le_bytes());
    if !crate::sched::write_user(buf, &out) {
        return err::EFAULT;
    }
    0
}

/// `UFFDIO_REGISTER`: claim a span of pages, in `MODE_MISSING` only.
pub(super) fn register_ioctl(inode: &UserfaultFd, buf: usize, from_user: bool) -> isize {
    if !buffer_ok(buf, REGISTER_BYTES, true, from_user) {
        return err::EFAULT;
    }
    let mut bytes = [0u8; REGISTER_BYTES];
    if !crate::sched::read_user(buf, &mut bytes) {
        return err::EFAULT;
    }
    let start = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let mode = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let page = crate::mm::PAGE_SIZE as u64;
    match register_verdict(mode, start, len, page) {
        RegisterVerdict::Serve => {}
        RegisterVerdict::Unsupported => return err::EOPNOTSUPP,
        RegisterVerdict::Invalid => return err::EINVAL,
    }
    // The span must be mapped: a range with no VMA behind it is `EINVAL`
    // (measured). The question is the address space's.
    if !crate::sched::range_is_mapped(start as usize, (start + len) as usize) {
        return err::EINVAL;
    }
    if !inode
        .object()
        .add_range(start as usize, (start + len) as usize)
    {
        return err::ENOMEM;
    }
    // The answer: which requests work on this range. The ioctls word rides at
    // offset 24, after the range and the mode.
    let mut out = bytes;
    out[24..32].copy_from_slice(&RANGE_IOCTLS.to_le_bytes());
    if !crate::sched::write_user(buf, &out) {
        return err::EFAULT;
    }
    0
}

/// `UFFDIO_UNREGISTER`: give a span back. Measured: 0, even for a span that was
/// never registered.
pub(super) fn unregister_ioctl(inode: &UserfaultFd, buf: usize, from_user: bool) -> isize {
    if !buffer_ok(buf, RANGE_BYTES, false, from_user) {
        return err::EFAULT;
    }
    let mut bytes = [0u8; RANGE_BYTES];
    if !crate::sched::read_user(buf, &mut bytes) {
        return err::EFAULT;
    }
    let start = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    inode
        .object()
        .remove_range(start as usize, (start + len) as usize);
    0
}

/// `UFFDIO_WAKE`: wake the tasks parked on the pages in this span. Measured as
/// 0 whether or not anything was parked.
pub(super) fn wake_ioctl(inode: &UserfaultFd, buf: usize, from_user: bool) -> isize {
    if !buffer_ok(buf, RANGE_BYTES, false, from_user) {
        return err::EFAULT;
    }
    let mut bytes = [0u8; RANGE_BYTES];
    if !crate::sched::read_user(buf, &mut bytes) {
        return err::EFAULT;
    }
    let start = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    inode
        .object()
        .wake_span(start as usize, (start + len) as usize);
    0
}

/// `UFFDIO_COPY`: here is the page the fault was about.
///
/// The whole error surface is measured: `EEXIST` over a present page,
/// `ENOENT` for a span this descriptor did not register, `EFAULT` for an
/// unreadable source, `EINVAL` for a partial page or an unknown mode bit.
pub(super) fn copy_ioctl(inode: &UserfaultFd, buf: usize, from_user: bool) -> isize {
    if !buffer_ok(buf, COPY_BYTES, true, from_user) {
        return err::EFAULT;
    }
    let mut bytes = [0u8; COPY_BYTES];
    if !crate::sched::read_user(buf, &mut bytes) {
        return err::EFAULT;
    }
    let dst = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let src = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let mode = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    if !copy_len_ok(len, crate::mm::PAGE_SIZE as u64) {
        return err::EINVAL;
    }
    match copy_mode_verdict(mode) {
        CopyModeVerdict::Serve => {}
        CopyModeVerdict::Unsupported => return err::EOPNOTSUPP,
        CopyModeVerdict::Invalid => return err::EINVAL,
    }
    if !inode
        .object()
        .covers_span(dst as usize, (dst + len) as usize)
    {
        return err::ENOENT;
    }
    // The source is an ordinary user buffer, which is why an unaligned one is
    // accepted (measured).
    if !buffer_ok(src as usize, len as usize, false, from_user) {
        return err::EFAULT;
    }
    let mut page = alloc::vec![0u8; len as usize];
    if !crate::sched::read_user(src as usize, &mut page) {
        return err::EFAULT;
    }
    let dontwake = mode & COPY_MODE_DONTWAKE != 0;
    if let Err(e) = crate::sched::userfault::fill_page(dst as usize, &page, dontwake) {
        return e;
    }
    // The answer word `copy` rides at offset 32; Linux fills it with the bytes
    // it copied, and so does this.
    let mut out = bytes;
    out[32..40].copy_from_slice(&len.to_le_bytes());
    if !crate::sched::write_user(buf, &out) {
        return err::EFAULT;
    }
    0
}

/// `UFFDIO_ZEROPAGE`: a zero page will do. `EEXIST` over a present page,
/// `ENOENT` outside the registered span (both measured).
pub(super) fn zeropage_ioctl(inode: &UserfaultFd, buf: usize, from_user: bool) -> isize {
    if !buffer_ok(buf, ZEROPAGE_BYTES, true, from_user) {
        return err::EFAULT;
    }
    let mut bytes = [0u8; ZEROPAGE_BYTES];
    if !crate::sched::read_user(buf, &mut bytes) {
        return err::EFAULT;
    }
    let dst = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let mode = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    if !copy_len_ok(len, crate::mm::PAGE_SIZE as u64) || mode != 0 {
        return err::EINVAL;
    }
    if !inode
        .object()
        .covers_span(dst as usize, (dst + len) as usize)
    {
        return err::ENOENT;
    }
    if let Err(e) = crate::sched::userfault::fill_zero_page(dst as usize, len as usize) {
        return e;
    }
    let mut out = bytes;
    out[24..32].copy_from_slice(&len.to_le_bytes());
    if !crate::sched::write_user(buf, &out) {
        return err::EFAULT;
    }
    0
}
