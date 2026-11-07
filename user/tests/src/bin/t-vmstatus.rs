//! t-vmstatus - `/proc/<pid>/status`'s four `Vm` lines are four different
//! facts, not one number printed four times.
//!
//! `VmSize` is the address space a process has mapped right now; `VmPeak` is the
//! largest it has ever been. `VmRSS` is what is resident right now; `VmHWM` is
//! the largest resident set it has ever held. `status_of` printed all four from
//! the resident frame count, so a 4 MiB mapping nobody had touched moved every
//! line by the same (zero) amount, and unmapping a touched one made the peak
//! "unhappen".
//!
//! Measured on Linux 7.2.14 before any claim was written
//! (`build/scratch/vmfields/u.c`, `docs/notes/` §6.109):
//!
//! ```text
//! baseline                     VmPeak=248    VmSize=248    VmHWM=88     VmRSS=88
//! mapped 4 MiB, untouched      VmPeak=4344   VmSize=4344   VmHWM=88     VmRSS=88
//! touched all 4 MiB            VmPeak=4344   VmSize=4344   VmHWM=4184   VmRSS=4184
//! unmapped it again            VmPeak=4344   VmSize=248    VmHWM=4112   VmRSS=88
//! ```
//!
//! The decisive rows are the first and the last: `VmPeak` keeps 4344 kB after
//! `VmSize` has gone back to 248 kB, and `VmHWM` keeps 4112 kB after `VmRSS` has
//! gone back to 88 kB. A high-water mark is a fact about the past, which no
//! current count can answer - so the kernel records it where the tables grow
//! rather than deriving it on a read.
//!
//! Every claim here is a movement (or a deliberate non-movement) on one mapping.
//! A relation like `VmRSS <= VmSize` passes for any same-source implementation,
//! which is why it is not a claim on its own: §6.103 paid for that lesson.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const MAPPED: usize = 4 << 20;
const MOVEMENT: i64 = (MAPPED as i64) / PAGE as i64;

/// The four lines, in kilobytes. The unit is part of the promise.
#[derive(Clone, Copy, Debug)]
struct Vm {
    peak: i64,
    size: i64,
    hwm: i64,
    rss: i64,
}

fn vm() -> Option<Vm> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut out = Vm {
        peak: -1,
        size: -1,
        hwm: -1,
        rss: -1,
    };
    let mut found = 0;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        // Only the four `Vm` lines are numbers - `Name:`, `State:` and the rest
        // are words, so parsing every `:` line and bailing on the first one that
        // is not a number would empty the whole reading.
        let target = match name {
            "VmPeak" => &mut out.peak,
            "VmSize" => &mut out.size,
            "VmHWM" => &mut out.hwm,
            "VmRSS" => &mut out.rss,
            _ => continue,
        };
        let Some(kb) = rest.trim().trim_end_matches(" kB").parse::<i64>().ok() else {
            continue;
        };
        *target = kb;
        found += 1;
    }
    (found == 4).then_some(out)
}

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
    (at != libc::MAP_FAILED).then_some(at as *mut u8)
}

fn main() {
    let mut r = Report::new();

    let Some(base) = vm() else {
        r.check("status prints the four Vm lines (they did not)", false);
        r.finish();
        return;
    };
    r.check(
        &format!("status prints the four Vm lines ({base:?})"),
        base.peak >= 0 && base.size >= 0 && base.hwm >= 0 && base.rss >= 0,
    );
    r.check(
        &format!(
            "and the resident set fits the address space ({} of {} kB)",
            base.rss, base.size
        ),
        base.rss <= base.size && base.hwm <= base.peak,
    );

    let Some(region) = map_anonymous(MAPPED) else {
        r.check("mapping 4 MiB anonymously succeeds", false);
        r.finish();
        return;
    };
    let Some(mapped) = vm() else {
        r.check("status reads again after the mapping", false);
        r.finish();
        return;
    };
    r.check(
        &format!(
            "mapping 4 MiB nobody has touched extends VmSize ({} -> {} kB)",
            base.size, mapped.size
        ),
        mapped.size >= base.size + MOVEMENT * (PAGE as i64 / 1024) - 32,
    );
    r.check(
        &format!(
            "and takes VmPeak with it ({} -> {} kB)",
            base.peak, mapped.peak
        ),
        mapped.peak >= mapped.size,
    );
    r.check(
        &format!("and leaves VmRSS alone ({} -> {} kB)", base.rss, mapped.rss),
        (mapped.rss - base.rss).abs() <= 32,
    );
    r.check(
        &format!(
            "and leaves VmHWM alone too ({} -> {} kB)",
            base.hwm, mapped.hwm
        ),
        (mapped.hwm - base.hwm).abs() <= 32,
    );

    // Touch every page: the address space must not move, the resident set must.
    unsafe {
        for page in 0..MAPPED / PAGE {
            core::ptr::write_volatile(region.add(page * PAGE), 1u8);
        }
    }
    let Some(touched) = vm() else {
        r.check("status reads after touching the mapping", false);
        r.finish();
        return;
    };
    r.check(
        &format!(
            "touching it raises VmRSS ({} -> {} kB)",
            mapped.rss, touched.rss
        ),
        touched.rss >= mapped.rss + MOVEMENT * (PAGE as i64 / 1024) - 32,
    );
    r.check(
        &format!(
            "and takes VmHWM with it ({} -> {} kB)",
            mapped.hwm, touched.hwm
        ),
        touched.hwm >= touched.rss,
    );
    r.check(
        &format!(
            "and leaves VmSize where it was ({} against {} kB)",
            touched.size, mapped.size
        ),
        (touched.size - mapped.size).abs() <= 32,
    );

    unsafe {
        libc::munmap(region as *mut libc::c_void, MAPPED);
    }
    let Some(after) = vm() else {
        r.check("status reads after unmapping", false);
        r.finish();
        return;
    };
    r.check(
        &format!(
            "unmapping gives VmSize back ({} against {} kB)",
            after.size, base.size
        ),
        (after.size - base.size).abs() <= 32,
    );
    r.check(
        &format!(
            "and gives VmRSS back ({} against {} kB)",
            after.rss, base.rss
        ),
        (after.rss - base.rss).abs() <= 32,
    );
    // The two separations are the claims a current count cannot pass: in the
    // kernel this pins, all four lines were the resident set, so after unmap
    // all four fell back together and neither peak existed.
    //
    // Linux's `VmHWM` is a *synced* record (`mm->hiwater_rss`), not
    // `max(VmRSS)` sampled at every instant: `build/scratch/vmfields/w.c`
    // measured it dip 4188 -> 4112 kB across a `munmap` while `ru_maxrss`
    // stayed at 4112 kB. So the claim is that the high water still records a
    // set much larger than the current one, not that it is byte-for-byte the
    // largest transient `VmRSS` reading - which is itself a counter estimate.
    r.check(
        &format!(
            "but VmPeak keeps the largest address space ({} against {} kB)",
            after.peak, after.size
        ),
        after.peak >= touched.peak - 64 && after.peak > after.size,
    );
    r.check(
        &format!(
            "and VmHWM still records the large resident set ({} against a baseline of {} kB)",
            after.hwm, base.hwm
        ),
        after.hwm >= base.hwm + (MOVEMENT * PAGE as i64 / 1024) / 2,
    );
    r.check(
        &format!(
            "so the two peaks are separated from the two current counts ({} > {} and {} > {} kB)",
            after.peak, after.size, after.hwm, after.rss
        ),
        after.peak > after.size && after.hwm > after.rss,
    );

    r.finish();
}
