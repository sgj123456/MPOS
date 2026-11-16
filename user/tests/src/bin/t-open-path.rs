//! t-open-path - `O_PATH`: a descriptor that **names** its subject and does not
//! open it. The whole contract is what *other* calls answer on one.
//!
//! The report that started this is the shape item 297 found: this kernel recorded
//! `O_PATH` (so `F_GETFL` reported the bit) and never acted on it, so `read(2)`
//! on an `O_PATH` descriptor **succeeded** - the one thing such a descriptor
//! exists to prevent. A flag that is accepted, reported back and enforced nowhere
//! is worse than one refused, because the caller has been told it holds
//! something it does not.
//!
//! Measured whole on Linux 7.3.4 (`build/scratch/opath/{t,combos}.c`,
//! `docs/notes/` §6.177) before any claim was written:
//!
//! | what | Linux |
//! |---|---|
//! | `read`/`write` on an `O_PATH` descriptor | **`EBADF`**, both |
//! | `fstat` on one | works, and reports the real size |
//! | `F_GETFL` on one | carries `O_PATH` |
//! | `F_DUPFD` on one | works |
//! | an `O_PATH` open's effect on `atime` | none |
//! | `O_PATH\|O_NOFOLLOW` on a symlink | **served**, and it is the *link* |
//! | `O_PATH` alone on a symlink | served, and it is the *target* |
//! | `O_RDONLY\|O_NOFOLLOW` on a symlink | `ELOOP` |
//! | `O_PATH\|O_RDONLY\|O_NOFOLLOW` on a symlink | served, the *link* |
//! | `O_DIRECTORY\|O_NOFOLLOW` on a symlink | `ENOTDIR`, the kind asked first |
//!
//! The last two rows are the pair that a one-line `O_NOFOLLOW` check gets wrong:
//! the flag is not "refuse a link", it is "do not *traverse* one", and `O_PATH` is
//! what lets a caller hold the link itself - which is how `/proc`-style doors hand
//! out a descriptor on a link without following it.
//!
//! # The second half: the doors that read the inode directly
//!
//! Items 299 and 300 closed the *path-shaped* facts and `fcntl`'s table. Item 303
//! found the rest and it was the larger half: nine calls reach
//! `Inode::read_at`/`write_at` (or the lock table) **without** passing
//! `File::pre_read`/`pre_write`, so the predicate those two carry never ran for
//! them. `mmap`, `readahead`, `sync_file_range`, `fsync`, `fdatasync`,
//! `sendfile`, `copy_file_range`, `splice`, `flock` and `fallocate` are all
//! **`EBADF`** on Linux for an `O_PATH` descriptor
//! (`build/scratch/opathmmap/`, eight probes) - and the refusals are
//! **authority**, not tidiness: `open(path, O_PATH)` asks no permission on the
//! file, so a descriptor can name a file the caller cannot open. This kernel
//! served six of them, copied 16 bytes through one, and *grew a file* through
//! another.
//!
//! The same section pins the `F_GETFL` **fold**: Linux's `build_open_flags` keeps
//! only `O_DIRECTORY|O_NOFOLLOW|O_PATH` and sets `acc_mode = 0`, so an
//! `O_PATH|O_TRUNC` open reports no `O_TRUNC` and truncates nothing.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::os::unix::fs::PermissionsExt;

/// The fixture directory and the file/link inside it.
const DIR: &str = "/tmp/t-open-path";
const FILE: &str = "/tmp/t-open-path/file";
const LINK: &str = "/tmp/t-open-path/link";

