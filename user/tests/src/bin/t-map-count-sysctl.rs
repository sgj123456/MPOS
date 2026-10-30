//! `/proc/sys/vm/max_map_count` is the ceiling `mmap` asks, not a second
//! number: a write to the file moves the gate the next `mmap` meets.
//!
//! Measured on Linux 7.2.11 first (`build/scratch/sysctl/p1.c`, `p2.c`, as root
//! with the host's own sysctl restored afterwards, and as uid 1000 for the
//! refusal):
//!
//! - the file reads the number and a trailing newline (`1048576\n` there);
//! - an unprivileged open for write is **`EACCES` (13)** - the mode is `0644`
//!   and the owner is root;
//! - writing `1000` is accepted, the read-back is `1000`, and the very next
//!   `mmap` past 1 000 descriptions answers **`ENOMEM` (12)**;
//! - writing `10` *below* the count the process already holds is accepted too,
//!   the mappings already made keep working, and only the next `mmap` is
//!   refused - the number is checked at placement, not at write time;
//! - `abc`, `-1` and `2147483648` are each **`EINVAL` (22)** and leave the
//!   value alone; `0` is accepted (and then every `mmap` is refused); a value
//!   without a trailing newline is accepted, and extra newlines are ignored.
//!
//! This pin walks the first three of those and the restore, because they are
//! the claims a kernel can get wrong silently: a read-only file would pass a
//! shape check, and a write that nothing reads would pass a read-back check.
use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PATH: &str = "/proc/sys/vm/max_map_count";
/// Linux's default, and the number this kernel ships before anyone writes.
const LINUX_DEFAULT: usize = 65_530;
/// The value this pin lowers the ceiling to: small enough to walk in an
/// instant, big enough that the process's own descriptions fit under it.
const LOWERED: usize = 1_024;

fn read_ceiling() -> Option<usize> {
    std::fs::read_to_string(PATH).ok()?.trim().parse().ok()
}

fn write_ceiling(text: &str) -> Result<(), i32> {
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).open(PATH) {
        Ok(mut file) => match file.write_all(format!("{text}\n").as_bytes()) {
            Ok(()) => Ok(()),
            Err(e) => Err(e.raw_os_error().unwrap_or(0)),
        },
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

fn map_page(prot: i32) -> Result<*mut libc::c_void, i32> {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(p)
    }
}

/// The number of descriptions the process holds, as `/proc/self/maps` sees
/// them (two rows are not VMAs - the gate's own and the kernel's vdso-stand-in
/// rows - so the number is a band, not an identity).
fn map_rows() -> Option<usize> {
    Some(
        std::fs::read_to_string("/proc/self/maps")
            .ok()?
            .lines()
            .count(),
    )
}

/// Map alternating permissions so neighbours cannot merge, until refused.
///
/// The last `recent.len()` mappings are handed back to the caller so they can
/// be given up before anything is printed: at the ceiling the process's own
/// heap cannot grow - its next `libc` mapping is refused like any other - and
/// the first `format!` after the loop would abort (measured: `memory
/// allocation of 5 bytes failed`, exit 134).
fn map_until_refused(limit: usize, recent: &mut [*mut libc::c_void]) -> (usize, i32) {
    let mut mapped = 0usize;
    while mapped < limit {
        let prot = if mapped % 2 == 0 {
            libc::PROT_NONE
        } else {
            libc::PROT_READ
        };
        match map_page(prot) {
            Ok(p) => {
                recent[mapped % recent.len()] = p;
                mapped += 1;
            }
            Err(e) => return (mapped, e),
        }
    }
    (mapped, 0)
}

