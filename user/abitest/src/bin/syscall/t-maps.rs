//! t-maps - `/proc/<pid>/maps`, the file a crash reporter, a leak checker,
//! `pmap` and Mesa's own allocator paths read to find out what is mapped
//! where.
//!
//! The format is the whole contract: six whitespace-separated fields -
//! `start-end perms offset dev inode name` - with the addresses in lowercase
//! hex, the offset zero-padded to eight digits, `dev` as `%02x:%02x`, the
//! inode in decimal, and the name starting in a fixed column (73, measured on
//! Linux 7.2.7 across address and inode widths) when there is one. A reader
//! splits on whitespace and indexes, so "close enough" parses as something
//! else.
//!
//! Pinned here, on the machine, for every kind of mapping this kernel makes:
//! the process's own image (which is *not* in the VMA table - `execve` fills
//! those pages eagerly - and is reported from what the loader recorded), a
//! private anonymous `mmap`, a shared anonymous one, a `memfd` mapping, the
//! stack, and the heap. `[stack]` and `[heap]` are Linux's names for regions
//! that are not files, and the heap's is applied by address because this
//! kernel's heap is an ordinary anonymous mapping.
//!
//! Two divergences are pinned *as* divergences, so that changing either is a
//! visible decision rather than a silent one: an image's and a private file
//! mapping's line carries no `offset` and no `inode`, because this kernel reads
//! such a file's bytes in rather than paging them and has no object left to
//! name; and its `dev` column is `00:00`, which is what this kernel's `stat`
//! reports for every filesystem too - so the two agree rather than disagreeing.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Where the name column starts, measured on Linux 7.2.7.
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
            b'A'..=b'F' => (b - b'A' + 10) as usize,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    Some(value)
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

/// Read `/proc/self/maps` whole. The file is generated per read, so a test
/// that maps something must read it *again* to see the new line.
fn read_maps(buf: &mut [u8]) -> Option<&[u8]> {
    let fd = lin::openat(lin::AT_FDCWD, b"/proc/self/maps\0", lin::O_RDONLY, 0);
    if fd < 0 {
        return None;
    }
    let fd = fd as i32;
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = lin::read(fd, &mut buf[filled..]);
        if n <= 0 {
            break;
        }
        filled += n as usize;
    }
    lin::close(fd);
    Some(&buf[..filled])
}

/// Every line of `maps`, in order.
fn lines<'a>(maps: &'a [u8]) -> impl Iterator<Item = Line<'a>> + 'a {
    maps.split(|&b| b == b'\n')
        .filter(|text| !text.is_empty())
        .filter_map(|text| parse(text))
}

/// The line covering `addr`, if any.
fn line_at(maps: &[u8], addr: usize) -> Option<Line<'_>> {
    lines(maps).find(|line| line.covers(addr, 1))
}

