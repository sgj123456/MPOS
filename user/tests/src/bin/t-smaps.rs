//! `/proc/<pid>/smaps`: the per-mapping accounting `maps` does not carry.
//!
//! The file is a *parser's* file - `smem`, `ps_mem` and every compositor asking
//! how much of a client's buffer is resident read it as one record per mapping,
//! each starting with the same line `/proc/<pid>/maps` prints and then one
//! `Name: value` line per field. So the claims here are about two things at
//! once: that the **shape** is Linux's (a header that is the `maps` line byte
//! for byte, Linux's twenty-four field lines in Linux's order, and a `VmFlags`
//! line that ends with the space Linux's does), and that the **numbers** this
//! kernel answers for a fixture the test built itself are the ones the kernel's
//! own model implies.
//!
//! # Why the fixtures are shaped the way they are
//!
//! Two measurements decided that, and both are about the *host* the claims were
//! first run against (`.workbuddy/probe_smaps*.c`):
//!
//! * **Adjacent mappings with equal flags are one VMA.** The first draft mapped
//!   two anonymous ranges in a row and the host merged them into a single
//!   five-page record - so "Size is the mapping's length" became a claim about
//!   a number the test had not chosen. Every fixture below is therefore bounded
//!   by a `PROT_NONE` page on both sides, which no other mapping can merge
//!   across.
//! * **Reading a file can move the heap.** `[heap]`'s own range changed between
//!   two reads of the two files, because the buffer the reader allocates is
//!   itself a mapping. The header comparison is therefore made over the records
//!   two `smaps` reads agree on, and the buffers are sized before the reads.
//!
//! A record is found by *containing* an address rather than by starting at one,
//! because a mapping's address is what the test knows and where the kernel put
//! the VMA's start is what the merge rule above can move.
//!
//! # What is not Linux's here
//!
//! Three of the accounting numbers cannot be Linux's, and each is a *rule*
//! rather than an approximation (`docs/uapi/` §5.2 carries the measurement and
//! this file pins the rule):
//!
//! * `Pss` and `Pss_Dirty` are `Rss`. Linux divides a page by how many address
//!   spaces map it; this kernel keeps no mapcount, so a page two tasks share - a
//!   `fork`ed anonymous page, a `memfd` mapped twice - is charged whole to each.
//!   Measured on a host: the same page reads `Rss: 4 kB` with `Pss: 2 kB` when
//!   two address spaces map it.
//! * every resident page is `Private_Dirty`, and `Shared_*`/`Private_Clean` are
//!   zero. Nothing here records whether a page has been written, and nothing
//!   keeps a page cache to be clean against, so the one bucket every page can
//!   state is "resident, this mapping's own, dirty". Linux's four buckets come
//!   from its page cache and its COW states: the host's own answers for these
//!   fixtures are `Private_Dirty` for a written anonymous page and for a shared
//!   `memfd` page - which this kernel reproduces - and `Shared_Dirty` for a page
//!   a `fork` shares, which it cannot.
//! * `Anonymous` follows the mapping's *kind*: a range with no object behind it
//!   is anonymous, an object's range is not. The host agrees on both shapes
//!   below (a written anonymous page is anonymous, a `memfd` page is not).
//!
//! Two words Linux prints are absent from `VmFlags`: `ac` (overcommit
//! accounting) and `sd` (soft-dirty tracking), because neither exists here. The
//! claim is that this kernel's words are Linux's words in Linux's order - so a
//! host run, which prints two more, accepts its own answer and says so.

use mpos_user_tests::{Report, host_run};

const PAGE: usize = 4096;

/// One `/proc/<pid>/smaps` record, parsed the way a reader parses it.
struct Record {
    /// The `maps` line the record starts with.
    header: String,
    /// Every field line's key and value, in the order the file prints them -
    /// the value is the words after the colon, so a claim about a number is
    /// also a claim about the unit and the padding.
    fields: Vec<(String, String)>,
    /// The `VmFlags` words, in order.
    flags: Vec<String>,
}