fn main() {
    println!("t-open-path");
    let mut r = Report::new();

    let _ = std::fs::remove_dir_all(DIR);
    let made = std::fs::create_dir_all(DIR);
    r.check(
        &format!("the fixture directory is made ({made:?})"),
        made.is_ok(),
    );
    if made.is_err() {
        r.finish();
    }
    r.check(
        "the fixture file is written",
        std::fs::write(FILE, b"hello").is_ok(),
    );
    // SAFETY: two NUL-terminated paths this test owns.
    let linked = unsafe {
        let (mut t, mut l) = (Vec::from(FILE), Vec::from(LINK));
        t.push(0);
        l.push(0);
        libc::symlink(
            t.as_ptr().cast::<libc::c_char>(),
            l.as_ptr().cast::<libc::c_char>(),
        ) == 0
    };
    r.check("the fixture symlink is made", linked);

    // === the descriptor names and does not open =============================
    //
    // `O_PATH` is `0o10000000` on x86_64 and a different bit on aarch64, so it
    // comes from the libc rather than from a literal - and the *pair* with
    // `O_NOFOLLOW` is spelled here too, since that is the combination whose
    // meaning is the point.
    let path_flags = OFlags::from_bits_retain(libc::O_PATH as u32);
    let nofollow = OFlags::from_bits_retain(libc::O_NOFOLLOW as u32);

    match rustix::fs::open(FILE, path_flags, Mode::empty()) {
        Ok(fd) => {
            r.check("open(file, O_PATH) succeeds", true);
            // The two doors that must refuse, and the one that must not.
            let rd = rustix::io::read(&fd, &mut [0u8; 8]);
            r.check(
                &format!("read on it is EBADF, got {rd:?}"),
                rd.as_ref().err().copied() == Some(Errno::BADF),
            );
            let wr = rustix::io::write(&fd, b"x");
            r.check(
                &format!("write on it is EBADF, got {wr:?}"),
                wr.as_ref().err().copied() == Some(Errno::BADF),
            );
            let st = rustix::fs::fstat(&fd);
            r.check(
                &format!(
                    "fstat still works and reports the file's size ({})",
                    st.as_ref().map(|s| s.st_size).unwrap_or(-1)
                ),
                st.is_ok_and(|s| s.st_size == 5),
            );
            let fl = rustix::fs::fcntl_getfl(&fd);
            r.check(
                &format!("F_GETFL carries O_PATH, got {fl:?}"),
                fl.is_ok_and(|f| f.bits() as i32 & libc::O_PATH != 0),
            );
            // And it carries **only** what the fold keeps. Linux's
            // `build_open_flags` does `flags &= O_DIRECTORY | O_NOFOLLOW |
            // O_PATH` for an `O_PATH` open and sets `acc_mode = 0`, so the
            // access bits, `O_APPEND`, `O_NONBLOCK`, `O_SYNC`, `O_TRUNC` and
            // `O_CREAT` are all dropped: measured on Linux 7.3.4
            // (`build/scratch/opathmmap/fold.c`), opening one file with
            // `O_PATH|O_APPEND|O_NONBLOCK|O_SYNC|O_TRUNC|O_NOFOLLOW` reports
            // `F_GETFL = 0x220000` - `O_PATH|O_NOFOLLOW` alone. This kernel
            // reported the whole word *plus* the access bits (`0x208001` for
            // `O_PATH|O_WRONLY`, measured on the guest), which is how `F_GETFL`
            // came to advertise a writable descriptor that had opened nothing.
            //
            // `O_CLOEXEC` is not part of this word at all - it is a *descriptor*
            // flag, so it survives as `FD_CLOEXEC` and is claimed separately.
            let folded = rustix::fs::open(
                FILE,
                OFlags::PATH | OFlags::APPEND | OFlags::NONBLOCK | OFlags::TRUNC,
                Mode::empty(),
            );
            if let Ok(f) = &folded {
                let g = rustix::fs::fcntl_getfl(f);
                let keep = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW;
                let got = g.as_ref().map(|g| g.bits() as i32).unwrap_or(-1);
                r.check(
                    &format!(
                        "F_GETFL on O_PATH|APPEND|NONBLOCK|TRUNC keeps only the fold's bits \
                         (O_PATH|O_DIRECTORY|O_NOFOLLOW), got 0x{got:x}",
                    ),
                    got >= 0 && got & !keep == 0 && got & libc::O_PATH != 0,
                );
                let size = std::fs::metadata(FILE).map(|m| m.len()).unwrap_or(0);
                r.check(
                    &format!("and the O_TRUNC beside it truncated nothing (size {size})"),
                    size == 5,
                );
            }
            let dup = rustix::io::dup(&fd);
            r.check(
                &format!("F_DUPFD on an O_PATH descriptor works, got {dup:?}"),
                dup.is_ok(),
            );
        }
        Err(e) => r.check(&format!("open(file, O_PATH) is served, got {e:?}"), false),
    }

    // `O_PATH` does not count as an access, so it must not move `atime`. The
    // comparison is against the stamp the *test* read a moment earlier, so a
    // filesystem whose clock has one-second resolution cannot make this a
    // coin flip: only a *moved* stamp reddens it.
    {
        let before = std::fs::metadata(FILE).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.atime()
        });
        let opened = rustix::fs::open(FILE, path_flags, Mode::empty());
        r.check("a second O_PATH open succeeds", opened.is_ok());
        drop(opened);
        let after = std::fs::metadata(FILE).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.atime()
        });
        r.check(
            &format!("and it did not move atime ({before:?} -> {after:?})"),
            before == after,
        );
    }

    // === O_NOFOLLOW traverses nothing, and O_PATH is how you hold the link ===
    if linked {
        let rdonly = rustix::fs::open(LINK, OFlags::RDONLY | nofollow, Mode::empty());
        r.check(
            &format!("O_RDONLY|O_NOFOLLOW on a symlink is ELOOP, got {rdonly:?}"),
            rdonly.as_ref().err().copied() == Some(Errno::LOOP),
        );

        match rustix::fs::open(LINK, path_flags | nofollow, Mode::empty()) {
            Ok(fd) => {
                r.check("O_PATH|O_NOFOLLOW on a symlink is served", true);
                let st = rustix::fs::fstat(&fd);
                r.check(
                    "and the descriptor is the LINK itself",
                    st.is_ok_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFLNK),
                );
            }
            Err(e) => r.check(
                &format!("O_PATH|O_NOFOLLOW on a symlink is served, got {e:?}"),
                false,
            ),
        }

        // The neighbour that makes the claim above about `O_PATH` rather than
        // about `O_NOFOLLOW`: `O_PATH` alone follows to the target.
        match rustix::fs::open(LINK, path_flags, Mode::empty()) {
            Ok(fd) => {
                let st = rustix::fs::fstat(&fd);
                r.check(
                    "O_PATH alone follows to the target, not the link",
                    st.is_ok_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFREG),
                );
            }
            Err(e) => r.check(&format!("O_PATH alone is served, got {e:?}"), false),
        }

        // And the kind question before the link one: a link is not a directory.
        let dir_nofollow = rustix::fs::open(
            LINK,
            OFlags::from_bits_retain(libc::O_DIRECTORY as u32) | nofollow,
            Mode::empty(),
        );
        r.check(
            &format!("O_DIRECTORY|O_NOFOLLOW on a symlink is ENOTDIR, got {dir_nofollow:?}"),
            dir_nofollow.as_ref().err().copied() == Some(Errno::NOTDIR),
        );
    }

    // === `O_PATH` as a *dirfd*: the flag's largest use ======================
    //
    // A descriptor opened `O_PATH` on a **directory** is exactly what an `*at`
    // door wants as its dirfd: it names the directory without opening it. All
    // of them are served through one - measured on Linux 7.3.4
    // (`build/scratch/opathfd/{t,fin}.c`, `docs/notes/` §6.178) - and the
    // *reverse* holds too: the doors that would touch the subject's contents
    // refuse it (`fchmod`, `getdents64`, `read`).
    //
    // Every raw door here, not the libc wrapper, because two of these have
    // userspace fallbacks that hide the kernel's answer: musl's `fchmod()`
    // calls `fchmodat(AT_FDCWD, "/proc/self/fd/N")` when the direct call fails,
    // so on **both** machines `fchmod(O_PATH_fd)` returns 0 and changes the mode
    // while the raw call refuses. A probe that measured the wrapper would have
    // "found" a divergence that is musl's, and a pin would have pinned it.
    let dirfd_flags = OFlags::from_bits_retain(libc::O_PATH as u32);
    match rustix::fs::open(DIR, dirfd_flags, Mode::empty()) {
        Ok(dirfd) => {
            let raw = std::os::fd::AsRawFd::as_raw_fd(&dirfd);
            // The doors that must be *served* through it.
            let reopened = rustix::fs::openat(&dirfd, "file", OFlags::RDONLY, Mode::empty());
            r.check("openat(O_PATH dirfd, \"file\") is served", reopened.is_ok());
            // `fstatat`, `fchmodat` and `unlinkat` are the three that a
            // `O_PATH` dirfd is most often handed to; each through its own raw
            // door so nothing in the libc can answer for the kernel.
            let st = rustix::fs::statat(&dirfd, "file", AtFlags::empty());
            r.check(
                &format!("fstatat(O_PATH dirfd, \"file\") is served, got {st:?}"),
                st.is_ok(),
            );

            // The doors that must be **refused** on the descriptor itself.
            // SAFETY: `raw` is a live descriptor this test owns, and the
            // arguments are a mode, a buffer and a length.
            let fchmod_raw = unsafe { libc::syscall(libc::SYS_fchmod, raw, 0o700) };
            r.check(
                &format!(
                    "fchmod(O_PATH fd) is EBADF (raw), got rc={fchmod_raw} errno={}",
                    std::io::Error::last_os_error()
                ),
                fchmod_raw == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
            );
            let mut dirents = [0u8; 512];
            // SAFETY: as above, with a buffer this test owns.
            let getdents = unsafe {
                libc::syscall(
                    libc::SYS_getdents64,
                    raw,
                    dirents.as_mut_ptr().cast::<libc::c_void>(),
                    dirents.len(),
                )
            };
            r.check(
                &format!("getdents64(O_PATH fd) is EBADF (raw), got rc={getdents}"),
                getdents == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
            );
        }
        Err(e) => r.check(&format!("open(dir, O_PATH) is served, got {e:?}"), false),
    }

    // === every other door that takes the descriptor =========================
    //
    // Linux refuses **all** of them for an `O_PATH` descriptor, and each with
    // `EBADF` rather than whatever the door would say about its own arguments.
    // Measured raw on 7.3.4 (`build/scratch/opathrest/t.c`): `fchown`, the four
    // `f*xattr` doors, `ftruncate`, `fallocate`, `fadvise64` and `ioctl` are all
    // `EBADF`, while `fstat` and `fchdir` are served. This kernel served seven of
    // the nine - including `fchown`, which *changed the owner*, and `fadvise64`,
    // which told the caller its advice had been taken about a file it never
    // opened - and answered `EINVAL`/`ENOTTY` from the other two, refusing a
    // legal argument for the wrong reason.
    //
    // All raw, so no libc fallback can answer for the kernel (§6.178's lesson:
    // musl's `fchmod` and `fchown` both route through `/proc/self/fd` when the
    // direct call fails).
    if let Ok(f) = rustix::fs::open(FILE, path_flags, Mode::empty()) {
        let raw = std::os::fd::AsRawFd::as_raw_fd(&f);
        let ebadf = |rc: i64| {
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
        };
        // SAFETY: `raw` is a live descriptor this test owns; every argument is a
        // buffer or a number this test made.
        let refused: [(&str, i64); 7] = unsafe {
            [
                ("fchown", libc::syscall(libc::SYS_fchown, raw, 0, 0)),
                (
                    "fgetxattr",
                    libc::syscall(
                        libc::SYS_fgetxattr,
                        raw,
                        c"user.k".as_ptr(),
                        [0u8; 8].as_mut_ptr(),
                        8,
                    ),
                ),
                (
                    "flistxattr",
                    libc::syscall(libc::SYS_flistxattr, raw, [0u8; 64].as_mut_ptr(), 64),
                ),
                (
                    "fsetxattr",
                    libc::syscall(
                        libc::SYS_fsetxattr,
                        raw,
                        c"user.k".as_ptr(),
                        c"v".as_ptr(),
                        1,
                        0,
                    ),
                ),
                (
                    "fadvise64",
                    libc::syscall(libc::SYS_fadvise64, raw, 0, 4096, 0),
                ),
                ("ftruncate", libc::syscall(libc::SYS_ftruncate, raw, 0)),
                (
                    "ioctl(TCGETS)",
                    libc::syscall(libc::SYS_ioctl, raw, 0x5401, [0u8; 64].as_mut_ptr()),
                ),
            ]
        };
        for (name, rc) in refused {
            r.check(
                &format!(
                    "{name}(O_PATH fd) is EBADF (raw), got rc={rc} errno={}",
                    std::io::Error::last_os_error()
                ),
                ebadf(rc),
            );
        }
        // The neighbour that makes the claims above about `O_PATH` rather than
        // about "this descriptor is broken": `fstat` is served on the same one.
        let st = rustix::fs::fstat(&f);
        r.check(
            &format!("fstat on the same descriptor is still served, got {st:?}"),
            st.is_ok_and(|s| s.st_size == 5),
        );

        // === `fcntl`'s own split on an `O_PATH` descriptor ==================
        //
        // `fcntl` is a table, and `O_PATH` partitions it: the commands that ask
        // *what this descriptor is* are **served** (`F_GETFD`, `F_GETFL`,
        // `F_SETFD`, `F_DUPFD*`), and the ones that would act on the file
        // behind it are `EBADF` (`F_SETFL` and the six lock commands).
        // Measured raw on Linux 7.3.4 (`build/scratch/fcntl/t.c`). This
        // kernel served `F_SETFL` - which stored a status word on a descriptor
        // that has no open file to carry one - and answered the lock commands
        // from the lock table.
        for (name, cmd) in [
            ("F_SETFL", libc::F_SETFL),
            ("F_SETLK", libc::F_SETLK),
            ("F_GETLK", libc::F_GETLK),
            ("F_OFD_SETLK", libc::F_OFD_SETLK),
        ] {
            // The lock commands take a `struct flock`; `F_SETFL` a flag word.
            let mut flock: libc::flock = unsafe { core::mem::zeroed() };
            flock.l_type = libc::F_WRLCK as i16;
            let arg: usize = if name == "F_SETFL" {
                libc::O_NONBLOCK as usize
            } else {
                (&raw mut flock) as usize
            };
            // SAFETY: `raw` is a live descriptor, and `arg` is either a flag
            // word or a live `struct flock` this test owns.
            let rc = unsafe { libc::fcntl(raw, cmd, arg) };
            r.check(
                &format!(
                    "{name}(O_PATH fd) is EBADF, got rc={rc} errno={}",
                    std::io::Error::last_os_error()
                ),
                rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
            );
        }
        // And the served half, so the claims above are about the split rather
        // than about `fcntl` refusing everything on one.
        // SAFETY: as above; `F_GETFD` takes no argument.
        let getfd = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        r.check(
            &format!("F_GETFD on the same descriptor is served, got {getfd}"),
            getfd >= 0,
        );

        // === the doors that read the inode directly =========================
        //
        // `mmap`, `readahead`, `sync_file_range`, `fsync`/`fdatasync`,
        // `sendfile`, `copy_file_range`, `splice`, `flock` and `fallocate` all
        // reach `Inode::read_at`/`write_at` (or the lock table) *without*
        // passing `File::pre_read`/`pre_write`, which is the one place item 297
        // put the `O_PATH` predicate - so each owed its own line and none had
        // one. Measured whole on Linux 7.3.4
        // (`build/scratch/opathmmap/{matrix,sweep,order6,order7,order9,
        // order10,sf,fa2}.c`): **all nine answer `EBADF`**, and the wrong access
        // *mode* is `EBADF` too (`readahead` on a write-only descriptor,
        // `splice` from one) - the same two questions `read`/`write` ask.
        //
        // The guest this kernel was measured on answered `0` from
        // `readahead`/`sync_file_range`/`fsync`, **moved bytes** from
        // `copy_file_range` (16) and `splice` (3), and **took a lock** with
        // `flock` - a descriptor whose entire purpose is to name a file being
        // one that could read, copy, flush and lock it.
        let ebadf = |rc: i64| {
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
        };
        // A destination for the two copy doors, so their source side is the
        // only `O_PATH` descriptor in the call.
        let dst = rustix::fs::open(
            "/tmp/t-open-path/copy.out",
            OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC,
            Mode::from(0o644),
        );
        let (pipe_r, pipe_w) = rustix::pipe::pipe().unwrap();
        let dst_raw = dst
            .as_ref()
            .map(std::os::fd::AsRawFd::as_raw_fd)
            .unwrap_or(-1);
        let _pr = std::os::fd::AsRawFd::as_raw_fd(&pipe_r);
        let pw = std::os::fd::AsRawFd::as_raw_fd(&pipe_w);
        // SAFETY: `raw`, `dst_raw`, `pr` and `pw` are live descriptors this test
        // owns; every other argument is a number or a buffer this test made.
        let more_refused: [(&str, i64); 9] = unsafe {
            [
                // `mmap` is the one that was *served*: a name-only descriptor
                // gave back an address whose bytes could be read.
                (
                    "mmap",
                    libc::syscall(
                        libc::SYS_mmap,
                        0usize,
                        4096usize,
                        1usize,
                        0x02usize,
                        raw,
                        0usize,
                    ),
                ),
                (
                    "readahead",
                    libc::syscall(libc::SYS_readahead, raw, 0usize, 4096usize),
                ),
                (
                    "sync_file_range",
                    libc::syscall(libc::SYS_sync_file_range, raw, 0i64, 0i64, 0u32),
                ),
                ("fsync", libc::syscall(libc::SYS_fsync, raw)),
                ("fdatasync", libc::syscall(libc::SYS_fdatasync, raw)),
                (
                    "sendfile",
                    libc::syscall(libc::SYS_sendfile, dst_raw, raw, 0usize, 16usize),
                ),
                (
                    "copy_file_range",
                    libc::syscall(
                        libc::SYS_copy_file_range,
                        raw,
                        0usize,
                        dst_raw,
                        0usize,
                        16usize,
                        0u32,
                    ),
                ),
                (
                    "splice",
                    libc::syscall(libc::SYS_splice, raw, 0usize, pw, 0usize, 16usize, 0u32),
                ),
                ("flock", libc::syscall(libc::SYS_flock, raw, libc::LOCK_SH)),
            ]
        };
        for (name, rc) in more_refused {
            r.check(
                &format!(
                    "{name}(O_PATH fd) is EBADF (raw), got rc={rc} errno={}",
                    std::io::Error::last_os_error()
                ),
                ebadf(rc),
            );
        }
        // `fallocate` gets its own entry because it was the one that **wrote**:
        // on the guest `fallocate(O_PATH|O_WRONLY_fd, 0, 0, 4096)` answered 0 and
        // grew the file from 10 bytes to 4096. The `O_PATH` word below carries
        // the access bits on purpose - an `O_PATH` descriptor with no access bits
        // already refused here by accident, because `may_write` saw `O_RDONLY`.
        let opw = rustix::fs::open(FILE, path_flags | OFlags::WRONLY, Mode::empty());
        if let Ok(f) = &opw {
            let opw_raw = std::os::fd::AsRawFd::as_raw_fd(f);
            let before = std::fs::metadata(FILE).map(|m| m.len()).unwrap_or(0);
            // SAFETY: `opw_raw` is a live descriptor this test owns.
            let rc = unsafe { libc::syscall(libc::SYS_fallocate, opw_raw, 0i32, 0i64, 4096i64) };
            let after = std::fs::metadata(FILE).map(|m| m.len()).unwrap_or(0);
            r.check(
                &format!(
                    "fallocate(O_PATH|O_WRONLY fd) is EBADF, got rc={rc} errno={}",
                    std::io::Error::last_os_error()
                ),
                ebadf(rc),
            );
            r.check(
                &format!("and it changed nothing: size {before} -> {after}"),
                before == after,
            );
        }
        // The pair that makes the refusals above about `O_PATH` rather than
        // "these calls are broken here": the same doors are **served** on an
        // ordinary descriptor of the same file, and succeed together with the
        // `fstat` neighbour already claimed. `copy_file_range` and `splice` are
        // the two that moved bytes through the wrong descriptor, so they are the
        // two worth proving still work on the right one.
        if let (Ok(rd), Ok(dst)) = (
            rustix::fs::open(FILE, OFlags::RDONLY, Mode::empty()),
            rustix::fs::open(
                "/tmp/t-open-path/copy2.out",
                OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC,
                Mode::from(0o644),
            ),
        ) {
            let rd_raw = std::os::fd::AsRawFd::as_raw_fd(&rd);
            let d_raw = std::os::fd::AsRawFd::as_raw_fd(&dst);
            // SAFETY: both are live descriptors this test owns.
            let copied = unsafe {
                libc::syscall(
                    libc::SYS_copy_file_range,
                    rd_raw,
                    0usize,
                    d_raw,
                    0usize,
                    5usize,
                    0u32,
                )
            };
            r.check(
                &format!(
                    "copy_file_range over two ordinary descriptors still copies, got {copied}"
                ),
                copied == 5,
            );
        }
        // And the access-mode half, which is the same predicate asked a
        // different way: `readahead` on a **write-only** descriptor is `EBADF`
        // on Linux (`build/scratch/opathmmap/mode2.c`), because the question is
        // "may this descriptor be read", not "is this word `O_PATH`".
        if let Ok(wr) = rustix::fs::open(FILE, OFlags::WRONLY, Mode::empty()) {
            let wr_raw = std::os::fd::AsRawFd::as_raw_fd(&wr);
            // SAFETY: `wr_raw` is a live descriptor this test owns.
            let rc = unsafe { libc::syscall(libc::SYS_readahead, wr_raw, 0usize, 4096usize) };
            r.check(
                &format!(
                    "readahead(write-only fd) is EBADF, got rc={rc} errno={}",
                    std::io::Error::last_os_error()
                ),
                ebadf(rc),
            );
            r.check(
                "and readahead on a read-only descriptor of the same file is still served",
                rustix::fs::open(FILE, OFlags::RDONLY, Mode::empty()).is_ok_and(|rd2| {
                    let rd2_raw = std::os::fd::AsRawFd::as_raw_fd(&rd2);
                    // SAFETY: `rd2_raw` is a live descriptor this test owns.
                    unsafe { libc::syscall(libc::SYS_readahead, rd2_raw, 0usize, 4096usize) == 0 }
                }),
            );
        }
    }

    // === the open asks no permission on the file ============================
    //
    // The half of the flag that makes every refusal above load-bearing rather
    // than tidy: `open(path, O_PATH)` **asks nothing of the file's own mode**,
    // which is why the descriptor must then be refused by every contents door.
    // Measured on Linux 7.3.4 (`build/scratch/opathmmap/{opening2,shadow}.c`):
    // as uid 1000, a file the caller owns opened `0000` refuses `O_RDONLY` with
    // `EACCES` and **serves `O_PATH`**; `/etc/shadow` does the same (`0600`,
    // owner `nobody`). The *parent directory's* search bit is still asked and
    // still refused. Linux reaches it in `build_open_flags`, which sets
    // `acc_mode = 0` for `O_PATH` and checks `MAY_OPEN` alone.
    //
    // **This claim needs a second uid, and that is why it is a child.** The
    // sweep's payload runs as root, where `CAP_DAC_OVERRIDE` answers every mode
    // question "yes" - so a claim made in this process would pass on a kernel
    // that asked `R_OK` for an `O_PATH` open, which is exactly the divergence
    // this pins. The child drops to `nobody` (the pattern `t-proc-access` and
    // `t-affinity-rules` already use, and `nobody` exists in both images)
    // **before** it opens anything, and reports over a pipe. On the host the
    // drop fails without privilege and the child stays the caller's own user -
    // which is still a uid that does not own a root-owned fixture, so the
    // allowed/refused pair below reads the same rule from the other side.
    let secret = format!("{DIR}/moon");
    let _ = std::fs::write(&secret, b"not for you");
    let _ = std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000));
    // A fixture the child can still reach: its directory is 0755, so only the
    // file's own bits are in question.
    let readable_by_all = format!("{DIR}/sun");
    let _ = std::fs::write(&readable_by_all, b"for anyone");
    let _ = std::fs::set_permissions(&readable_by_all, std::fs::Permissions::from_mode(0o444));
    {
        let mut fds = [0i32; 2];
        // SAFETY: a two-element array this test owns.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
        r.check("the report pipe is made", piped);
        if piped {
            // SAFETY: `fork` in a single-threaded std program; the child only
            // writes to `fds[1]` and exits.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // The child never returns through `main`: `_exit` keeps it from
                // unwinding the parent's `Report`.
                unsafe {
                    libc::close(fds[0]);
                    libc::setgroups(0, core::ptr::null());
                    libc::setgid(65534);
                    libc::setuid(65534);
                }
                let mut report = String::new();
                for (label, path) in [("secret", &secret), ("world", &readable_by_all)] {
                    let rd = rustix::fs::open(path, OFlags::RDONLY, Mode::empty());
                    let op = rustix::fs::open(path, OFlags::PATH, Mode::empty());
                    report.push_str(&format!(
                        "{label} rd={} path={}\n",
                        answer_code(&rd),
                        answer_code(&op),
                    ));
                }
                unsafe {
                    let bytes = report.as_bytes();
                    libc::write(fds[1], bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
                    libc::close(fds[1]);
                    libc::_exit(0);
                }
            }
            unsafe { libc::close(fds[1]) };
            let mut buf = Vec::new();
            loop {
                let mut chunk = [0u8; 256];
                // SAFETY: `fds[0]` is open until the child's end closes it.
                let n = unsafe {
                    libc::read(
                        fds[0],
                        chunk.as_mut_ptr().cast::<libc::c_void>(),
                        chunk.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n as usize]);
            }
            unsafe { libc::close(fds[0]) };
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            let text = String::from_utf8_lossy(&buf).to_string();
            // Parsed rather than substring-matched: the report's line shape is
            // `<name> rd=<answer> path=<answer>`, and "secret path=ok:" is not
            // a substring of "secret rd=errno:13 path=ok:3" - a first draft
            // matched the two independently and failed on Linux for saying so.
            let field = |line_start: &str, key: &str| -> Option<String> {
                text.lines()
                    .find(|l| l.starts_with(line_start))
                    .and_then(|l| {
                        l.split_whitespace()
                            .find_map(|w| w.strip_prefix(key).map(str::to_string))
                    })
            };
            let secret_rd = field("secret ", "rd=");
            let secret_path = field("secret ", "path=");
            let world_rd = field("world ", "rd=");
            let world_path = field("world ", "path=");
            // A `0000` file: `O_RDONLY` is `EACCES` and `O_PATH` is served.
            r.check(
                &format!("a 0000 file refuses O_RDONLY for a stranger ({text:?})"),
                secret_rd.as_deref() == Some("errno:13"),
            );
            r.check(
                &format!("and serves O_PATH on the same file ({text:?})"),
                secret_path.as_deref().is_some_and(|a| a.starts_with("ok:")),
            );
            // The neighbour that keeps the pair from being "everything is
            // refused": a world-readable file is served both ways.
            r.check(
                &format!("a 0444 file is served as O_PATH for the same caller ({text:?})"),
                world_path.as_deref().is_some_and(|a| a.starts_with("ok:")),
            );
            r.check(
                &format!("and as O_RDONLY too, so the caller is not simply refused ({text:?})"),
                world_rd.as_deref().is_some_and(|a| a.starts_with("ok:")),
            );
        }
    }
    // The owner may always open it, so the fixture really is readable-by-mode
    // and the child's refusal above is about the id rather than the file.
    r.check(
        "the 0000 file is served to its owner as O_PATH, so the mode is the only question",
        rustix::fs::open(&secret, OFlags::PATH, Mode::empty()).is_ok(),
    );
    let _ = std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644));

    let _ = std::fs::remove_dir_all(DIR);
    r.finish()
}

/// The answer code of an open, as the child reports it: `ok:<fd>` or
/// `errno:<n>`, so a claim can name the errno rather than only "it failed".
fn answer_code(answer: &Result<std::os::fd::OwnedFd, rustix::io::Errno>) -> String {
    match answer {
        Ok(fd) => format!("ok:{}", std::os::fd::AsRawFd::as_raw_fd(fd)),
        Err(e) => format!("errno:{}", e.raw_os_error()),
    }
}
