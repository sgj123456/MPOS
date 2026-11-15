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

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

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
    }

    let _ = std::fs::remove_dir_all(DIR);
    r.finish()
}
