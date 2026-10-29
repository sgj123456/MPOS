//! t-secretmem - the memory the kernel itself cannot read, and the one number
//! whose honest answer here is `ENOSYS`.
//!
//! `memfd_secret(2)` (447 on both machines) is not "a memfd with a flag". Its
//! contract is a *property*: the pages are never placed in the kernel's direct
//! map, so no kernel path can read them, they are never written to a core dump,
//! and no mapping of them can be made executable. That property is the syscall;
//! the descriptor is bookkeeping around it.
//!
//! Measured on Linux 7.2.11 first, as an ordinary user (`build/scratch/secretmem/
//! probe{,2,3}.c`): `memfd_secret(0)` returns a descriptor, `memfd_secret(1)` and
//! `(2)` are `EINVAL`; the descriptor is an `O_RDWR` file with mode `0600` whose
//! `/proc/self/fd` link reads `/secretmem (deleted)`, and it does **not** carry
//! `FD_CLOEXEC` - a caller sets that itself. `ftruncate` fixes the size once
//! (`4096` -> 0, then `8192` or `0` -> `EINVAL`), and `fstat` then reports a
//! regular file of that size. `mmap` is `MAP_SHARED` only (`MAP_PRIVATE` is
//! `EINVAL`) and needs no size first - an unmapped-beyond-the-end mapping is
//! legal and faults with `SIGBUS` on touch. `mprotect` may make a mapping
//! read-only, writable again or `PROT_NONE`, but adding `PROT_EXEC` is `EACCES`;
//! the permission is the *mapping's*, so one read-only mapping does not stop a
//! sibling mapping from writing. Reading or writing the descriptor is `EINVAL`,
//! `pread`/`pwrite`/`lseek` are `ESPIPE` (it is not seekable), `F_ADD_SEALS` is
//! `EINVAL`, and a forked child inherits both the descriptor and the mapping and
//! sees the same bytes.
//!
//! Why this kernel refuses it: MPOS's memory model has one direct map of all
//! RAM and every frame reachable through it - that is how the kernel copies user
//! buffers, how the block and network drivers reach payloads, and how the
//! allocator and every `Vec` behind a filesystem work. A page this kernel can
//! always read cannot be secret, so serving the call would hand a caller a
//! descriptor whose name promises a property the machine does not have, which is
//! the one thing `AGENTS.md` says is worse than refusing. `ENOSYS` is also
//! *Linux's own* answer for a kernel built without `CONFIG_SECRETMEM`: the
//! syscall number exists, the machine has no such memory, and the caller is told
//! exactly that.
//!
//! The miss is then not the answer but the record: this program pins the
//! refusal. On the guest every call is `ENOSYS`, whatever the flags, and that is
//! the claim. A future kernel that grows non-direct-mapped frames serves it and
//! the same program measures Linux's shape instead - which is what the host
//! branch does today, so `MPOS_ON_HOST=1` is this file's reference direction
//! rather than a special case.

use mpos_user_tests::{Report, host_run};

/// `memfd_secret(2)`'s number on x86_64 and aarch64 alike.
const SYS_MEMFD_SECRET: libc::c_long = 447;

/// The call, with the negative errno kept as an errno.
fn memfd_secret(flags: u32) -> Result<i32, i32> {
    let r = unsafe { libc::syscall(SYS_MEMFD_SECRET, flags as libc::c_long) };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(r as i32)
    }
}

/// `mmap` of `len` bytes at `offset`, answering the address or the errno.
fn map(fd: i32, len: usize, prot: i32, flags: i32, offset: i64) -> Result<*mut libc::c_void, i32> {
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            prot,
            flags,
            fd,
            offset as libc::off_t,
        )
    };
    if p == libc::MAP_FAILED {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(p)
    }
}

/// `mprotect`, answering the errno on refusal.
fn protect(p: *mut libc::c_void, len: usize, prot: i32) -> Result<(), i32> {
    let r = unsafe { libc::mprotect(p, len, prot) };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(())
    }
}