/// Whether some line is named exactly `name`.
fn has_line_named(maps: &[u8], name: &[u8]) -> bool {
    lines(maps).any(|line| line.name == name)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-maps\n");
    let mut failed = false;
    let mut buf = [0u8; 16384];
    // A local whose address is on the stack, for the `[stack]` check.
    let probe = 0u8;

    // --- what the file is ---
    match read_maps(&mut buf) {
        Some(maps) => {
            lin::check_true(
                &mut failed,
                b"the maps file opens and reads",
                !maps.is_empty(),
            );
            lin::check_true(
                &mut failed,
                b"every line ends in a newline",
                maps.last() == Some(&b'\n'),
            );

            // Each field's shape. A reader indexes into the line, so a field of
            // the wrong shape is not a cosmetic problem.
            let mut count = 0usize;
            let mut shaped = true;
            for line in lines(maps) {
                count += 1;
                if line.perms.len() != 4
                    || !matches!(line.perms[3], b's' | b'p')
                    || line.offset.len() != 8
                    || line.dev.len() != 5
                    || line.dev[2] != b':'
                    || line.inode.is_empty()
                    || !line.inode.iter().all(|b| b.is_ascii_digit())
                    || line.start >= line.end
                {
                    shaped = false;
                }
            }
            lin::check_true(&mut failed, b"there are lines to read", count >= 4);
            lin::check_true(
                &mut failed,
                b"and every field has the shape the format promises",
                shaped,
            );

            // --- the image the process is running from ---
            //
            // The strongest property in the file: a crash reporter reads this
            // to find the base address of the program it is unwinding, and an
            // image's pages are filled eagerly here, so no VMA describes them.
            let mut exe = [0u8; 256];
            let exe_len = lin::readlink(b"/proc/self/exe\0", &mut exe);
            lin::check_true(&mut failed, b"the exe link resolves", exe_len > 0);
            let exe = &exe[..exe_len.max(0) as usize];

            lin::check_true(
                &mut failed,
                b"the program's own image is reported under its own path",
                has_line_named(maps, exe),
            );
            match line_at(maps, mpos_user_main as *const () as usize) {
                Some(line) => {
                    lin::check_true(
                        &mut failed,
                        b"the line covering this function's code is the image's",
                        line.name == exe,
                    );
                    // Readable and executable and not writable. The exact word
                    // is the linker script's business; that the segment's own
                    // flags are what is reported is this kernel's.
                    lin::check_true(
                        &mut failed,
                        b"and that line is executable, readable and not writable",
                        line.perms[0] == b'r'
                            && line.perms[1] == b'-'
                            && line.perms[2] == b'x'
                            && line.perms[3] == b'p',
                    );
                }
                None => lin::check_true(
                    &mut failed,
                    b"the line covering this function's code is the image's",
                    false,
                ),
            }

            // --- the stack, under Linux's bracketed name ---
            //
            // The address of an ordinary local: a stack line that did not
            // cover the stack this function is standing on would be the one
            // thing the name promises and fails to deliver.
            let frame = &probe as *const u8 as usize;
            lin::check_true(
                &mut failed,
                b"the stack is listed under Linux's bracketed name",
                has_line_named(maps, b"[stack]"),
            );
            match line_at(maps, frame) {
                Some(line) => lin::check_true(
                    &mut failed,
                    b"and the line covering this frame is that one",
                    line.name == b"[stack]",
                ),
                None => lin::check_true(&mut failed, b"a line covers this function's frame", false),
            }
        }
        None => {
            lin::check_true(&mut failed, b"the maps file opens", false);
        }
    }

    // --- a private anonymous mapping ---
    let private = lin::mmap(
        0,
        3 * 4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(&mut failed, b"an anonymous mapping is made", private > 0);
    let private = private.max(0) as usize;

    // --- a shared anonymous mapping ---
    let shared = lin::mmap(
        0,
        2 * 4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_SHARED | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    lin::check_true(&mut failed, b"a shared mapping is made", shared > 0);
    let shared = shared.max(0) as usize;

    // --- a memfd mapping, which is what a `wl_shm` pool is ---
    let mfd = lin::memfd_create(b"t-maps-buf\0", 0);
    lin::check_true(&mut failed, b"a memfd is created", mfd > 0);
    let mfd = mfd.max(0) as i32;
    lin::check_eq(
        &mut failed,
        b"and given a size",
        lin::raw_sys2(77, mfd as usize, 2 * 4096), // ftruncate
        0,
    );
    let memfd_map = lin::mmap(
        0,
        2 * 4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_SHARED,
        mfd,
        0,
    );
    lin::check_true(&mut failed, b"and mapped shared", memfd_map > 0);
    let memfd_map = memfd_map.max(0) as usize;

    // The three mappings above are only in the file the *next* read produces.
    match read_maps(&mut buf) {
        Some(maps) => {
            match line_at(maps, private) {
                Some(line) => {
                    lin::check_true(
                        &mut failed,
                        b"the private mapping is reported as rw-p",
                        line.perms == b"rw-p",
                    );
                    lin::check_true(
                        &mut failed,
                        b"and covers all three of its pages",
                        line.covers(private, 3 * 4096),
                    );
                    lin::check_true(
                        &mut failed,
                        b"with no object behind it: no inode and no device",
                        line.inode == b"0" && line.dev == b"00:00",
                    );
                    lin::check_true(&mut failed, b"and no name", line.name.is_empty());
                }
                None => lin::check_true(&mut failed, b"the private mapping is listed", false),
            }

            match line_at(maps, shared) {
                Some(line) => {
                    lin::check_true(
                        &mut failed,
                        b"the fourth permission character is the shared bit",
                        line.perms == b"rw-s",
                    );
                    lin::check_true(
                        &mut failed,
                        b"and the shared object is named and has an inode",
                        line.name == b"/memfd: (deleted)" && line.inode != b"0",
                    );
                }
                None => lin::check_true(&mut failed, b"the shared mapping is listed", false),
            }

            match line_at(maps, memfd_map) {
                Some(line) => {
                    lin::check_true(
                        &mut failed,
                        b"the memfd mapping is shared",
                        line.perms == b"rw-s",
                    );
                    lin::check_true(
                        &mut failed,
                        b"and is named after the memfd, as Linux names it",
                        line.name == b"/memfd:t-maps-buf (deleted)",
                    );
                    // The column, not just the field: this is what makes the
                    // last field addressable whatever the address width is.
                    let name_at = line.text.len() - line.name.len();
                    lin::check_eq(
                        &mut failed,
                        b"and its name starts in Linux's name column",
                        name_at as isize,
                        NAME_COLUMN as isize,
                    );
                }
                None => lin::check_true(&mut failed, b"the memfd mapping is listed", false),
            }
        }
        None => lin::check_true(&mut failed, b"the maps file still opens", false),
    }

    // --- the heap, which this kernel makes when `brk` grows it ---
    let base = lin::brk(0);
    lin::check_true(&mut failed, b"the break is readable", base > 0);
    let grown = lin::brk(base.max(0) as usize + 64 * 1024);
    lin::check_true(&mut failed, b"and growing it succeeds", grown > 0);
    match read_maps(&mut buf) {
        Some(maps) => {
            lin::check_true(
                &mut failed,
                b"the heap is listed under Linux's bracketed name",
                has_line_named(maps, b"[heap]"),
            );
            match line_at(maps, base.max(0) as usize) {
                Some(line) => lin::check_true(
                    &mut failed,
                    b"and the line covering the old break is that one",
                    line.name == b"[heap]",
                ),
                None => lin::check_true(&mut failed, b"the heap covers where the break was", false),
            }
        }
        None => lin::check_true(&mut failed, b"the maps file still opens", false),
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-maps: panicked\n")
}
