//! t-maps - `/proc/<pid>/maps`, the file a crash reporter, a leak checker,
//! `pmap` and Mesa's own allocator paths read to find out what is mapped
//! where.
//!
//! The format is the whole contract: six whitespace-separated fields -
//! `start-end perms offset dev inode name` - with the addresses in lowercase
//! hex, the offset zero-padded to eight digits, `dev` as `%02x:%02x`, the
//! inode in decimal, and the name starting in a fixed column (73, measured on
//! Linux 7.2.7 across address and inode widths, and re-measured on this
//! sandbox's host) when there is one. A reader splits on whitespace and
//! indexes, so "close enough" parses as something else.
//!
//! Pinned here, on the machine, for every kind of mapping this kernel makes:
//! the process's own image, a private anonymous `mmap`, a shared anonymous
//! one, a `memfd` mapping, the stack, and the heap. `[stack]` and `[heap]`
//! are Linux's names for regions that are not files, and the heap's is
//! applied by address because this kernel's heap is an ordinary anonymous
//! mapping.
//!
//! # The migration, and the one name that moved
//!
//! The fixture is the named calls: the two anonymous mappings are
//! `rustix::mm::mmap_anonymous`, the memfd is `rustix::fs::memfd_create`
//! sized with `ftruncate` and mapped through `rustix::mm::mmap` (the one
//! mapping with an object behind it), the break is the raw `SYS_brk`
//! (`t-brk`'s door, because `libc::brk` answers the *old* break rather than
//! the new one and this program wants neither), and the image's path is
//! `read_link("/proc/self/exe")`. The parser is the no-std file's parser,
//! on purpose: the field shapes are half the claims, and a parser that
//! split more forgivingly than the format promises would stop testing the
//! format.
//!
//! **The shared anonymous object's name diverged, and the probe is why.**
//! This kernel names the shared anonymous mapping `/memfd: (deleted)` - the
//! tmpfs-style spelling - where the host's Linux names the very same
//! mapping `/dev/zero (deleted)` (measured, `.workbuddy/probe_maps.c`; both
//! kernels agree it *is* a named shared object with a nonzero inode, and
//! the host's `dev` is its tmpfs rather than `00:00`). Linux's own history
//! moved this name once already, so the pin keeps the property - a shared
//! anonymous mapping is *named*, with an inode - and under `MPOS_ON_HOST`
//! accepts the host's spelling beside this kernel's. Everything else the
//! probe measured agrees: the name column is 73 wherever there is a name,
//! a nameless line stops after the inode, `rw-p`/`rw-s`, the bracketed
//! `[stack]` and `[heap]`, and an image line that is `r-xp`.

use mpos_user_tests::{Report, host_run};
use rustix::fd::AsFd;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::mm::{MapFlags, MprotectFlags, ProtFlags, mmap, mmap_anonymous, mprotect, munmap};

/// Where the name column starts, measured on Linux 7.2.7 and re-measured on
/// this sandbox's host (`.workbuddy/probe_maps.c`).
const NAME_COLUMN: usize = 73;

/// One parsed line, borrowing from the buffer the file was read into.
struct Line<'a> {
    start: usize,
    end: usize,
    perms: &'a [u8],
    offset: &'a [u8],
    dev: &'a [u8],
    inode: &'a [u8],
    /// The whole line, without its newline, so a test can locate the name
    /// column in it.
    text: &'a [u8],
    name: &'a [u8],
}

impl Line<'_> {
    fn covers(&self, addr: usize, len: usize) -> bool {
        self.start <= addr && addr + len <= self.end
    }
}

fn hex_field(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for &b in bytes {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as usize,
            b'a'..=b'f' => (b - b'a' + 10) as usize,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    Some(value)
}

fn is_hex(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| b.is_ascii_hexdigit())
}

/// Split one line into its six fields, or `None` when it is not one.
fn parse(text: &[u8]) -> Option<Line<'_>> {
    let mut fields = [&text[..0]; 5];
    let mut rest = text;
    for field in fields.iter_mut() {
        let space = rest.iter().position(|&b| b == b' ')?;
        *field = &rest[..space];
        rest = &rest[space + 1..];
    }
    // Everything past the fifth space is the padding and the name; a name never
    // has leading spaces, so trimming them finds it.
    let name = match rest.iter().position(|&b| b != b' ') {
        Some(at) => &rest[at..],
        None => &rest[..0],
    };
    let dash = fields[0].iter().position(|&b| b == b'-')?;
    Some(Line {
        start: hex_field(&fields[0][..dash])?,
        end: hex_field(&fields[0][dash + 1..])?,
        perms: fields[1],
        offset: fields[2],
        dev: fields[3],
        inode: fields[4],
        text,
        name,
    })
}

