//! `/proc/<pid>/numa_maps`: one line per mapping, and what each line says.
//!
//! The file `numactl`/`numastat` read to ask where a range's pages live. Its
//! line is *not* a `maps` line - the address is the start alone, with no `-end`
//! - and the words after the policy are counts: `anon=`, `dirty=`, `mapped=`,
//! `mapmax=`, `active=`, one `N<node>=` per node, and the frame size. Every one
//! of those was measured on a current host before this file existed
//! (`.workbuddy/probe_numamaps.c`, `probe_numamaps2.c`, Linux 7.2.11, one node),
//! and the measurements worth carrying are:
//!
//! * **the policy word is `default`** for every mapping here, because
//!   `set_mempolicy`/`mbind` are not served and `default` is what a task has
//!   when it never asked for one;
//! * **a range with no resident page prints the policy and no number at all** -
//!   not `N0=0`, and not a count of zero. Two shapes do that on Linux: a mapping
//!   nothing has touched, and a *read-only anonymous* read, whose fault Linux
//!   satisfied with the shared zero page and which is therefore not a page it
//!   counts (measured: a two-page read of a private anonymous range came back
//!   with no counts at all);
//! * **the second half of that is the one place this kernel's answer differs**,
//!   and it is named rather than claimed: a read fault here materialises a real
//!   frame, where Linux hands out the shared zero page, so a read-only
//!   anonymous range reports `anon=1 dirty=1` here and nothing there. The claim
//!   below is therefore made about a *written* range, which both count;
//! * **`dirty=` and not `mapped=` is the honest answer for an object's pages.**
//!   Linux prints `dirty=` for a range whose pages have been written and
//!   `mapped=` for one whose pages have not (measured with the shape that tells
//!   them apart: a mapping that is *writable but never written* prints
//!   `mapped=4`, and the same mapping written prints `dirty=4`). This kernel
//!   cannot tell a written page from a read one - the same absence `smaps`' four
//!   shared/private buckets name - so it answers with the one rule it can state:
//!   every resident page is this mapping's own and dirty;
//! * **`active=0`**, and the field is there rather than absent: the number is
//!   the pages on Linux's active LRU list and this kernel keeps no LRU, so zero
//!   is what it can state, and Linux prints the field for every range whose
//!   count differs from the total - which is every range holding a page;
//! * **`mapped=` and `mapmax=` are not printed at all.** The first is Linux's
//!   answer where its own counts do not add up to the total, and the second is
//!   the highest mapcount of any page in the range - and there is no mapcount
//!   here, which is the same fact `smaps`' `Pss` names.
//!
//! A mapping's name is `file=<path>` when a path is known and `heap` for the
//! heap; `[vdso]` and `[vvar]` carry no name on Linux either, measured. A space
//! in a path is written `\040` and a tab `\011` - `seq_file_path`'s octal escape,
//! measured with a path carrying both - and that escaping is pinned by the
//! kernel's own unit test rather than here, because a claim about it needs a
//! filesystem that can hold such a name.
//!
//! # Why the fixtures are shaped the way they are
//!
//! Every mapping is bounded by `PROT_NONE` pages, because two adjacent ranges
//! with the same flags are one VMA to both kernels - the first draft of
//! `t-smaps` mapped two ranges in a row and got one five-page record back, so
//! "the count is the number the test wrote" became a claim about a number the
//! test had not chosen.
//!
//! The `memfd` fixture is an *object's* pages rather than a file's, because a
//! mapping of a file is a copy of it in this kernel (the bytes are read in at
//! `mmap` time and nothing tracks the file afterwards), which is the divergence
//! `smaps` already carries and not this file's to pin.

use mpos_user_tests::Report;

const PAGE: usize = 4096;

/// Read a whole `/proc` file into a buffer the caller keeps.
///
/// The buffer is the caller's and is *reused*, because a read that allocated
/// could add a mapping of its own - and the claim below is that two files list
/// the same VMAs, which a buffer appearing between the two reads would break
/// (that is what the first draft did, and why the warm-up below exists).
fn read_proc(path: &str, into: &mut String) {
    into.clear();
    if let Ok(mut file) = std::fs::File::open(path) {
        let _ = std::io::Read::read_to_string(&mut file, into);
    }
}

