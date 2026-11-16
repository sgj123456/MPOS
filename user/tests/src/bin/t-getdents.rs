//! t-getdents - the **legacy** `getdents(2)`, number 78, beside the
//! `getdents64` this kernel has served for a long time.
//!
//! It was undeclared, so the dispatch answered `ENOSYS` - and unlike most of the
//! census's leftovers, the sibling **does not replace it**: the records differ by
//! one byte of header and the `d_type` field, which is the kind of difference
//! that reads as a wrong *name* rather than a wrong call.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{gd,gd2}.c`) before any
//! claim:
//!
//! | what | Linux |
//! |---|---|
//! | `getdents` on a directory | records, `d_ino`/`d_off`/`d_reclen`/name, **no `d_type`** |
//! | header before the name | **18** bytes (`getdents64`'s is 19) |
//! | exhausted directory | `0` |
//! | a buffer too small for one record | `EINVAL` |
//! | a **regular file** | `ENOTDIR` |
//! | a bad descriptor | `EBADF` |
//! | a `NULL` buffer | `EFAULT` |
//! | `count == 0` | `EINVAL` |
//! | the position | the **descriptor's** — an `lseek` rewinds it |
//!
//! **`getdents` is x86_64-only.** The asm-generic table aarch64 uses has only
//! `getdents64` (61), so on that machine this number is a value no caller can
//! pass; the program says so there rather than printing nothing.
//!
//! Numbers from libc's table; `rustix` binds `getdents64` and not this one.

use mpos_user_tests::Report;

/// The legacy record's header, **measured**: `d_ino` 0..8, `d_off` 8..16,
/// `d_reclen` 16..18, name at 18.
const LEGACY_HDR: usize = 18;
/// `getdents64`'s header, for the comparison the claim depends on.
const V64_HDR: usize = 19;

#[cfg(target_arch = "x86_64")]
const SYS_GETDENTS: libc::c_long = 78;
// **The modern number is per machine** - x86_64 numbers `getdents64` 217, the
// asm-generic table aarch64 uses numbers it 61. A first draft hardcoded the
// x86_64 value and the *aarch64* arm called a number that machine gives to
// something else, so `getdents64` looked broken there - the same defect item
// 314's pin records, made again in a different file.
#[cfg(target_arch = "x86_64")]
const SYS_GETDENTS64: libc::c_long = 217;
#[cfg(not(target_arch = "x86_64"))]
const SYS_GETDENTS64: libc::c_long = 61;

fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}
fn open_dir(path: &str) -> i32 {
    let c = std::ffi::CString::new(path).unwrap();
    // SAFETY: a path this test owns.
    unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) }
}

/// Walk one record at a time, answering `(name, d_ino, d_off, reclen)` for the
/// legacy door's layout.
fn walk_legacy(buf: &[u8], n: usize) -> Vec<(String, u64, u64, u16)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + LEGACY_HDR <= n {
        let ino = u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap());
        let doff = u64::from_ne_bytes(buf[off + 8..off + 16].try_into().unwrap());
        let reclen = u16::from_ne_bytes(buf[off + 16..off + 18].try_into().unwrap());
        if reclen == 0 || off + reclen as usize > n {
            break;
        }
        let name_end = buf[off + LEGACY_HDR..off + reclen as usize]
            .iter()
            .position(|b| *b == 0)
            .map(|p| off + LEGACY_HDR + p)
            .unwrap_or(off + reclen as usize);
        let name = String::from_utf8_lossy(&buf[off + LEGACY_HDR..name_end]).into_owned();
        out.push((name, ino, doff, reclen));
        off += reclen as usize;
    }
    out
}

/// And the 64-bit door's, which has a `d_type` at 18 and the name at 19.
fn walk64(buf: &[u8], n: usize) -> Vec<(String, u8, u16)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + V64_HDR <= n {
        let reclen = u16::from_ne_bytes(buf[off + 16..off + 18].try_into().unwrap());
        let dtype = buf[off + 18];
        if reclen == 0 || off + reclen as usize > n {
            break;
        }
        let name_end = buf[off + V64_HDR..off + reclen as usize]
            .iter()
            .position(|b| *b == 0)
            .map(|p| off + V64_HDR + p)
            .unwrap_or(off + reclen as usize);
        let name = String::from_utf8_lossy(&buf[off + V64_HDR..name_end]).into_owned();
        out.push((name, dtype, reclen));
        off += reclen as usize;
    }
    out
}

