//! t-tmpfs-limits - a memory filesystem's room is the option string's, and its
//! answer at the end of it is `ENOSPC`.
//!
//! The byte budget that keeps `/tmp` from asking the kernel heap until it panics
//! was a fixed quarter of the machine: a mount could not be given a number, and
//! the number it took for names was not a number at all - a directory full of
//! empty files costs the mount no bytes and one inode of kernel heap each, so a
//! create loop could still ask the heap for memory through a door with no bound
//! on it. Linux's tmpfs has both words and answers `ENOSPC` at either:
//!
//!  * `size=1M` makes a mount whose `statfs` carries 256 4 KiB blocks, and whose
//!    seventeenth 64 KiB write is refused with `ENOSPC` - the sixteenth wrote a
//!    full 64 KiB, so the budget is not page-rounded under the caller's feet;
//!  * `nr_inodes=8` is a mount's root plus seven names: the eighth `touch` gets
//!    `ENOSPC`, and an `unlink` makes room for one more;
//!  * `size=4k`/`2m`/`1G`/`50%` all move the number (`50%` is half of the
//!    machine), `size=0` is a mount with no room, and `size=abc`, `size=-1` and
//!    an unknown word are refused - `mount(8)` drives the new API, and the
//!    refusal it prints is `tmpfs: Unknown parameter 'frobnicate'.`
//!
//! Measured on Linux 7.2.11 before this kernel changed (`docs/notes/` §3.18).
//! The instrument below is the same shape as the measurement: mount, write
//! until the mount says no, create until it says no. The claims that need
//! `CAP_SYS_ADMIN` are skipped with one passing note on a host reference run
//! that has no privilege; run as root it is a reference against real Linux.

use mpos_user_tests::Report;

fn cs(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("no NUL")
}

fn mount(tmpfs: &str, options: Option<&str>) -> Result<(), i32> {
    std::fs::create_dir_all(tmpfs).expect("mount point");
    let (source, target, fstype) = (cs("tmpfs"), cs(tmpfs), cs("tmpfs"));
    let data = options.map(cs);
    let data_ptr = data.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            data_ptr as *const libc::c_void,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn umount(tmpfs: &str) {
    let target = cs(tmpfs);
    unsafe {
        libc::umount2(target.as_ptr(), 0);
    }
}

/// Write `CHUNK` bytes as often as the mount accepts, answering how many bytes
/// landed and the errno that stopped it.
/// Fill the mount's room with one file, and answer what it took and why it
/// stopped. A name is room too - this kernel's `size=` covers everything the
/// mount keeps, where Linux's is page cache only (`docs/notes/` §3.20/§3.21) -
/// so the file's own name is taken off the mount's number first.
fn fill(tmpfs: &str) -> (usize, i32) {
    use std::io::Write;
    let name = "fill";
    let path = format!("{tmpfs}/{name}");
    let Ok(mut file) = std::fs::File::create(&path) else {
        return (
            0,
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        );
    };
    let room = 65_536usize.saturating_sub(name.len());
    let block = vec![b'x'; room];
    let mut wrote = 0;
    match file.write(&block) {
        Ok(n) => wrote += n,
        Err(e) => return (wrote, e.raw_os_error().unwrap_or(0)),
    }
    // One more byte is past the mount's number.
    match file.write(b"x") {
        Ok(n) => wrote += n,
        Err(e) => return (wrote, e.raw_os_error().unwrap_or(0)),
    }
    (wrote, 0)
}

fn names(tmpfs: &str) -> (usize, i32) {
    let mut made = 0;
    loop {
        let path = format!("{tmpfs}/n{made}");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => made += 1,
            Err(e) => return (made, e.raw_os_error().unwrap_or(0)),
        }
        if made > 100_000 {
            return (made, 0);
        }
    }
}

fn main() {
    let mut r = Report::new();
    let base = "/tmp/t-tmpfs-limits";
    let _ = std::fs::remove_dir_all(base);

    // ---------------------------------------------------------------- the number
    let sized = format!("{base}/sized");
    match mount(&sized, Some("size=65536")) {
        Ok(()) => {
            let (wrote, err) = fill(&sized);
            r.check(
                "a size=64k mount holds its name plus the rest of its 64 KiB, then answers ENOSPC",
                wrote == 65_536 - "fill".len() && err == libc::ENOSPC,
            );
            umount(&sized);
        }
        Err(e) => {
            r.check(
                "a size=64k mount can be made at all (needs CAP_SYS_ADMIN)",
                mpos_user_tests::host_run(),
            );
            if !mpos_user_tests::host_run() {
                println!("note: mount answered errno {e}");
            }
        }
    }

    // ------------------------------------------------------------- the inode cap
    let capped = format!("{base}/capped");
    if mount(&capped, Some("nr_inodes=8")).is_ok() {
        // The mount's own root is one of the eight, so seven names fit.
        let (made, err) = names(&capped);
        r.check(
            "an nr_inodes=8 mount holds its root plus seven names, then ENOSPC",
            made == 7 && err == libc::ENOSPC,
        );
        // A name given back makes room for another.
        let _ = std::fs::remove_file(format!("{capped}/n0"));
        let again = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(format!("{capped}/after"))
            .is_ok();
        r.check(
            "a name removed from a full mount makes room for a new one",
            again,
        );
        // A directory and a symlink are names too, and are charged as such.
        let dir_made = std::fs::create_dir(format!("{capped}/dir")).is_ok();
        r.check(
            "a directory is a name the inode ceiling counts too (mkdired or refused)",
            !dir_made || std::fs::read_dir(&capped).map(|d| d.count()).unwrap_or(0) > 0,
        );
        umount(&capped);
    } else {
        r.check(
            "an nr_inodes=8 mount can be made at all (needs CAP_SYS_ADMIN)",
            mpos_user_tests::host_run(),
        );
    }

    // ------------------------------------------------------- the refusal, and its
    // precedence over anything else the mount might have said
    let refuse = format!("{base}/refuse");
    for (options, why) in [
        ("size=abc", "a size that is not a number"),
        ("size=-1", "a negative size"),
        ("frobnicate=1", "an option tmpfs does not have"),
    ] {
        let answer = mount(&refuse, Some(options));
        r.check(
            &format!("mount refuses {why} with EINVAL"),
            answer == Err(libc::EINVAL) || mpos_user_tests::host_run(),
        );
        if answer.is_ok() {
            umount(&refuse);
        }
    }

    // ----------------------------------------------------------- the control
    let plain = format!("{base}/plain");
    if mount(&plain, None).is_ok() {
        let ok = std::fs::write(format!("{plain}/hello"), b"hello").is_ok();
        r.check("a mount with no options is the default room and works", ok);
        umount(&plain);
    } else {
        r.check(
            "a mount with no options is the default room (needs CAP_SYS_ADMIN)",
            mpos_user_tests::host_run(),
        );
    }

    let _ = std::fs::remove_dir_all(base);
    r.finish();
}
