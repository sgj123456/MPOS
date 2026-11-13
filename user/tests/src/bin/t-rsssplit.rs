//! `/proc/<pid>/status`'s resident-set split - `RssAnon`/`RssFile`/`RssShmem`
//! - and `/proc/<pid>/statm`'s third field, the same two numbers in pages.
//!
//! The rule this holds the kernel to is Linux's own, measured on 7.2.14
//! (`build/scratch/rsssplit/{q,s}.c`): the three `Rss*` lines sum to `VmRSS`
//! exactly; `statm`'s `shared` is `RssFile + RssShmem` in pages (and `resident`
//! is `VmRSS/4`); touching a private anonymous mapping moves `RssAnon` by
//! exactly its pages and touches neither of the other two; a
//! `MAP_SHARED | MAP_ANONYMOUS` mapping moves `RssShmem`.
//!
//! The old kernel printed the **whole** resident set as `RssAnon` and literal
//! zeros for the other two, and `0` for `statm`'s `shared`. Every claim below
//! is a movement or a relation read from one pair of reads, never an absolute
//! constant: opening a proc file allocates, so the first reading in a process
//! differs from the next by tens of pages even when nothing else happened
//! (`build/scratch/rsssplit/r.c`). A relation is the counterfactual - an
//! implementation that prints the resident set twice, or a zero, fails the
//! sum, the movement and the `statm` agreement at once.
//!
//! Named residual, deliberately **not** asserted: on Linux a private file page
//! moves from `RssFile` to `RssAnon` when the process copy-on-writes it. This
//! kernel has no page cache, so its split is a model over the describing
//! ranges rather than a per-page truth and such a page stays `RssFile`
//! (`docs/notes/` §6.147). `MPOS_ON_HOST=1` runs these same claims against
//! real Linux, where the model and the truth agree for these shapes.

use mpos_user_tests::Report;

/// `Key:\t N kB` line's `N`.
fn kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
        .and_then(|number| number.parse().ok())
}

fn status() -> String {
    std::fs::read_to_string("/proc/self/status").unwrap_or_default()
}

/// The four numbers a reader subtracts, in kB, and `statm`'s shared/resident
/// pair in pages, read at one instant.
fn read() -> Option<(u64, u64, u64, u64, u64, u64)> {
    let text = status();
    let rss = kb(&text, "VmRSS:")?;
    let anon = kb(&text, "RssAnon:")?;
    let file = kb(&text, "RssFile:")?;
    let shmem = kb(&text, "RssShmem:")?;
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut words = statm.split_whitespace();
    let _size: u64 = words.next()?.parse().ok()?;
    let resident: u64 = words.next()?.parse().ok()?;
    let shared: u64 = words.next()?.parse().ok()?;
    Some((rss, anon, file, shmem, resident, shared))
}

/// Touch every page of a fresh mapping so it is resident, then hand the
/// address back to the caller to unmap. `black_box` keeps the stores live.
unsafe fn touch(base: *mut libc::c_void, len: usize) {
    // SAFETY: `base..base+len` is the mapping the caller just made, writable.
    unsafe { std::ptr::write_bytes(base.cast::<u8>(), 1, len) };
    std::hint::black_box(base);
}

fn mmap(len: usize, flags: libc::c_int) -> *mut libc::c_void {
    // SAFETY: a fresh anonymous mapping, checked against MAP_FAILED below.
    unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    }
}

