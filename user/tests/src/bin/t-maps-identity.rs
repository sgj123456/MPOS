//! t-maps-identity - `/proc/<pid>/maps` reports the **filesystem** device, not zeros.
//!
//! Found while analysing Chromium (`docs/notes/07-chromium-analysis.md`, finding
//! C): crashpad read a module's segments by the device `maps` printed, every line
//! said `00:00`, and it could not associate a mapping with its file.
//!
//! Measured before the claim: `stat -c %d` on a file answered a real device while
//! its `maps` line printed `00:00` - the two files disagreed.
//!
//! | what | Linux |
//! |---|---|
//! | `maps` device column | `st_dev`, the **filesystem**, as `maj:min` |
//! | two files on one filesystem | the same device |
//! | a file on another filesystem (`/tmp` vs `/`) | a different device |
//! | the segment's file offset | the segment's own (`00001000`, …) |
//!
//! **Two functions answered the device half with `rdev()`** - the device a
//! *device node* names, and `(0,0)` for every ordinary file: `FilePages` (a
//! private file mapping) and `MappedInode`. Both now ask the mount table, the same
//! door `fstat` uses, so `stat` and `maps` agree by construction.
//!
//! **The mapping this test checks is one it makes itself.** `exec` builds
//! *pre-populated private anonymous* VMAs for an image, so a program's own text is
//! deliberately not file-backed and has no `maps` line at all - a first draft
//! asserted on `/proc/self/exe`'s line, found none, and hung a lane.

use mpos_user_tests::Report;