impl Record {
    fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// The line a mapping's record starts with? Its first field is the address
/// range - two hex numbers around a `-` - and none of the field lines has one.
fn is_header(line: &str) -> bool {
    line.split(' ')
        .next()
        .is_some_and(|field| field.contains('-') && !field.contains(':'))
}

/// The `start-end` pair a header line begins with.
fn range_of(line: &str) -> Option<(u64, u64)> {
    let field = line.split(' ').next()?;
    let (start, end) = field.split_once('-')?;
    Some((
        u64::from_str_radix(start, 16).ok()?,
        u64::from_str_radix(end, 16).ok()?,
    ))
}

/// The record whose range contains `addr`, or `None` when no record does.
fn record_at(text: &str, addr: u64) -> Option<Record> {
    let mut out: Option<Record> = None;
    for line in text.lines() {
        if is_header(line) {
            if out.is_some() {
                break;
            }
            let (start, end) = range_of(line)?;
            if start <= addr && addr < end {
                out = Some(Record {
                    header: line.to_owned(),
                    fields: Vec::new(),
                    flags: Vec::new(),
                });
            }
            continue;
        }
        let Some(record) = out.as_mut() else { continue };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key == "VmFlags" {
            record.flags = value.split_whitespace().map(str::to_owned).collect();
        } else {
            record
                .fields
                .push((key.to_owned(), value.trim().to_owned()));
        }
    }
    out
}

/// Linux's field lines, in Linux's order (`fs/proc/task_mmu.c`'s `show_smap`),
/// measured over a whole process on a current host (`.workbuddy/probe_smaps.c`,
/// where every one of them appeared on every record).
const FIELD_ORDER: [&str; 24] = [
    "Size",
    "KernelPageSize",
    "MMUPageSize",
    "Rss",
    "Pss",
    "Pss_Dirty",
    "Shared_Clean",
    "Shared_Dirty",
    "Private_Clean",
    "Private_Dirty",
    "Referenced",
    "Anonymous",
    "KSM",
    "LazyFree",
    "AnonHugePages",
    "ShmemPmdMapped",
    "FilePmdMapped",
    "Shared_Hugetlb",
    "Private_Hugetlb",
    "Swap",
    "SwapPss",
    "Locked",
    "THPeligible",
    "ProtectionKey",
];

/// Read a whole `/proc` file, with the buffer's capacity taken *before* the
/// read so that the reading itself does not move the heap between two files
/// this test compares.
fn read_proc(path: &str) -> String {
    let mut text = String::with_capacity(64 * 1024);
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let _ = std::io::Read::read_to_string(&mut file, &mut text);
        }
        Err(_) => return String::new(),
    }
    text
}

/// Read a whole `/proc` file into a buffer the caller keeps and reuses.
///
/// A read that allocated can add a mapping of its own, and the rollup's claim
/// below is that a total and a sum of the same thing agree - which a mapping
/// appearing between the two reads would break. The buffer is therefore the
/// caller's, sized once, and the first pass is thrown away.
fn read_into(path: &str, into: &mut String) {
    into.clear();
    if let Ok(mut file) = std::fs::File::open(path) {
        let _ = std::io::Read::read_to_string(&mut file, into);
    }
}

/// Linux's `smaps_rollup` field lines, in Linux's order - twenty-two of them,
/// measured on a current host (`.workbuddy/probe_rollup.c`).
///
/// It is `smaps`' list with three taken out and three put in: a total has no
/// `Size`, `KernelPageSize` or `MMUPageSize` to report, and no `THPeligible`,
/// `ProtectionKey` or `VmFlags` - those are one mapping's - and it is the only
/// spelling that carries `Pss_Anon`, `Pss_File` and `Pss_Shmem`, the split of
/// the proportional set size.
const ROLLUP_ORDER: [&str; 22] = [
    "Rss",
    "Pss",
    "Pss_Dirty",
    "Pss_Anon",
    "Pss_File",
    "Pss_Shmem",
    "Shared_Clean",
    "Shared_Dirty",
    "Private_Clean",
    "Private_Dirty",
    "Referenced",
    "Anonymous",
    "KSM",
    "LazyFree",
    "AnonHugePages",
    "ShmemPmdMapped",
    "FilePmdMapped",
    "Shared_Hugetlb",
    "Private_Hugetlb",
    "Swap",
    "SwapPss",
    "Locked",
];

/// The number on one `label: value kB` line of a file, summed over every line
/// that carries the label - which is one per record, and therefore the total.
fn total_kb(text: &str, label: &str) -> u64 {
    let needle = format!("{label}:");
    text.lines()
        .filter(|line| line.starts_with(needle.as_str()))
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter_map(|word| word.parse::<u64>().ok())
        .sum()
}

/// The number on one line of the rollup, which has one line per label.
fn rollup_kb(text: &str, label: &str) -> Option<u64> {
    let needle = format!("{label}:");
    text.lines()
        .find(|line| line.starts_with(needle.as_str()))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|word| word.parse::<u64>().ok())
}