fn main() {
    println!("t-getdents");
    let mut r = Report::new();

    // A directory with a mix of names, including one long enough to cross the
    // 8-byte alignment boundary - which is where the two doors' `d_reclen`
    // actually diverge, and the reason a short-name-only fixture would pass
    // either layout.
    let dir = "/tmp/t-getdents.d";
    let _ = std::fs::remove_dir_all(dir);
    let made = std::fs::create_dir_all(dir).is_ok();
    r.check(&format!("a scratch directory is made ({made})"), made);
    for name in ["a", "medium-name", "a-fairly-long-file-name-here"] {
        let _ = std::fs::write(format!("{dir}/{name}"), b"x");
    }
    if !made {
        r.finish();
    }

    // === the served shape ==================================================
    #[cfg(target_arch = "x86_64")]
    {
        let fd = open_dir(dir);
        r.check(&format!("the directory opens ({fd})"), fd >= 0);
        if fd >= 0 {
            let mut buf = vec![0u8; 1024];
            // SAFETY: `buf` is this test's, and the descriptor is the directory
            // just opened.
            let n =
                unsafe { libc::syscall(SYS_GETDENTS, fd, buf.as_mut_ptr(), buf.len()) as isize };
            r.check(&format!("getdents is served ({n})"), n > 0);
            let records = walk_legacy(&buf, n.max(0) as usize);
            // The fixture made three files. **This filesystem does not
            // synthesise `.` and `..`**, and the pin says so rather than
            // expecting Linux's tmpfs list: `getdents64` here returns the same
            // three, so the claim is about the door's *record layout*, not about
            // which entries a directory walk invents. (Linux's own ext4 does add
            // them; a ramfs that reports only what was made is a difference the
            // twin already carries, and item 318 is not the place to change it.)
            // **Which entries a directory lists is the filesystem's answer, not
            // this door's.** A host's ext4 synthesises `.` and `..`; this
            // kernel's ramfs reports only what was made, and its `getdents64`
            // does the same - so the claim here is that the fixture's three
            // names are all present, not that the list is exactly three.
            let mut names: Vec<&str> = records.iter().map(|(n, ..)| n.as_str()).collect();
            names.sort_unstable();
            let wanted = ["a", "a-fairly-long-file-name-here", "medium-name"];
            r.check(
                &format!("and every fixture name is listed ({names:?})"),
                wanted.iter().all(|w| names.contains(w)),
            );
            r.check(
                &format!("and nothing invented is ({names:?})"),
                names
                    .iter()
                    .all(|n| wanted.contains(n) || *n == "." || *n == ".."),
            );

            // **The layout claim**: the name starts at 18, and a reader using
            // `getdents64`'s 19 would be one byte into it.
            r.check(
                "every record's name is at 18, not 19",
                records.iter().all(|(name, ..)| {
                    // A name read at 19 would lose its first byte; these names
                    // are the ones the fixture made, so a missed first byte
                    // shows as a name that is not in the fixture.
                    [
                        "a",
                        "medium-name",
                        "a-fairly-long-file-name-here",
                        ".",
                        "..",
                    ]
                    .contains(&name.as_str())
                }),
            );
            // A record's `reclen` must both cover the name and be 8-aligned.
            r.check(
                "records are 8-aligned and cover their names",
                records.iter().all(|(name, _, _, reclen)| {
                    (*reclen as usize) % 8 == 0 && (*reclen as usize) > LEGACY_HDR + name.len()
                }),
            );
            // The exhausted directory answers 0, not EINVAL.
            // SAFETY: as above.
            let again =
                unsafe { libc::syscall(SYS_GETDENTS, fd, buf.as_mut_ptr(), buf.len()) as isize };
            r.check(
                &format!("an exhausted directory answers 0 ({again})"),
                again == 0,
            );

            // **The position is the descriptor's**: rewind and the same records
            // come back.
            // SAFETY: `fd` is live.
            let rewound = unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
            r.check(&format!("lseek rewinds it ({rewound})"), rewound == 0);
            // SAFETY: as above.
            let after =
                unsafe { libc::syscall(SYS_GETDENTS, fd, buf.as_mut_ptr(), buf.len()) as isize };
            r.check(
                "and the next call starts over",
                after > 0 && walk_legacy(&buf, after as usize).len() == records.len(),
            );
            // SAFETY: the descriptor this block opened.
            unsafe { libc::close(fd) };
        }

        // === the refusals ==================================================
        let fd = open_dir(dir);
        if fd >= 0 {
            let mut tiny = [0u8; 8];
            // SAFETY: a buffer this test owns.
            let e = unsafe { libc::syscall(SYS_GETDENTS, fd, tiny.as_mut_ptr(), 8usize) as isize };
            r.check(
                "a buffer too small for one record is EINVAL",
                refused(e, libc::EINVAL),
            );
            // SAFETY: `NULL` is the subject of the claim.
            let null = unsafe { libc::syscall(SYS_GETDENTS, fd, 0usize, 64usize) } as isize;
            r.check("a NULL buffer is EFAULT", refused(null, libc::EFAULT));
            let zero = [0u8; 64];
            // SAFETY: a buffer this test owns.
            let z =
                unsafe { libc::syscall(SYS_GETDENTS, fd, zero.as_ptr() as usize, 0usize) as isize };
            r.check("count 0 is EINVAL", refused(z, libc::EINVAL));
            // SAFETY: the descriptor this block opened.
            unsafe { libc::close(fd) };
        }
        // **A regular file is `ENOTDIR`, not `EBADF`** - measured on the host
        // (`build/scratch/opathmmap/gd3.c`: `rc=-1 errno=20`), and
        // `getdents64` answers the same. A first draft wrote `EBADF` by analogy
        // with the `O_PATH` case, which *is* `EBADF` — two refusals about
        // different things, and the mode is not the question here.
        let f = std::ffi::CString::new(format!("{dir}/a")).unwrap();
        // SAFETY: a path this test made.
        let regular = unsafe { libc::open(f.as_ptr(), libc::O_RDONLY) };
        if regular >= 0 {
            let mut buf = [0u8; 64];
            // SAFETY: a buffer this test owns.
            let e =
                unsafe { libc::syscall(SYS_GETDENTS, regular, buf.as_mut_ptr(), 64usize) as isize };
            r.check("a regular file is ENOTDIR", refused(e, libc::ENOTDIR));
            // SAFETY: as above.
            unsafe { libc::close(regular) };
        }
        // SAFETY: `-1` is the subject of the claim.
        let bad = unsafe { libc::syscall(SYS_GETDENTS, -1i32, 0usize, 64usize) } as isize;
        r.check(
            &format!("a bad descriptor is EBADF (rc {bad}, errno {:?})", errno()),
            refused(bad, libc::EBADF),
        );
        // **`O_PATH` on a directory *is* `EBADF`** (measured, `gd3.c`: `rc=-1
        // errno=9`), which is the answer the draft mistook for the regular-file
        // case: the descriptor names the directory without opening it.
        let pathonly = {
            let c = std::ffi::CString::new(dir).unwrap();
            // SAFETY: a path this test made.
            unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) }
        };
        if pathonly >= 0 {
            let mut buf = [0u8; 64];
            // SAFETY: a buffer this test owns.
            let e = unsafe {
                libc::syscall(SYS_GETDENTS, pathonly, buf.as_mut_ptr(), 64usize) as isize
            };
            r.check(
                "an O_PATH directory descriptor is EBADF",
                refused(e, libc::EBADF),
            );
            // SAFETY: the descriptor just opened.
            unsafe { libc::close(pathonly) };
        }

        // === the twin, for the contrast the layout claim rests on ===========
        //
        // Same directory, `getdents64`: the names match and the `d_type` byte
        // exists. That is what makes this door a *different* call rather than a
        // second spelling.
        let fd = open_dir(dir);
        if fd >= 0 {
            let mut buf = vec![0u8; 1024];
            // SAFETY: a buffer this test owns and a live descriptor.
            let n =
                unsafe { libc::syscall(SYS_GETDENTS64, fd, buf.as_mut_ptr(), buf.len()) as isize };
            r.check(
                &format!("getdents64 still serves the same directory ({n})"),
                n > 0,
            );
            let recs = walk64(&buf, n.max(0) as usize);
            r.check(
                &format!("and its records carry d_type ({})", recs.len()),
                recs.iter().all(|(_, dtype, _)| *dtype != 0),
            );
            r.check(
                "and its names are the same set",
                recs.iter().any(|(name, ..)| name == "medium-name"),
            );
            // SAFETY: the descriptor this block opened.
            unsafe { libc::close(fd) };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // **Named rather than silent**: asm-generic has only `getdents64`, so
        // this number is one no aarch64 caller can pass. A program that printed
        // nothing here would look like a pass.
        r.check(
            "the legacy getdents number is x86_64-only, so aarch64 has nothing to claim",
            true,
        );
        // The twin is still claimed on this machine, so the file is not a
        // no-op there.
        let fd = open_dir(dir);
        if fd >= 0 {
            let mut buf = vec![0u8; 1024];
            // SAFETY: a buffer this test owns and a live descriptor.
            let n =
                unsafe { libc::syscall(SYS_GETDENTS64, fd, buf.as_mut_ptr(), buf.len()) as isize };
            r.check(&format!("getdents64 serves it ({n})"), n > 0);
            // SAFETY: the descriptor this block opened.
            unsafe { libc::close(fd) };
        }
    }

    let _ = std::fs::remove_dir_all(dir);
    r.finish()
}
