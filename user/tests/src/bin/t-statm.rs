//! t-statm - `/proc/<pid>/statm` and `stat`'s `vsize` count the address space a
//! process has **mapped**, not the pages it has touched.
//!
//! The two numbers answer two questions a memory monitor asks separately, and a
//! program that conflates them is wrong in both directions at once: a process
//! that maps a gigabyte and touches a page is reported as tiny (so a leak
//! detector sees nothing), and one that touches what it mapped is reported as
//! huge (so an over-commit check refuses a mapping that costs nothing yet).
//! `/proc/<pid>/statm`'s seven fields are, in order, total, resident, shared,
//! text, library, data and dirty, in pages. This kernel printed the *resident*
//! count in the first field as well, and `stat`'s field 23 (`vsize`) as the same
//! resident count times the page size.
//!
//! Measured on a current host before any of these claims was written
//! (`build/scratch/vsize/p.c`, Linux 7.2.14, `§6.103`): the baseline was
//! `vsize=258048` with `statm total=63 resident=24`, and `vsize` was exactly
//! `total * 4096`. Mapping 4 MiB anonymously and touching **none** of it moved
//! `total` to `1087` (and `vsize` with it) while `resident` stayed `24`;
//! touching it moved `resident` to `1048` and left `total` at `1087`. The same
//! probe on this kernel answered `total`/`vsize` with the resident count, so an
//! untouched mapping did not exist to either file.
//!
//! The claims are therefore a movement and a non-movement on the same mapping:
//! mapping must raise `total` without raising `resident`, touching must raise
//! `resident` without raising `total`, and unmapping must give the space back.
//! A kernel that reports only one of the two numbers fails one half or the
//! other whichever one it picks.

use mpos_user_tests::Report;

/// The page size every claim here is in. Both machines this runs on are 4 KiB,
/// and `statm` is a page count, so the number is the unit rather than a setting.
const PAGE: i64 = 4096;

/// The seven fields of `/proc/self/statm`, in Linux's order.
#[derive(Clone, Copy, Debug)]
struct Statm {
    total: i64,
    resident: i64,
    shared: i64,
    text: i64,
    library: i64,
    data: i64,
    dirty: i64,
}

fn read_statm() -> Option<Statm> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() != 7 {
        return None;
    }
    let field = |index: usize| -> Option<i64> { fields.get(index)?.parse().ok() };
    Some(Statm {
        total: field(0)?,
        resident: field(1)?,
        shared: field(2)?,
        text: field(3)?,
        library: field(4)?,
        data: field(5)?,
        dirty: field(6)?,
    })
}

/// `stat`'s field 23, `vsize`, in bytes.
///
/// Field *N* is token *N-3* after the last `)`: `comm` is parenthesised and may
/// hold spaces of its own, so the split starts after the closing parenthesis.
fn read_vsize() -> Option<i64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    let tail = &text[text.rfind(')')? + 1..];
    tail.split_whitespace().nth(23 - 3)?.parse().ok()
}

/// Map `len` bytes privately and anonymously: an address-space claim with
/// nothing behind it until a page is written.
fn map_anonymous(len: usize) -> Option<*mut u8> {
    let at = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        None
    } else {
        Some(at as *mut u8)
    }
}

/// One reading of both files, bracketed so the two can be compared without
/// demanding they were taken in the same instant: `statm`, then `vsize`, then
/// `statm` again. Anything the process maps in between is counted by the second
/// reading, and a consistent pair lies between the two totals.
struct Reading {
    statm: Statm,
    after: Statm,
    vsize: i64,
}

impl Reading {
    fn take() -> Option<Reading> {
        let statm = read_statm()?;
        let vsize = read_vsize()?;
        let after = read_statm()?;
        Some(Reading {
            statm,
            after,
            vsize,
        })
    }

