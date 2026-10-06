//! t-tmpfile-link - a file with no name gets one, the way `apk` gives one to
//! the repository index it downloads.
//!
//! `apk update` writes `APKINDEX.<hash>.tar.gz` through this exact sequence
//! (`apk-tools`' `apk_ostream_to_file_safe`, `src/io.c`):
//!
//! 1. `openat(cache_fd, ".", O_RDWR | O_TMPFILE | O_CLOEXEC, 0644)` - an inode
//!    with no name in it, so the index is never seen half-written;
//! 2. write the body;
//! 3. `linkat(AT_FDCWD, "/proc/self/fd/N", cache_fd, "APKINDEX...tmp.PID",
//!    AT_SYMLINK_FOLLOW)` - the *only* way to give that inode a name, and the
//!    reason the flag exists: `/proc/self/fd/N` is a descriptor's second name,
//!    so the source is the file the descriptor holds rather than a path on
//!    procfs;
//! 4. `renameat(cache_fd, "...tmp.PID", cache_fd, "APKINDEX.<hash>.tar.gz")`
//!    and close.
//!
//! **This is the pin for a real failure.** Measured on the second machine's
//! real Alpine userland (2026-10-06): every `apk update` answered
//!
//! ```text
//! WARNING: updating and opening https://.../APKINDEX.tar.gz: Cross-device link
//! 2 unavailable, 0 stale; 27 distinct packages available
//! ```
//!
//! which is `apk_cache_download` returning the stream's close error, `EXDEV`
//! from step 3 - "the two ends live on different filesystems" - even though the
//! inode was born in the very directory it is being linked into. `apk` then
//! treats the index as unavailable and re-fetches it on every run.
//!
//! The check has to run where `O_TMPFILE` is *served*: only the ext4 root does
//! that (ramfs and tmpfs answer `EOPNOTSUPP`, which is what `t-open-flags`
//! pins), so on the x86_64 gate - whose root is the initramfs's ramfs - the test
//! takes `apk`'s own named-temporary fallback and says so, and on the aarch64
//! sweep, whose root is the ext4 disk, it drives steps 1-4 for real. The
//! counterfactual is on the second machine: with the bug in place this program
//! fails there and passes on x86_64, which is how the report arrived.
//!
//! Two facts about where it runs, both measured rather than assumed:
//!
//! * The scratch tree is under `/`, not `/tmp`: the point is the ext4 root, and
//!   both sweeps mount a tmpfs over `/tmp`.
//! * Step 3 needs *a* procfs. The x86_64 harness's `/init` mounts `/proc` before
//!   it runs anything, and a real boot has one already; the aarch64 sweep's
//!   runner mounts only `/tmp`, so this test mounts a procfs of its own when the
//!   payload has none and says which one it used. The mount is the harness's
//!   gap, not the kernel's: `/proc/self/fd/N` is how the ABI names a descriptor
//!   whatever the payload set up, and a test that skipped the step where
//!   `/proc` was missing would have pinned `ENOENT` instead of the bug.

use std::ffi::CString;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// The scratch directory this test owns, on the root filesystem.
const DIR: &str = "/tmpfile-link-probe";

fn cstr(path: &str) -> CString {
    CString::new(path.as_bytes()).expect("the path has no interior NUL")
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn openat_raw(
    dirfd: libc::c_int,
    path: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    // SAFETY: `path` is a NUL-terminated string this call owns, the flags are
    // `libc`'s own, and `errno` is read immediately by the caller.
    let fd = unsafe { libc::openat(dirfd, cstr(path).as_ptr(), flags, mode) };
    if fd < 0 { -errno() } else { fd }
}

/// The bytes the body carries, so a copy that lost them is not read as a
/// successful link.
const BODY: &[u8] = b"the index apk would have written\n";

/// Where a descriptor's second name can be read: the machine's own `/proc` when
/// the payload has one, otherwise a procfs this test mounts and owns.
///
/// `None` means the mount was refused, which is a setup failure rather than a
/// kernel failure and is reported as its own line.
fn procfs_root(r: &mut Report) -> Option<&'static str> {
    if std::path::Path::new("/proc/self").exists() {
        println!("     the payload's own /proc");
        return Some("/proc");
    }
    // The mountpoint must exist first: `mount` answers ENOTDIR otherwise.
    let root = "/tmpfile-link-probe.proc";
    let _ = std::fs::create_dir(root);
    let (source, fstype) = (cstr("proc"), cstr("proc"));
    let answer = unsafe {
        libc::mount(
            source.as_ptr(),
            cstr(root).as_ptr(),
            fstype.as_ptr(),
            0,
            core::ptr::null(),
        )
    };
    if answer != 0 {
        r.check(
            &format!(
                "a procfs can be mounted to name the descriptor in (errno {})",
                errno()
            ),
            false,
        );
        return None;
    }
    println!("     a procfs mounted by this test ({root}): the payload has none");
    Some(root)
}