fn main() {
    let mut r = Report::new();

    if mpos_user_tests::host_run() {
        // On the development host the sysctl is tuned (1 048 576), and this
        // pin would write the *host's* global policy if it ran: report its
        // number as context and leave it alone.
        let host = read_ceiling();
        r.check(
            &format!("the host's own max_map_count is {host:?}; this pin writes the guest's, not another kernel's"),
            true,
        );
        r.finish();
    }

    // The number the guest starts with is Linux's default; a host run's own
    // sysctl is that host's policy, which is why the claim names what *this*
    // kernel ships.
    let original = read_ceiling();
    r.check(
        &format!("the file reads Linux's default ({LINUX_DEFAULT}) before anyone writes",),
        original == Some(LINUX_DEFAULT),
    );

    // A mapping made under the default has to outlive a write that moves the
    // ceiling below it: the number is checked at placement, so nothing that is
    // already mapped is touched by a write.
    let early = map_page(libc::PROT_READ | libc::PROT_WRITE).ok();

    // The refusal an ordinary user meets. On Linux the `0644` word is policed
    // and the *open* itself is `EACCES`; this kernel's procfs keeps no mode of
    // its own, so its open succeeds and the write is the door that answers -
    // `EACCES` either way, which is the errno a caller branches on. (Item 137
    // names that difference; the pin claims the behaviour, not the door.)
    let mut child_answer = -1i32;
    match unsafe { libc::fork() } {
        -1 => {}
        0 => {
            unsafe { libc::setuid(65_534) };
            // A no-op value: if the write *does* land, the ceiling stays where
            // it was and the claim still reads as "the hole was open", never
            // as a machine left at a ceiling of 1.
            let text = format!("{}\n", read_ceiling().unwrap_or(0));
            let refused = match std::fs::OpenOptions::new().write(true).open(PATH) {
                Ok(mut file) => {
                    use std::io::Write as _;
                    match file.write_all(text.as_bytes()) {
                        Ok(()) => None,
                        Err(e) => Some(e.raw_os_error().unwrap_or(-1)),
                    }
                }
                Err(e) => Some(e.raw_os_error().unwrap_or(-1)),
            };
            unsafe {
                libc::_exit(match refused {
                    None => 100, // nobody was refused: the hole is open
                    Some(e) if e == libc::EACCES => 0,
                    Some(e) => e,
                })
            };
        }
        pid => {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            if libc::WIFEXITED(status) {
                child_answer = libc::WEXITSTATUS(status);
            }
        }
    }
    r.check(
        &format!("an unprivileged writer cannot move the number (answered {child_answer})"),
        child_answer == 0 && read_ceiling() == original,
    );

    r.check(
        &format!("root may lower the ceiling to {LOWERED}"),
        write_ceiling(&LOWERED.to_string()).is_ok() && read_ceiling() == Some(LOWERED),
    );

    // The point of the file: the number it holds is the number the next
    // `mmap` is answered against, not a copy kept somewhere else. Nothing is
    // formatted while the process sits at the lowered ceiling - its own heap's
    // next growth is an `mmap` like any other, and at 1 024 of 1 024 the
    // allocator's request is refused (measured: `memory allocation of 32 bytes
    // failed`, exit 134). The ceiling is put back first, then the evidence is
    // written out.
    let mut recent = [std::ptr::null_mut::<libc::c_void>(); 64];
    let (mapped, refusal) = map_until_refused(LOWERED + 512, &mut recent);
    // Give the allocator its headroom back before a single word is formatted.
    for p in recent.iter() {
        if !p.is_null() {
            unsafe { libc::munmap(*p, PAGE) };
        }
    }
    let restore = original.unwrap_or(LINUX_DEFAULT);
    let restored = write_ceiling(&restore.to_string());
    let rows = map_rows().unwrap_or(0);
    r.check(
        &format!(
            "the lowered ceiling is the one the next mmap is answered against \
             (refused with {refusal}, {mapped} mapped, {rows} map rows now)"
        ),
        refusal == libc::ENOMEM && mapped + 32 >= LOWERED && mapped <= LOWERED,
    );

    // An invalid word changes nothing - each answer is `EINVAL`, and the value
    // the last accepted write left is still there.
    let before_bad = read_ceiling();
    let bad = ["abc", "-1", "2147483648"];
    let mut refused_bad = 0usize;
    for text in bad {
        if write_ceiling(text) == Err(libc::EINVAL) {
            refused_bad += 1;
        }
    }
    r.check(
        &format!(
            "a word that is not a number this knob takes is EINVAL and moves nothing \
             ({refused_bad} of {} refused; {before_bad:?} -> {:?})",
            bad.len(),
            read_ceiling()
        ),
        refused_bad == bad.len() && read_ceiling() == before_bad,
    );

    // The number is checked when a mapping is placed, so the page mapped
    // before the ceiling was lowered still reads and writes.
    let alive = match early {
        Some(p) => {
            unsafe { std::ptr::write_volatile(p as *mut u8, 0x5a) };
            unsafe { std::ptr::read_volatile(p as *const u8) == 0x5a }
        }
        None => false,
    };
    r.check(
        "a mapping made before the ceiling moved still works after it",
        alive,
    );

    r.check(
        &format!("root may restore the ceiling to {restore}"),
        restored.is_ok() && read_ceiling() == Some(restore),
    );

    // Restored, so the mappings a lowered ceiling refused are accepted again.
    let mut spare = [std::ptr::null_mut::<libc::c_void>(); 64];
    let (again, _) = map_until_refused(64, &mut spare);
    r.check(
        &format!("the restored ceiling accepts mappings again ({again} mapped)"),
        again == 64,
    );

    r.finish();
}