/// A fixture bounded by `PROT_NONE` pages: `pages` of `prot` in the middle of a
/// span nobody can merge with, and the address of the middle's first page.
///
/// The guards are what make a record's *size* the test's own number: a mapping
/// whose neighbour has the same flags is one VMA to both kernels (measured on
/// the host, where the first draft of this test's two fixtures came back as a
/// single five-page record).
fn bounded(pages: usize, prot: libc::c_int) -> (u64, u64) {
    let span = pages + 2;
    let base = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            span * PAGE,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return (0, 0);
    }
    let middle = unsafe { (base as *mut u8).add(PAGE) };
    let rc = unsafe { libc::mprotect(middle as *mut libc::c_void, pages * PAGE, prot) };
    if rc != 0 {
        return (0, 0);
    }
    (middle as u64, base as u64)
}

fn main() {
    let mut r = Report::new();

    // The fixture: four pages with three faulted in, so `Size` and `Rss` are
    // different numbers and a reader that confuses them is caught.
    let (busy, guard) = bounded(4, libc::PROT_READ | libc::PROT_WRITE);
    r.check("the fixture mapping was made", busy != 0);
    for page in 0..3 {
        if busy != 0 {
            unsafe { ((busy as *mut u8).add(page * PAGE)).write_volatile(1) };
        }
    }
    // And one nothing has touched, whose every accounting line is zero.
    let (quiet, _) = bounded(2, libc::PROT_READ);
    r.check("the untouched mapping was made", quiet != 0);

    let text = read_proc("/proc/self/smaps");
    r.check(
        "reading /proc/self/smaps answered something",
        !text.is_empty(),
    );

    // ---- the shape ------------------------------------------------------
    //
    // Every record's header must be one of the lines `/proc/<pid>/maps` prints,
    // byte for byte: the two files describe one address space, and a reader may
    // compare them (measured on a host over a process whose mappings covered
    // every shape: 34 of 34 agreed).
    //
    // The comparison is over the records two `smaps` reads agree on, because
    // reading a file allocates and an allocation can move the program break -
    // which moves `[heap]`'s record between one read and the next (measured on
    // the host: the heap's line is the one thing a reader cannot ask this
    // question about, because it is the reader that changes it).
    let maps = read_proc("/proc/self/maps");
    let again = read_proc("/proc/self/smaps");
    let header_lines: Vec<&str> = maps.lines().collect();
    let headers: Vec<&str> = text.lines().filter(|l| is_header(l)).collect();
    let stable: Vec<&str> = headers
        .iter()
        .copied()
        .filter(|h| again.lines().any(|l| l == *h))
        .collect();
    r.check("every record has a header", headers.len() > 3);
    r.check(
        "and every header is a line of /proc/self/maps",
        stable.len() > 3 && stable.iter().all(|h| header_lines.contains(h)),
    );

    // The field lines: Linux's names, in Linux's order, nothing missing and
    // nothing invented. This is the claim a parser's correctness rests on.
    if let Some(record) = record_at(&text, busy) {
        let keys: Vec<&str> = record.fields.iter().map(|(k, _)| k.as_str()).collect();
        r.check(
            "the record's field lines are Linux's, in Linux's order",
            keys == FIELD_ORDER,
        );
        // The header this file prints for the fixture is the line the other
        // file prints for the same mapping - the property a reader that maps
        // one file onto the other depends on.
        r.check(
            "and the fixture's header is its own line of /proc/self/maps",
            maps.lines().any(|line| line == record.header),
        );

        // The numbers this fixture pins: a four-page mapping of which three
        // pages were faulted in, guarded on both sides.
        r.check(
            "Size is the mapping's length",
            record.field("Size") == Some("16 kB"),
        );
        r.check(
            "KernelPageSize is the frame size",
            record.field("KernelPageSize") == Some("4 kB"),
        );
        r.check(
            "MMUPageSize is the frame size",
            record.field("MMUPageSize") == Some("4 kB"),
        );
        r.check(
            "Rss counts the pages that faulted in",
            record.field("Rss") == Some("12 kB"),
        );
        r.check(
            "Pss is Rss: this kernel keeps no mapcount",
            record.field("Pss") == Some("12 kB"),
        );
        r.check(
            "Pss_Dirty follows Pss for the same reason",
            record.field("Pss_Dirty") == Some("12 kB"),
        );
        r.check(
            "the four shared/private buckets are this kernel's rule",
            record.field("Shared_Clean") == Some("0 kB")
                && record.field("Shared_Dirty") == Some("0 kB")
                && record.field("Private_Clean") == Some("0 kB")
                && record.field("Private_Dirty") == Some("12 kB"),
        );
        r.check(
            "Referenced is the resident set: a page arrives by being accessed",
            record.field("Referenced") == Some("12 kB"),
        );
        r.check(
            "Anonymous is the resident set of a mapping with no object behind it",
            record.field("Anonymous") == Some("12 kB"),
        );
        r.check(
            "the features this kernel does not have answer zero",
            record.field("KSM") == Some("0 kB")
                && record.field("LazyFree") == Some("0 kB")
                && record.field("AnonHugePages") == Some("0 kB")
                && record.field("ShmemPmdMapped") == Some("0 kB")
                && record.field("FilePmdMapped") == Some("0 kB")
                && record.field("Shared_Hugetlb") == Some("0 kB")
                && record.field("Private_Hugetlb") == Some("0 kB")
                && record.field("Swap") == Some("0 kB")
                && record.field("SwapPss") == Some("0 kB"),
        );
        r.check(
            "nothing is locked: mlock takes no lock here, so Locked is the truth",
            record.field("Locked") == Some("0 kB"),
        );
        r.check(
            "the two bare-number lines answer zero",
            record.field("THPeligible") == Some("0") && record.field("ProtectionKey") == Some("0"),
        );
        // The flags this kernel prints are Linux's words in Linux's order. A
        // host run prints two more (`ac`, `sd`) that this kernel has no state
        // for, which is the named divergence rather than a failure.
        let expected = ["rd", "wr", "mr", "mw", "me"];
        let ours: Vec<&str> = record
            .flags
            .iter()
            .filter(|w| !host_run() || (w.as_str() != "ac" && w.as_str() != "sd"))
            .map(String::as_str)
            .collect();
        if host_run() && ours != expected {
            println!("note: the host's own words are {:?}", record.flags);
        }
        r.check(
            "VmFlags carries this kernel's words, in Linux's order",
            ours == expected,
        );
    } else {
        r.check("the fixture's record is in the file", false);
    }

    // A mapping nothing has touched: mapped, and unmaterialised - every
    // accounting line zero, `Size` only the length.
    if let Some(record) = record_at(&text, quiet) {
        r.check(
            "an untouched mapping is Size without Rss",
            record.field("Size") == Some("8 kB")
                && record.field("Rss") == Some("0 kB")
                && record.field("Anonymous") == Some("0 kB")
                && record.field("Private_Dirty") == Some("0 kB"),
        );
    } else {
        r.check("the untouched mapping's record is in the file", false);
    }

    // The guard page a `PROT_NONE` span is made of: mapped, with no resident
    // page, and its flags are the may-words with no permission - which is
    // Linux's own reading of a guard page (measured: `mr mw me`, and `sd`).
    if let Some(record) = record_at(&text, guard) {
        let ours: Vec<&str> = record
            .flags
            .iter()
            .filter(|w| !host_run() || (w.as_str() != "ac" && w.as_str() != "sd"))
            .map(String::as_str)
            .collect();
        r.check(
            "a guard page's flags are the may-words, and no permission",
            ours == ["mr", "mw", "me"],
        );
        r.check(
            "and a guard page is mapped without a resident page",
            record.field("Rss") == Some("0 kB"),
        );
    } else {
        r.check("the guard page's record is in the file", false);
    }

    // ---- a mapping whose pages belong to an object ----------------------
    //
    // A `memfd`'s page is the object's, so it is not anonymous - which is
    // Linux's own answer for the same shape (measured: `Private_Dirty`, and
    // `Anonymous: 0 kB`), and the reason that flag comes from the mapping's
    // kind rather than from a count.
    let mfd = unsafe { libc::memfd_create(c"t-smaps".as_ptr(), 0) };
    r.check("memfd_create answered a descriptor", mfd >= 0);
    if mfd >= 0 {
        let rc = unsafe { libc::ftruncate(mfd, PAGE as i64) };
        let shared = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                mfd,
                0,
            )
        };
        r.check(
            "the memfd was mapped",
            rc == 0 && shared != libc::MAP_FAILED,
        );
        if shared != libc::MAP_FAILED {
            unsafe { (shared as *mut u8).write_volatile(7) };
            let text = read_proc("/proc/self/smaps");
            match record_at(&text, shared as u64) {
                Some(record) => {
                    r.check(
                        "an object's page is resident and not anonymous",
                        record.field("Rss") == Some("4 kB")
                            && record.field("Anonymous") == Some("0 kB"),
                    );
                    r.check(
                        "and its flags say the mapping is shared",
                        record.flags.iter().any(|w| w == "sh")
                            && record.flags.iter().any(|w| w == "ms"),
                    );
                }
                None => r.check("the memfd's record is in the file", false),
            }
        }
    }

    // ---- the rollup: one record for the whole address space -------------
    //
    // The spelling a reader that wants the total and not the walk reads. Its
    // header is the *bounds* of the space rather than one mapping's, and its
    // field lines are the sums of the ones above - which is the property the
    // last two claims here are about.
    //
    // The reads are warmed up first: a read that allocated could add a mapping
    // of its own, and a total read before the addition cannot equal a sum read
    // after it. The buffer `read_proc` takes is sized once and dropped, so the
    // allocator hands the same memory to the next read and the space settles.
    let mut every = String::with_capacity(64 * 1024);
    let mut rollup = String::with_capacity(64 * 1024);
    read_into("/proc/self/smaps", &mut every);
    read_into("/proc/self/smaps_rollup", &mut rollup);
    read_into("/proc/self/smaps", &mut every);
    read_into("/proc/self/smaps_rollup", &mut rollup);
    r.check(
        "reading /proc/self/smaps_rollup answered something",
        !rollup.is_empty(),
    );

    let header = rollup.lines().next().unwrap_or("");
    // The name is in the same column a `maps` line names its mapping, which is
    // what lets one parser read all three files.
    r.check(
        "the rollup's header names it [rollup], in maps' name column",
        header.find("[rollup]") == Some(73),
    );
    // And the header is the bounds: the first mapping's start and the last
    // one's end. The gate VMA is the exception and it is Linux's own -
    // `[vsyscall]` is not a VMA of the address space, so it has a `maps` line
    // and is not part of either bound (measured on a current host).
    let maps_now = read_proc("/proc/self/maps");
    let bounds = |last: bool| -> Option<String> {
        let lines: Vec<&str> = maps_now
            .lines()
            .filter(|line| !line.ends_with("[vsyscall]"))
            .collect();
        let line = if last {
            *lines.last()?
        } else {
            *lines.first()?
        };
        let field = line.split_whitespace().next()?;
        Some(String::from(if last {
            field.split('-').nth(1)?
        } else {
            field.split('-').next()?
        }))
    };
    let ours: Vec<&str> = header.split_whitespace().collect();
    let start = ours.first().and_then(|f| f.split('-').next());
    let end = ours.first().and_then(|f| f.split('-').nth(1));
    r.check(
        "and those bounds are the first and last mapping's, from maps",
        start.is_some()
            && start == bounds(false).as_deref()
            && end.is_some()
            && end == bounds(true).as_deref(),
    );
    // `---p 00000000 00:00 0`: a record that is the whole space is not one
    // mapping's protection, offset, device or inode.
    r.check(
        "and the rest of the header is the empty word Linux prints",
        header.contains(" ---p 00000000 00:00 0 "),
    );

    let keys: Vec<&str> = rollup
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':').map(|(k, _)| k))
        .collect();
    r.check(
        "the rollup's field lines are Linux's, in Linux's order",
        keys == ROLLUP_ORDER,
    );
    // The whole point of the file: each number is the sum of the same number
    // over every record above. `Rss` is the one that cannot be fudged, and
    // `Anonymous` the one this kernel's own rule has to survive (the anon/file
    // split is a split this kernel can make - it is the question `Anonymous`
    // already answers).
    r.check(
        "the rollup's Rss is the sum of Rss over every record",
        rollup_kb(&rollup, "Rss") == Some(total_kb(&every, "Rss")),
    );
    r.check(
        "and its Anonymous is the sum of Anonymous over every record",
        rollup_kb(&rollup, "Anonymous") == Some(total_kb(&every, "Anonymous")),
    );
    // The features this kernel does not have answer zero in the total for the
    // same reason they do in a record: no KSM, no huge pages, no swap, no lock.
    r.check(
        "and the features this kernel does not have answer zero in the total",
        rollup_kb(&rollup, "KSM") == Some(0)
            && rollup_kb(&rollup, "AnonHugePages") == Some(0)
            && rollup_kb(&rollup, "Swap") == Some(0)
            && rollup_kb(&rollup, "SwapPss") == Some(0)
            && rollup_kb(&rollup, "Locked") == Some(0),
    );

    r.check(
        "and a pid nothing answers to has no rollup either",
        std::fs::read_to_string("/proc/424242/smaps_rollup").is_err(),
    );

    // ---- the file is the *pid*'s, not the reader's ----------------------
    //
    // `/proc/<pid>/smaps` is one address space's, and the child's own mapping
    // is in the child's file because the child made it. This is also the path
    // that walks a *named* task's page tables rather than the reader's, which
    // is the half of the implementation a `self`-only test could not see.
    //
    // The child's fixture is a named object, because a `fork`ed child inherits
    // every mapping the parent had: "a mapping the parent does not have" has to
    // be one the parent *could not* have made, and a three-page mapping with
    // two pages resident is otherwise the parent's own fixture's shape. The
    // child *reports the address it got* down a pipe rather than letting the
    // parent guess it from the numbers: the first draft matched on `Size` and
    // `Rss` alone, which is a claim about the heap as much as about the child
    // (the inherited heap can be three pages with two resident, and on the
    // second machine it was not - measured by that draft failing there and
    // passing on the first, which is exactly what a fixture-keyed claim looks
    // like when it is wrong).
    let mut report = [0 as libc::c_int; 2];
    let piped = unsafe { libc::pipe(report.as_mut_ptr()) } == 0;
    r.check("a pipe for the child's address was made", piped);
    let child = if piped { unsafe { libc::fork() } } else { 0 };
    if child == 0 {
        let fd = unsafe { libc::memfd_create(c"t-smaps-child".as_ptr(), 0) };
        let mine = if fd >= 0 && unsafe { libc::ftruncate(fd, 3 * PAGE as i64) } == 0 {
            let base = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    3 * PAGE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE,
                    fd,
                    0,
                )
            };
            if base == libc::MAP_FAILED {
                0
            } else {
                unsafe {
                    (base as *mut u8).write_volatile(1);
                    (base as *mut u8).add(PAGE).write_volatile(2);
                }
                base as usize
            }
        } else {
            0
        };
        if mine == 0 {
            // Nothing to report: the parent's read comes back short, which its
            // own claim names.
            std::process::exit(1);
        }
        let bytes = mine.to_ne_bytes();
        unsafe {
            libc::write(
                report[1],
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
        // Park for good, in a loop that cannot fall out. `pause()` is the
        // obvious spelling and it is *not* used here, because on arm64 musl
        // builds it as `ppoll(NULL, 0, NULL, NULL, 0)` (measured: disassembling
        // musl's `pause.lo` shows syscall 73 with every argument zero) and this
        // kernel answered a zero-`nfds` poll with 0 at once - so the child
        // exited on the second machine and every `Rss` under it read 0, which
        // is how that divergence was found (`docs/notes/` §15.74). The fix is
        // its own landing; this fixture does not wait for it.
        loop {
            let nap = libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            unsafe { libc::nanosleep(&nap, core::ptr::null_mut()) };
        }
    }
    r.check("the child was forked", child > 0);
    if child > 0 {
        let mut buf = [0u8; core::mem::size_of::<usize>()];
        let got =
            unsafe { libc::read(report[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        r.check("and reported where it mapped", got == buf.len() as isize);
        let mine = usize::from_ne_bytes(buf) as u64;
        // Let it settle in the pause.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let path = format!("/proc/{child}/smaps");
        let text = read_proc(&path);
        r.check("the child's own file can be read", !text.is_empty());
        match record_at(&text, mine) {
            Some(record) => r.check(
                "and it carries the child's own mapping, three pages with two resident",
                record.field("Size") == Some("12 kB") && record.field("Rss") == Some("8 kB"),
            ),
            None => r.check(
                "and it carries the child's own mapping, three pages with two resident",
                false,
            ),
        }
        // The other half of "one file per address space": a mapping the child
        // *inherited* reads the same in both files, because the same pages are
        // resident in both after the fork.
        let ours = read_proc("/proc/self/smaps");
        let inherited = |text: &str| {
            record_at(text, busy).map(|record| {
                (
                    record.field("Size").map(str::to_owned),
                    record.field("Rss").map(str::to_owned),
                )
            })
        };
        r.check(
            "while an inherited mapping reads the same in the child's file",
            inherited(&text) == inherited(&ours) && inherited(&ours).is_some(),
        );
        unsafe { libc::kill(child, libc::SIGKILL) };
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
    }

    // A pid nothing answers to is the same answer `maps` gives: no such file.
    r.check(
        "a pid nothing answers to has no smaps file",
        std::fs::read_to_string("/proc/424242/smaps").is_err(),
    );

    r.finish();
}