/// Every line of `maps`, in order.
fn lines(maps: &[u8]) -> impl Iterator<Item = Line<'_>> {
    maps.split(|&b| b == b'\n')
        .filter(|text| !text.is_empty())
        .filter_map(parse)
}

/// The line covering `addr`, if any.
fn line_at(maps: &[u8], addr: usize) -> Option<Line<'_>> {
    lines(maps).find(|line| line.covers(addr, 1))
}

/// Whether some line is named exactly `name`.
fn has_line_named(maps: &[u8], name: &[u8]) -> bool {
    lines(maps).any(|line| line.name == name)
}

/// Read `/proc/self/maps` whole. The file is generated per read, so a test
/// that maps something must read it *again* to see the new line.
fn read_maps() -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open("/proc/self/maps")?.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The break, through the raw door (`t-brk`'s): `libc::brk` answers the old
/// break, and the claims below want the kernel's own answers.
fn brk(addr: usize) -> isize {
    // SAFETY: no arguments beyond the address, and the answers are the
    // syscall's own.
    unsafe { libc::syscall(libc::SYS_brk, addr) as isize }
}

/// A code site the image check can locate: never inlined, so the line the
/// maps file reports for its address is the image's own.
#[inline(never)]
fn pinned_code_site() {}

fn main() {
    println!("t-maps");
    let mut r = Report::new();
    // A local whose address is on the stack, for the `[stack]` check.
    let probe = 0u8;

    // --- what the file is ---
    match read_maps() {
        Ok(maps) => {
            r.check("the maps file opens and reads", !maps.is_empty());
            r.check("every line ends in a newline", maps.last() == Some(&b'\n'));

            // Each field's shape. A reader indexes into the line, so a field of
            // the wrong shape is not a cosmetic problem. The `dev` field is
            // checked against the format (`%02x:%02x`, so at least two hex
            // digits a side) rather than against a width: this kernel's
            // device numbers are small enough to pad to `00:00`, where the
            // host's major rides past two digits (`103:06` in the probe).
            let mut count = 0usize;
            let mut shaped = true;
            for line in lines(&maps) {
                count += 1;
                let dev_colon = line.dev.iter().filter(|&&b| b == b':').count();
                let dev_ok = dev_colon == 1
                    && match line.dev.iter().position(|&b| b == b':') {
                        Some(at) => {
                            line.dev.len() >= 5
                                && is_hex(&line.dev[..at])
                                && is_hex(&line.dev[at + 1..])
                        }
                        None => false,
                    };
                if line.perms.len() != 4
                    || !matches!(line.perms[3], b's' | b'p')
                    || line.offset.len() != 8
                    || !is_hex(line.offset)
                    || !dev_ok
                    || line.inode.is_empty()
                    || !line.inode.iter().all(|b| b.is_ascii_digit())
                    || line.start >= line.end
                {
                    shaped = false;
                }
            }
            r.check("there are lines to read", count >= 4);
            r.check("and every field has the shape the format promises", shaped);

            // --- the image the process is running from ---
            //
            // The strongest property in the file: a crash reporter reads this
            // to find the base address of the program it is unwinding.
            match std::fs::read_link("/proc/self/exe") {
                Ok(exe) => {
                    let exe = exe.as_os_str().as_encoded_bytes();
                    r.check("the exe link resolves", !exe.is_empty());
                    r.check(
                        "the program's own image is reported under its own path",
                        has_line_named(&maps, exe),
                    );
                    let code_site = pinned_code_site as *const () as usize;
                    match line_at(&maps, code_site) {
                        Some(line) => {
                            r.check(
                                "the line covering this function's code is the image's",
                                line.name == exe,
                            );
                            // Readable and executable and not writable. The
                            // exact word is the linker script's business; that
                            // the segment's own flags are what is reported is
                            // this kernel's.
                            r.check(
                                "and that line is executable, readable and not writable",
                                line.perms[0] == b'r'
                                    && line.perms[1] == b'-'
                                    && line.perms[2] == b'x'
                                    && line.perms[3] == b'p',
                            );
                        }
                        None => r.check(
                            "the line covering this function's code is the image's",
                            false,
                        ),
                    }
                }
                Err(_) => r.check("the exe link resolves", false),
            }

            // --- the stack, under Linux's bracketed name ---
            //
            // The address of an ordinary local: a stack line that did not
            // cover the stack this function is standing on would be the one
            // thing the name promises and fails to deliver.
            let frame = &probe as *const u8 as usize;
            r.check(
                "the stack is listed under Linux's bracketed name",
                has_line_named(&maps, b"[stack]"),
            );
            match line_at(&maps, frame) {
                Some(line) => r.check(
                    "and the line covering this frame is that one",
                    line.name == b"[stack]",
                ),
                None => r.check("a line covers this function's frame", false),
            }
        }
        Err(_) => r.check("the maps file opens", false),
    }

    // --- a private anonymous mapping ---
    let private = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            3 * 4096,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    };
    r.check("an anonymous mapping is made", private.is_ok());
    let private = private.map(|p| p as usize).unwrap_or(0);

    // --- a shared anonymous mapping ---
    let shared = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            2 * 4096,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
        )
    };
    r.check("a shared mapping is made", shared.is_ok());
    let shared = shared.map(|p| p as usize).unwrap_or(0);

    // --- a mapping nobody may touch ---
    //
    // `PROT_NONE` is what a reservation and a guard page are, and Linux prints
    // `---p` for it (measured, `.workbuddy/probe_maps_prot.c`: a two-page
    // reservation, and a `PROT_NONE`/`rw-p`/`PROT_NONE` span). This kernel's VMA
    // keeps its "the range is present" fact in the same `READ` bit a grant would
    // use, so the column has to be the *grant*: read off the internal bit, this
    // mapping answered `r--p`, which a tool deciding whether it may read a range
    // cannot tell from a readable one.
    let guard = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            2 * 4096,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    };
    r.check("a PROT_NONE reservation is made", guard.is_ok());
    let guard = guard.map(|p| p as usize).unwrap_or(0);

    // --- a memfd mapping, which is what a `wl_shm` pool is ---
    let mapped = (|| -> Option<usize> {
        let mfd = memfd_create("t-maps-buf", MemfdFlags::empty()).ok()?;
        r.check("a memfd is created", true);
        r.check("and given a size", ftruncate(mfd.as_fd(), 2 * 4096).is_ok());
        let map = unsafe {
            mmap(
                core::ptr::null_mut(),
                2 * 4096,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                mfd.as_fd(),
                0,
            )
        };
        r.check("and mapped shared", map.is_ok());
        map.ok().map(|p| p as usize)
    })()
    .unwrap_or(0);

    // The three mappings above are only in the file the *next* read produces.
    match read_maps() {
        Ok(maps) => {
            match line_at(&maps, private) {
                Some(line) => {
                    r.check(
                        "the private mapping is reported as rw-p",
                        line.perms == b"rw-p",
                    );
                    r.check(
                        "and covers all three of its pages",
                        line.covers(private, 3 * 4096),
                    );
                    r.check(
                        "with no object behind it: no inode and no device",
                        line.inode == b"0" && line.dev == b"00:00",
                    );
                    r.check("and no name", line.name.is_empty());
                }
                None => r.check("the private mapping is listed", false),
            }

            match line_at(&maps, shared) {
                Some(line) => {
                    r.check(
                        "the fourth permission character is the shared bit",
                        line.perms == b"rw-s",
                    );
                    // The shared anonymous object is *named* and has an inode.
                    // The name itself is the one divergence the host measured:
                    // this kernel spells it the tmpfs way, the host's Linux
                    // the `/dev/zero` way (`.workbuddy/probe_maps.c`), and a
                    // host run accepts the host's beside this kernel's.
                    r.check(
                        "and the shared object is named and has an inode",
                        (line.name == b"/memfd: (deleted)"
                            || host_run() && line.name == b"/dev/zero (deleted)")
                            && line.inode != b"0",
                    );
                }
                None => r.check("the shared mapping is listed", false),
            }

            match line_at(&maps, guard) {
                Some(line) => r.check(
                    "a PROT_NONE reservation is reported with no permission at all",
                    line.perms == b"---p",
                ),
                None => r.check("the PROT_NONE reservation is listed", false),
            }

            match line_at(&maps, mapped) {
                Some(line) => {
                    r.check("the memfd mapping is shared", line.perms == b"rw-s");
                    r.check(
                        "and is named after the memfd, as Linux names it",
                        line.name == b"/memfd:t-maps-buf (deleted)",
                    );
                    // The column, not just the field: this is what makes the
                    // last field addressable whatever the address width is.
                    let name_at = line.text.len() - line.name.len();
                    r.check(
                        "and its name starts in Linux's name column",
                        name_at == NAME_COLUMN,
                    );
                }
                None => r.check("the memfd mapping is listed", false),
            }
        }
        Err(_) => r.check("the maps file still opens", false),
    }

    // --- a private file mapping, and the two calls that rewrite it ---
    // A `MAP_PRIVATE` mapping of a file is what `record_eager_range` exists for:
    // its pages are read in at `mmap` time, so this kernel leaves no VMA and
    // `/proc/<pid>/maps` is built from the recorded range instead. That makes
    // the description the *only* copy of the mapping's permissions and extent,
    // so `munmap` and `mprotect` have to carry it - which is the claim here,
    // and the reason it is a claim: both were wired but neither was pinned, so
    // a range that outlived its pages would have read as a mapping that is not
    // there (`docs/notes/` §6.10 said so before this pin existed).
    let private_file = (|| -> Option<usize> {
        let mfd = memfd_create("t-maps-ranges", MemfdFlags::empty()).ok()?;
        r.check("a second memfd is created", true);
        r.check(
            "and sized to three pages",
            ftruncate(mfd.as_fd(), 3 * 4096).is_ok(),
        );
        let map = unsafe {
            mmap(
                core::ptr::null_mut(),
                3 * 4096,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE,
                mfd.as_fd(),
                0,
            )
        };
        r.check("and mapped private", map.is_ok());
        map.ok().map(|p| p as usize)
    })()
    .unwrap_or(0);

    if private_file != 0 {
        match read_maps() {
            Ok(maps) => match line_at(&maps, private_file) {
                Some(line) => {
                    r.check(
                        "the private file mapping reads rw-p over its three pages",
                        line.perms == b"rw-p" && line.covers(private_file, 3 * 4096),
                    );
                }
                None => r.check("the private file mapping is listed", false),
            },
            Err(_) => r.check("the maps file opens", false),
        }

        // A page out of the middle goes, and the description has to split.
        let hole = private_file + 4096;
        r.check(
            "the middle page is unmapped",
            unsafe { munmap(hole as *mut _, 4096) }.is_ok(),
        );
        match read_maps() {
            Ok(maps) => {
                r.check(
                    "the hole has no line of its own",
                    line_at(&maps, hole).is_none(),
                );
                match line_at(&maps, private_file) {
                    Some(line) => r.check(
                        "the first page is still there, and now shorter",
                        line.perms == b"rw-p" && line.end == hole,
                    ),
                    None => r.check("the first page survives the unmap", false),
                }
                match line_at(&maps, private_file + 2 * 4096) {
                    Some(line) => r.check(
                        "and the last page is its own range",
                        line.perms == b"rw-p" && line.start == hole + 4096,
                    ),
                    None => r.check("the last page survives the unmap", false),
                }
            }
            Err(_) => r.check("the maps file opens after the unmap", false),
        }

        // A permission change over the surviving first page, and no other.
        let prot = |addr: usize, len: usize, flags: MprotectFlags| unsafe {
            mprotect(addr as *mut _, len, flags)
        };
        r.check(
            "the first page is made read-only",
            prot(private_file, 4096, MprotectFlags::READ).is_ok(),
        );
        match read_maps() {
            Ok(maps) => {
                match line_at(&maps, private_file) {
                    Some(line) => r.check(
                        "the page reads r--p after the mprotect",
                        line.perms == b"r--p",
                    ),
                    None => r.check("the protected page is still listed", false),
                }
                match line_at(&maps, private_file + 2 * 4096) {
                    Some(line) => r.check(
                        "and the page the call did not name kept rw-p",
                        line.perms == b"rw-p",
                    ),
                    None => r.check("the untouched page is still listed", false),
                }
            }
            Err(_) => r.check("the maps file opens after the mprotect", false),
        }
    }

    // --- the heap, which grows when the break moves ---
    let base = brk(0);
    r.check("the break is readable", base > 0);
    let grown = brk(base.max(0) as usize + 64 * 1024);
    r.check("and growing it succeeds", grown > 0);
    match read_maps() {
        Ok(maps) => {
            r.check(
                "the heap is listed under Linux's bracketed name",
                has_line_named(&maps, b"[heap]"),
            );
            match line_at(&maps, base.max(0) as usize) {
                Some(line) => r.check(
                    "and the line covering the old break is that one",
                    line.name == b"[heap]",
                ),
                None => r.check("the heap covers where the break was", false),
            }
        }
        Err(_) => r.check("the maps file still opens", false),
    }

    r.finish()
}