/// A fixture bounded by `PROT_NONE` pages: `pages` of `prot` in the middle of a
/// span nobody can merge with, and the address of the middle's first page.
fn bounded(pages: usize, prot: libc::c_int) -> u64 {
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
        return 0;
    }
    let middle = unsafe { (base as *mut u8).add(PAGE) };
    let rc = unsafe { libc::mprotect(middle as *mut libc::c_void, pages * PAGE, prot) };
    if rc != 0 {
        return 0;
    }
    middle as u64
}

/// The `numa_maps` line beginning at `addr`: its words, with the address and
/// the policy taken off.
fn words_at(text: &str, addr: u64) -> Option<Vec<String>> {
    let want = format!("{addr:08x}");
    text.lines()
        .find(|line| line.split_whitespace().next() == Some(want.as_str()))
        .map(|line| line.split_whitespace().skip(1).map(str::to_owned).collect())
}

/// Every `key=value` word of one line, so a claim can ask about a count without
/// knowing what order the words come in.
fn counts(words: &[String]) -> Vec<(String, String)> {
    words
        .iter()
        .filter_map(|word| {
            word.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

fn has(words: &[String], key: &str) -> bool {
    counts(words).iter().any(|(k, _)| k == key)
}

/// The value of one `key=value` word. Matched on the `=` rather than on a
/// split, so that `anon=` is not confused with a name that merely begins with
/// those letters.
fn value<'a>(words: &'a [String], key: &str) -> Option<&'a str> {
    let body = key.len() + 1;
    words
        .iter()
        .find(|word| {
            word.len() > body && word.starts_with(key) && word.as_bytes()[key.len()] == b'='
        })
        .map(|word| &word[body..])
}

fn main() {
    let mut r = Report::new();

    // The fixtures: four pages with three written, and four pages nothing has
    // touched. Both are anonymous, which is the shape both kernels count the
    // same way.
    let busy = bounded(4, libc::PROT_READ | libc::PROT_WRITE);
    r.check("the written fixture was made", busy != 0);
    for page in 0..3 {
        if busy != 0 {
            unsafe { ((busy as *mut u8).add(page * PAGE)).write_volatile(1) };
        }
    }
    let quiet = bounded(2, libc::PROT_READ | libc::PROT_WRITE);
    r.check("the untouched fixture was made", quiet != 0);

    // Both buffers are sized once and reused, and both files are read once
    // before the pair the claims read: the first pass is what makes the second
    // one's two reads see the same address space.
    let mut text = String::with_capacity(64 * 1024);
    let mut maps = String::with_capacity(64 * 1024);
    read_proc("/proc/self/numa_maps", &mut text);
    read_proc("/proc/self/maps", &mut maps);
    read_proc("/proc/self/numa_maps", &mut text);
    read_proc("/proc/self/maps", &mut maps);
    r.check(
        "reading /proc/self/numa_maps answered something",
        !text.is_empty(),
    );

    // ---- the shape ------------------------------------------------------
    //
    // Every line names a start and a policy, and this kernel's policy is
    // always `default`. The address carries no `-end`, which is the difference
    // from a `maps` line and the thing a parser splitting on whitespace sees
    // first.
    let lines: Vec<&str> = text.lines().collect();
    r.check("the file has lines", lines.len() > 3);
    r.check(
        "every line names a start and the policy, and the policy is default",
        lines.len() > 3
            && lines.iter().all(|line| {
                let mut words = line.split_whitespace();
                let start = words.next().unwrap_or("");
                !start.is_empty()
                    && u64::from_str_radix(start, 16).is_ok()
                    && !start.contains('-')
                    && words.next() == Some("default")
            }),
    );

    // One line per mapping - the same VMAs `maps` lists, in the same order.
    // The gate VMA is the exception and it is Linux's own: `[vsyscall]` has a
    // `maps` line and no `numa_maps` one (measured), because it is not a VMA
    // of the address space at all.
    let starts: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    let maps_starts: Vec<&str> = maps
        .lines()
        .filter(|line| !line.ends_with("[vsyscall]"))
        .filter_map(|line| line.split('-').next())
        .collect();
    r.check(
        "every mapping's line is here: the starts are maps' starts, in order",
        starts.len() > 3 && starts == maps_starts,
    );

    // ---- a range with no resident page ----------------------------------
    //
    // The policy and nothing else - not a count of zero, which is the whole
    // difference between "mapped" and "resident".
    r.check(
        "an untouched range says the policy and no number at all",
        words_at(&text, quiet).as_deref() == Some(&[String::from("default")][..]),
    );

    // ---- a written anonymous range --------------------------------------
    //
    // The three counts are the same three pages: anonymous, dirty (the rule
    // `smaps` applies for the same reason - no dirty bit, no page cache), and
    // on the one node there is. `active=0` is this kernel's answer with no LRU
    // and Linux's own for a range nothing has promoted.
    if let Some(words) = words_at(&text, busy) {
        r.check(
            "a written anonymous range counts its resident pages as anon",
            value(&words, "anon") == Some("3"),
        );
        r.check(
            "and the same pages as dirty: every resident page here is written",
            value(&words, "dirty") == Some("3"),
        );
        r.check(
            "and the same pages on the one node there is",
            value(&words, "N0") == Some("3"),
        );
        r.check(
            "active is zero: this kernel keeps no LRU to be active on",
            value(&words, "active") == Some("0"),
        );
        r.check(
            "the frame size is 4 kB, which is what both machines' frames are",
            value(&words, "kernelpagesize_kB") == Some("4"),
        );
        // The two fields this kernel cannot state are absent rather than zero:
        // `mapped` is Linux's answer where its own counts do not add up, and
        // `mapmax` is a mapcount this kernel does not keep.
        r.check(
            "and neither mapped nor mapmax is printed, because both are a mapcount",
            !has(&words, "mapped") && !has(&words, "mapmax"),
        );
        // The count is the residency the other file reports - the same walk of
        // the same page tables, read through two spellings.
        let mut smaps = String::with_capacity(64 * 1024);
        read_proc("/proc/self/smaps", &mut smaps);
        read_proc("/proc/self/smaps", &mut smaps);
        let rss = smaps
            .lines()
            .filter(|line| line.starts_with(&format!("{busy:08x}-")))
            .count();
        r.check(
            "and it is the same residency /proc/self/smaps reports",
            rss == 1
                && smaps
                    .lines()
                    .skip_while(|line| !line.starts_with(&format!("{busy:08x}-")))
                    .any(|line| line.starts_with("Rss:") && line.contains("12 kB")),
        );
    } else {
        r.check("the written range's line is in the file", false);
    }

    // ---- an object's pages ----------------------------------------------
    //
    // A `memfd`'s page belongs to the object and not to the address space, so
    // it is not anonymous - Linux's own answer for the same shape (measured:
    // `dirty=1` and no `anon=`), and the reason the distinction comes from the
    // mapping's kind rather than from a count.
    let mfd = unsafe { libc::memfd_create(c"t-numamaps".as_ptr(), 0) };
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
            read_proc("/proc/self/numa_maps", &mut text);
            match words_at(&text, shared as u64) {
                Some(words) => {
                    r.check(
                        "an object's page is resident and dirty",
                        value(&words, "dirty") == Some("1") && value(&words, "N0") == Some("1"),
                    );
                    r.check(
                        "and it is not anonymous: the page is the object's",
                        !has(&words, "anon"),
                    );
                }
                None => r.check("the object's line is in the file", false),
            }
        }
    }

    // ---- the heap --------------------------------------------------------
    //
    // A name is a word of its own rather than a path: the heap prints `heap`
    // where a file prints `file=`. This kernel names the heap and not the
    // stack, so it is the one naming claim here.
    r.check(
        "the heap's line names it with the word heap",
        text.lines()
            .any(|line| line.split_whitespace().nth(2) == Some("heap")),
    );

    // A pid nothing answers to is the same answer every per-pid file gives.
    r.check(
        "a pid nothing answers to has no numa_maps file",
        std::fs::read_to_string("/proc/424242/numa_maps").is_err(),
    );

    r.finish();
}