fn main() {
    let mut r = Report::new();
    let first = memfd_secret(0);

    match first {
        // This kernel: the number exists and the machine has no such memory.
        Err(libc::ENOSYS) => {
            r.check(
                "memfd_secret(2) is absent here, and says so with ENOSYS",
                true,
            );
            r.check(
                "the absence is not a flag question: hidden flags are ENOSYS too",
                matches!(memfd_secret(1), Err(libc::ENOSYS))
                    && matches!(memfd_secret(0x8000_0000), Err(libc::ENOSYS)),
            );
            // A descriptor number would be the lie this refusal exists to avoid;
            // there is nothing further to ask.
        }
        // A kernel that serves the call: measure Linux's shape.
        Ok(fd) => {
            r.check("a descriptor comes back", fd >= 0);
            r.check(
                "the flags word is not a union of useful bits: 1 and 2 are refused",
                matches!(memfd_secret(1), Err(libc::EINVAL))
                    && matches!(memfd_secret(2), Err(libc::EINVAL)),
            );
            let mut st = unsafe { core::mem::zeroed::<libc::stat>() };
            let stat_ok = unsafe { libc::fstat(fd, &mut st) } == 0;
            r.check(
                "it is a regular file only its owner may touch (0600)",
                stat_ok && (st.st_mode & 0o777) == 0o600,
            );
            r.check(
                "the create call leaves FD_CLOEXEC off - the caller must set it",
                unsafe { libc::fcntl(fd, libc::F_GETFD) } == 0,
            );
            r.check(
                "the size is fixed once: the first ftruncate takes, the second is EINVAL",
                unsafe { libc::ftruncate(fd, 4096) } == 0
                    && unsafe { libc::ftruncate(fd, 8192) } == -1,
            );
            r.check(
                "it maps shared only: MAP_PRIVATE is EINVAL",
                matches!(
                    map(
                        fd,
                        4096,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE,
                        0
                    ),
                    Err(libc::EINVAL)
                ),
            );
            match map(
                fd,
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                0,
            ) {
                Ok(p) => {
                    unsafe { *(p as *mut u8) = 0x5a };
                    r.check("a shared mapping can be written and read", unsafe {
                        *(p as *const u8) == 0x5a
                    });
                    r.check(
                        "no mapping of it may become executable: PROT_EXEC is EACCES",
                        matches!(
                            protect(p, 4096, libc::PROT_READ | libc::PROT_EXEC),
                            Err(libc::EACCES)
                        ),
                    );
                    r.check(
                        "read-only, then writable again: the permission is the mapping's",
                        protect(p, 4096, libc::PROT_READ).is_ok()
                            && protect(p, 4096, libc::PROT_READ | libc::PROT_WRITE).is_ok(),
                    );
                    unsafe { libc::munmap(p, 4096) };
                }
                Err(_) => r.check("a shared mapping succeeds", false),
            }
            let mut one = [0u8; 1];
            let read_rc = unsafe { libc::read(fd, one.as_mut_ptr() as *mut libc::c_void, 1) };
            let read_err = std::io::Error::last_os_error().raw_os_error();
            r.check(
                "it is not a byte stream: read is EINVAL",
                read_rc == -1 && read_err == Some(libc::EINVAL),
            );
            let pread_rc = unsafe { libc::pread(fd, one.as_mut_ptr() as *mut libc::c_void, 1, 0) };
            let pread_err = std::io::Error::last_os_error().raw_os_error();
            r.check(
                "and the offset calls have no stream to seek: pread is ESPIPE",
                pread_rc == -1 && pread_err == Some(libc::ESPIPE),
            );
            unsafe { libc::close(fd) };
        }
        // Any other refusal is a shape this program does not know.
        Err(e) => {
            r.check(
                "memfd_secret(2) answers ENOSYS or creates a descriptor",
                false,
            );
            println!("(unexpected errno {e})");
        }
    }

    if host_run() {
        println!("(host run: the branch above measured Linux's own answer)");
    }
    r.finish()
}