fn main() {
    println!("t-tmpfile-link");
    let mut r = Report { failed: false };

    let _ = std::fs::remove_dir_all(DIR);
    if let Err(err) = std::fs::create_dir(DIR) {
        r.check(
            &format!("the scratch directory can be made (got {err})"),
            false,
        );
        std::process::exit(1);
    }
    // The destination is opened as a *descriptor* and the name is given through
    // it, exactly as `apk_repo_fd` hands `apk_ostream_to_file_safe` the cache
    // directory it opened once (`db->cache_fd`).
    let dirfd = openat_raw(
        libc::AT_FDCWD,
        DIR,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
    );
    if dirfd < 0 {
        r.check(
            &format!(
                "the cache directory opens as a descriptor (errno {})",
                -dirfd
            ),
            false,
        );
        std::process::exit(1);
    }

    // Step 1: the unnamed inode, born in the directory the name will live in.
    let fd = openat_raw(
        dirfd,
        ".",
        libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC,
        0o644,
    );
    if fd < 0 {
        let e = -fd;
        // `EOPNOTSUPP`/`EISDIR` is the filesystem saying it cannot keep an inode
        // out of the tree - the answer `apk` listens for, and the one the ramfs
        // root gives. Take `apk`'s own fallback and pin that instead.
        r.check(
            &format!(
                "open(dir, O_TMPFILE) is a refusal a caller can fall back from, not a wrong shape \
                 (errno {e})"
            ),
            e == libc::EOPNOTSUPP || e == libc::EISDIR || e == libc::EINVAL,
        );
        fallback(&mut r, dirfd);
        cleanup(dirfd);
        report(r);
        return;
    }
    r.check("open(dir, O_TMPFILE) makes an inode with no name", true);

    // Step 2: the body.
    let written = unsafe { libc::write(fd, BODY.as_ptr().cast::<libc::c_void>(), BODY.len()) };
    r.check(
        &format!(
            "the unnamed inode takes the body (wrote {written} of {})",
            BODY.len()
        ),
        written == BODY.len() as isize,
    );

    // Step 3: give it a name, through the descriptor's second name. This is the
    // call the report says answers `EXDEV`.
    let Some(procfs) = procfs_root(&mut r) else {
        unsafe { libc::close(fd) };
        cleanup(dirfd);
        report(r);
        return;
    };
    let tmpname = format!("APKINDEX.deadbeef.tar.gz.tmp.{}", std::process::id());
    let source = format!("{procfs}/self/fd/{fd}");
    let rc = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            cstr(&source).as_ptr(),
            dirfd,
            cstr(&tmpname).as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    let link_errno = if rc < 0 { errno() } else { 0 };
    r.check(
        &format!(
            "linkat(\"{source}\", dirfd, \"{tmpname}\", AT_SYMLINK_FOLLOW) gives the inode a name \
             (got errno {link_errno})"
        ),
        rc == 0,
    );
    if rc < 0 {
        // The whole point of the pin: name the failure the report names.
        r.check(
            &format!(
                "the source is the file, not a name on procfs (EXDEV {} is that reading)",
                libc::EXDEV
            ),
            link_errno != libc::EXDEV,
        );
    }
    unsafe { libc::close(fd) };

    // Step 4: the temporary name becomes the real one.
    if rc == 0 {
        let final_name = "APKINDEX.deadbeef.tar.gz";
        let moved = unsafe {
            libc::renameat(
                dirfd,
                cstr(&tmpname).as_ptr(),
                dirfd,
                cstr(final_name).as_ptr(),
            )
        };
        r.check(
            &format!(
                "renameat moves the temporary name into place (errno {})",
                if moved < 0 { errno() } else { 0 }
            ),
            moved == 0,
        );
        let path = format!("{DIR}/{final_name}");
        match std::fs::read(&path) {
            Ok(bytes) => r.check(
                &format!(
                    "the named file reads back the body ({} bytes of {})",
                    bytes.len(),
                    BODY.len()
                ),
                bytes == BODY,
            ),
            Err(err) => r.check(&format!("the named file reads back (got {err})"), false),
        }
    }

    cleanup(dirfd);
    report(r);
}

/// `apk`'s own fallback when the filesystem has no `O_TMPFILE`: a *named*
/// temporary in the destination directory, renamed into place. It is the
/// sequence the x86_64 gate's ramfs root drives, and it must work here.
fn fallback(r: &mut Report, dirfd: libc::c_int) {
    let tmpname = format!("APKINDEX.deadbeef.tar.gz.tmp.{}", std::process::id());
    let fd = openat_raw(
        dirfd,
        &tmpname,
        libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC | libc::O_CLOEXEC | libc::O_EXCL,
        0o644,
    );
    r.check(
        &format!(
            "the fallback's named temporary can be made (errno {})",
            if fd < 0 { -fd } else { 0 }
        ),
        fd >= 0,
    );
    if fd < 0 {
        return;
    }
    let written = unsafe { libc::write(fd, BODY.as_ptr().cast::<libc::c_void>(), BODY.len()) };
    r.check(
        &format!(
            "the named temporary takes the body (wrote {written} of {})",
            BODY.len()
        ),
        written == BODY.len() as isize,
    );
    unsafe { libc::close(fd) };
    let final_name = "APKINDEX.deadbeef.tar.gz";
    let moved = unsafe {
        libc::renameat(
            dirfd,
            cstr(&tmpname).as_ptr(),
            dirfd,
            cstr(final_name).as_ptr(),
        )
    };
    r.check(
        &format!(
            "renameat moves the temporary name into place (errno {})",
            if moved < 0 { errno() } else { 0 }
        ),
        moved == 0,
    );
}

/// Remove the scratch tree, including whatever a failed run left behind.
fn cleanup(dirfd: libc::c_int) {
    for name in [
        "APKINDEX.deadbeef.tar.gz",
        &format!("APKINDEX.deadbeef.tar.gz.tmp.{}", std::process::id()),
    ] {
        unsafe { libc::unlinkat(dirfd, cstr(name).as_ptr(), 0) };
    }
    unsafe { libc::close(dirfd) };
    let _ = std::fs::remove_dir(DIR);
}

fn report(r: Report) {
    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