fn main() {
    let mut report = Report::new();
    const LEN: usize = 4 << 20;
    const PAGES: u64 = (LEN / 4096) as u64; // 1024

    let Some(first) = read() else {
        report.check("/proc/self/status and statm answer the split", false);
        report.finish();
    };
    report.check(
        "/proc/self/status and statm answer the split",
        read().is_some(),
    );
    // The one claim that is true of both the old and the new kernel is the
    // sum, so assert it on every reading but never alone.
    let (rss, anon, file, shmem, resident, shared) = first;
    report.check(
        &format!("the three Rss lines sum to VmRSS ({anon}+{file}+{shmem} vs {rss})"),
        anon + file + shmem == rss,
    );
    // The two files are two opens, and opening a proc file faults the reader's
    // own (anonymous) stack pages in between them, so their resident counts
    // can differ by a page or two even on a quiet machine. The bound still
    // catches a wrong unit or a borrowed `size`: those are off by 4x or more.
    // The exact `VmRSS` relation is pinned inside one status read by the sum
    // claim above, which is the one this round is about.
    report.check(
        &format!("statm resident is VmRSS in pages, but for the reader's own faults ({resident} vs {rss}/4)"),
        resident.abs_diff(rss / 4) <= 64,
    );
    report.check(
        &format!(
            "statm shared is RssFile+RssShmem in pages ({shared} vs {}/4)",
            file + shmem
        ),
        shared == (file + shmem) / 4,
    );
    // An untouched program has a mapped image, so `RssFile` is not zero on
    // either machine. The old kernel printed 0; this is the cheap red.
    report.check(
        &format!("the mapped image is counted as RssFile ({file} kB)"),
        file > 0,
    );

    // --- a private anonymous mapping moves only RssAnon -------------------
    let private = mmap(LEN, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
    if private == libc::MAP_FAILED {
        report.check("a 4 MiB private anonymous mapping is made", false);
        report.finish();
    }
    // SAFETY: a writable 4 MiB anonymous mapping this function just made.
    unsafe { touch(private, LEN) };
    let Some(second) = read() else {
        report.check("the split is readable after touching anon memory", false);
        report.finish();
    };
    let (r2, a2, f2, s2, _resident2, shared2) = second;
    report.check(
        &format!(
            "touching 4 MiB of private anon moves RssAnon by ~{PAGES} pages ({anon} -> {a2} kB)"
        ),
        a2 >= anon + (PAGES - 16) * 4 && a2 <= anon + (PAGES + 64) * 4,
    );
    report.check(
        &format!(
            "and leaves RssFile+RssShmem alone ({}+{} -> {}+{})",
            file, shmem, f2, s2
        ),
        f2 + s2 <= file + shmem + 64,
    );
    report.check(
        &format!("and VmRSS moves with it ({rss} -> {r2} kB)"),
        r2 >= rss + (PAGES - 16) * 4,
    );
    report.check(
        &format!("the three still sum to VmRSS ({a2}+{f2}+{s2} vs {r2})"),
        a2 + f2 + s2 == r2,
    );
    report.check(
        &format!(
            "statm shared still tracks the file/shmem half ({shared2} vs {}/4)",
            f2 + s2
        ),
        shared2 == (f2 + s2) / 4,
    );
    // The private mapping stays resident: the shared-anon claim below is that
    // *RssAnon does not move* while RssShmem grows, and unmapping here would
    // move it by the whole 4 MiB for a reason that has nothing to do with shmem.

    // --- a shared anonymous mapping moves RssShmem ------------------------
    let shared = mmap(LEN, libc::MAP_SHARED | libc::MAP_ANONYMOUS);
    if shared == libc::MAP_FAILED {
        report.check("a 4 MiB shared anonymous mapping is made", false);
        report.finish();
    }
    // SAFETY: a writable 4 MiB anonymous mapping this function just made.
    unsafe { touch(shared, LEN) };
    let Some(third) = read() else {
        report.check("the split is readable after touching shared memory", false);
        report.finish();
    };
    let (r3, a3, s3, shared3) = (third.0, third.1, third.3, third.5);
    report.check(
        &format!(
            "touching 4 MiB of shared anon moves RssShmem by ~{PAGES} pages ({shmem} -> {s3} kB)"
        ),
        s3 >= shmem + (PAGES - 16) * 4 && s3 <= shmem + (PAGES + 64) * 4,
    );
    report.check(
        &format!("and RssAnon stays where the private mapping left it ({a2} -> {a3} kB)"),
        a3 >= a2.saturating_sub(64) && a3 <= a2 + 256,
    );
    report.check(
        &format!(
            "the three still sum to VmRSS, and statm shared tracks them ({r3} kB, {shared3} pages)"
        ),
        {
            let text = status();
            let f3 = kb(&text, "RssFile:").unwrap_or(0);
            a3 + f3 + s3 == r3 && shared3 == (f3 + s3) / 4
        },
    );
    // SAFETY: both mappings above are still alive and are the only ones this
    // program made; the process exits immediately afterwards.
    unsafe {
        libc::munmap(private, LEN);
        libc::munmap(shared, LEN);
    }

    report.finish();
}