/// `(maj, min)` for a `st_dev`.
fn split(dev: u64) -> (u32, u32) {
    let major = (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32;
    let minor = ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32;
    (major, minor)
}

fn main() {
    println!("t-maps-identity");
    let mut r = Report::new();
    use std::os::unix::fs::MetadataExt;

    // === a file this test maps itself ======================================
    let target = ["/bin/busybox", "/lib/ld-musl-x86_64.so.1", "/init"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .copied()
        .unwrap_or("");
    r.check(
        &format!("a file to map exists ({target})"),
        !target.is_empty(),
    );
    if target.is_empty() {
        r.finish();
    }

    let stat_dev = std::fs::metadata(target)
        .map(|m| split(m.dev()))
        .unwrap_or((0, 0));
    let stat_ino = std::fs::metadata(target).map(|m| m.ino()).unwrap_or(0);
    r.check(
        &format!("the file's stat device is non-zero ({stat_dev:?})"),
        stat_dev != (0, 0),
    );

    // One page at a **non-zero** offset: both columns under test at once.
    let mut mapped: *mut libc::c_void = libc::MAP_FAILED;
    if let Ok(file) = std::fs::File::open(target) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: a live descriptor, a page-aligned length, a private read-only
        // mapping; `MAP_FAILED` is checked below.
        mapped = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                4096,
            )
        };
    }
    r.check("the file maps", mapped != libc::MAP_FAILED);
    if mapped == libc::MAP_FAILED {
        r.finish();
    }

    let text = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let addr = mapped as usize;
    let parsed = text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let _perms = fields.next()?;
        let offset = u64::from_str_radix(fields.next()?, 16).ok()?;
        let dev = fields.next()?;
        let inode = fields.next()?.parse::<u64>().ok()?;
        let (lo, hi) = range.split_once('-')?;
        let lo = usize::from_str_radix(lo, 16).ok()?;
        let hi = usize::from_str_radix(hi, 16).ok()?;
        if addr < lo || addr >= hi {
            return None;
        }
        let (maj, min) = dev.split_once(':')?;
        Some((
            offset,
            (
                u32::from_str_radix(maj, 16).ok()?,
                u32::from_str_radix(min, 16).ok()?,
            ),
            inode,
        ))
    });

    match parsed {
        Some((offset, map_dev, inode)) => {
            // **The defect's claim**: the device column is the file's
            // filesystem, not `00:00` - which is what every file printed.
            r.check(
                &format!("the mapping's device is the file's ({map_dev:?})"),
                map_dev == stat_dev,
            );
            r.check(
                &format!("its inode matches stat ({inode} vs {stat_ino})"),
                inode == stat_ino,
            );
            // The offset column is the segment's own. It was already correct
            // before the fix; pinned so a later change cannot quietly zero it.
            r.check(
                &format!("and the offset is the one asked for ({offset:#x})"),
                offset == 0x1000,
            );
        }
        None => r.check("the mapping has a maps line", false),
    }
    // SAFETY: the mapping made above.
    unsafe { libc::munmap(mapped, 4096) };

    // === the program's own exec image ======================================
    //
    // `exec` places the image in pages it fills itself, so the *paging* is not
    // the file's - but `maps` describes where the bytes came from, and every
    // tool that attributes an address to a file (a crash reporter, `pmap`, a
    // symboliser) reads exactly these columns. Linux prints the segment's own
    // file offset, the file's device and its inode on each line, and a separate
    // zero line for the `.bss` tail
    // (`build/scratch/execmap/{probe6,s1}.c`).
    let exe = std::fs::read_link("/proc/self/exe").unwrap_or_default();
    let exe_str = exe.to_string_lossy().to_string();
    if !exe_str.is_empty() {
        let exe_dev = std::fs::metadata(&exe).map(|m| split(m.dev())).ok();
        let exe_ino = std::fs::metadata(&exe).map(|m| m.ino()).ok();
        // A struct rather than a six-tuple: the fields are read in several of
        // the claims below, and a tuple of six is unreadable at every use.
        struct Line {
            #[allow(dead_code)] // the address range is parsed for the shape only
            lo: usize,
            offset: u64,
            dev: (u32, u32),
            inode: u64,
            named: bool,
            text: String,
        }
        let lines: Vec<Line> = text
            .lines()
            .filter_map(|line| {
                let mut f = line.split_whitespace();
                let range = f.next()?;
                let _perms = f.next()?;
                let offset = u64::from_str_radix(f.next()?, 16).ok()?;
                let dev = f.next()?;
                let inode = f.next()?.parse::<u64>().ok()?;
                let (lo, hi) = range.split_once('-')?;
                let lo = usize::from_str_radix(lo, 16).ok()?;
                let hi = usize::from_str_radix(hi, 16).ok()?;
                let (maj, min) = dev.split_once(':')?;
                let _ = hi;
                Some(Line {
                    lo,
                    offset,
                    dev: (
                        u32::from_str_radix(maj, 16).ok()?,
                        u32::from_str_radix(min, 16).ok()?,
                    ),
                    inode,
                    named: line.split_whitespace().count() > 5,
                    text: line.to_string(),
                })
            })
            .collect();
        // The image's own lines: named with the exe path.
        let image: Vec<&Line> = lines
            .iter()
            .filter(|l| l.text.ends_with(&exe_str))
            .collect();
        r.check(
            &format!(
                "the exec image has named maps lines ({} lines)",
                image.len()
            ),
            !image.is_empty(),
        );
        if let (Some(dev), Some(ino)) = (exe_dev, exe_ino) {
            r.check(
                "every exec-image line reports the file's device and inode",
                !image.is_empty() && image.iter().all(|l| l.dev == dev && l.inode == ino),
            );
            // The offsets are the segments' own, so at least one is non-zero:
            // an image whose every line said 0 is the defect this pins.
            let nonzero = image.iter().any(|l| l.offset != 0);
            r.check(
                &format!("and its segment offsets are the file's own (non-zero: {nonzero})"),
                nonzero,
            );
        }
        // The `.bss` tail, when a segment has one, is a **separate** line
        // whose three columns are zero and which carries no name - Linux's
        // `rw-p 00000000 00:00 0 ` with a blank last column
        // (`build/scratch/execmap/probe6.c`). A program whose loads have no
        // memory tail has none, so the claim only says the shape holds for the
        // lines that are there.
        let unnamed_image: Vec<&Line> = lines
            .iter()
            .filter(|l| !l.named && l.text.split_whitespace().count() == 5)
            .collect();
        let tail_ok = unnamed_image
            .iter()
            .all(|l| l.offset == 0 && l.dev == (0, 0) && l.inode == 0);
        r.check(
            &format!(
                "an unnamed range reports no file ({} unnamed line(s))",
                unnamed_image.len()
            ),
            tail_ok,
        );
    }

    // === two files on one filesystem share the device ======================
    let root_dev = std::fs::metadata("/").map(|m| split(m.dev())).ok();
    let tmp_dev = std::fs::metadata("/tmp").map(|m| split(m.dev())).ok();
    if let (Some(root_dev), Some(tmp_dev)) = (root_dev, tmp_dev) {
        r.check(
            &format!("the root filesystem has a device ({root_dev:?})"),
            root_dev != (0, 0),
        );
        r.check(
            &format!("and /tmp is a different one ({tmp_dev:?})"),
            tmp_dev != root_dev,
        );
        // The mapped file is on one of them, and which one is what a reader sees.
        r.check(
            &format!("and the mapped file's device is one of them ({stat_dev:?})"),
            stat_dev == root_dev || stat_dev == tmp_dev,
        );
    } else {
        r.check("both roots are statable", mpos_user_tests::host_run());
    }

    r.finish()
}