    /// The two files report one number in two units, and do so consistently
    /// whenever `vsize` lies between the page counts taken around it.
    fn agrees(&self) -> bool {
        self.vsize >= self.statm.total * PAGE && self.vsize <= self.after.total * PAGE
    }
}

fn claim_the_two_numbers_are_two_questions(r: &mut Report) {
    let Some(baseline) = Reading::take() else {
        r.check("statm and stat both read (they did not)", false);
        return;
    };
    let statm = baseline.statm;
    r.check(
        &format!(
            "statm has seven page counts ({:?})",
            (
                statm.total,
                statm.resident,
                statm.shared,
                statm.text,
                statm.library,
                statm.data,
                statm.dirty
            )
        ),
        statm.total > 0 && statm.resident > 0,
    );
    r.check(
        &format!(
            "stat's vsize is statm's total in bytes ({} against {} pages of {PAGE})",
            baseline.vsize, statm.total
        ),
        baseline.agrees(),
    );
    r.check(
        &format!(
            "the resident set is never larger than the address space ({} of {} pages)",
            statm.resident, statm.total
        ),
        statm.resident <= statm.total,
    );

    // 4 MiB: 1024 pages, large enough that a tick of accounting cannot hide the
    // movement and small enough that a machine short of memory still grants it.
    const MAPPED: usize = 4 << 20;
    const PAGES: i64 = (4 << 20) / PAGE;
    let Some(region) = map_anonymous(MAPPED) else {
        r.check("mapping 4 MiB anonymously succeeds", false);
        return;
    };
    let Some(mapped) = Reading::take() else {
        r.check("statm reads again after the mapping", false);
        return;
    };
    r.check(
        &format!(
            "mapping 4 MiB nobody has touched extends the address space ({} -> {} pages)",
            statm.total, mapped.after.total
        ),
        mapped.after.total >= statm.total + PAGES - 8,
    );
    r.check(
        &format!(
            "and adds nothing to the resident set ({} -> {} pages)",
            statm.resident, mapped.statm.resident
        ),
        mapped.statm.resident <= statm.resident + 8,
    );
    r.check(
        &format!(
            "and stat's vsize follows it ({} bytes against {} pages)",
            mapped.vsize, mapped.statm.total
        ),
        mapped.agrees(),
    );

    // Touch every page: the address space must not move, the resident set must.
    let mapped_pages = MAPPED / PAGE as usize;
    unsafe {
        for page in 0..mapped_pages {
            core::ptr::write_volatile(region.add(page * PAGE as usize), 1u8);
        }
    }
    let Some(touched) = Reading::take() else {
        r.check("statm reads after touching the mapping", false);
        return;
    };
    r.check(
        &format!(
            "touching it makes it resident ({} of the {PAGES} pages mapped)",
            touched.statm.resident - mapped.statm.resident
        ),
        touched.statm.resident >= mapped.statm.resident + PAGES - 8,
    );
    r.check(
        &format!(
            "and leaves the address space where it was ({} against {} pages)",
            touched.statm.total, mapped.statm.total
        ),
        (touched.statm.total - mapped.statm.total).abs() <= 8,
    );
    r.check(
        &format!(
            "and vsize agrees with statm's total again ({} bytes against {} pages)",
            touched.vsize, touched.statm.total
        ),
        touched.agrees(),
    );

    unsafe {
        libc::munmap(region as *mut libc::c_void, MAPPED);
    }
    let Some(after) = Reading::take() else {
        r.check("statm reads after unmapping", false);
        return;
    };
    r.check(
        &format!(
            "unmapping gives the address space back ({} against {} pages)",
            after.statm.total, statm.total
        ),
        (after.statm.total - statm.total).abs() <= 8,
    );
    r.check(
        &format!(
            "and vsize comes back with it ({} bytes against {} pages)",
            after.vsize, after.statm.total
        ),
        after.agrees(),
    );
}

fn main() {
    let mut r = Report::new();
    claim_the_two_numbers_are_two_questions(&mut r);
    r.finish();
}
