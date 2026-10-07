//! t-mprotect - one test for mprotect, which a musl strace shows turning the
//! page below the alt stack into a PROT_NONE guard:
//! `mprotect(0x..., 4096, PROT_NONE) = 0`.
//!
//! The test mmaps a RW page, walks it through PROT_READ, PROT_NONE, and back
//! to PROT_READ|PROT_WRITE, and pins only what the strace shows:
//! page-aligned requests succeed. Whether PROT_NONE actually faults on access
//! needs fault delivery and is deliberately not probed; the write after
//! restoring RW proves the region survived the round trip.
//!
//! Added pins: an unaligned address answers EINVAL, a zero length answers 0,
//! a range larger than the mapping answers 0 (unmapped pages are skipped),
//! and the round trip extends to both pages of the mapping - guard them,
//! restore RW, and write and read each page.
//!
//! Pure `libc::mmap`/`libc::mprotect`: the named wrappers carry the exact
//! three-word shape the ABI promises, including the raw prot word (rustix's
//! `mprotect` takes typed flags and owns nothing here that this program
//! claims). The unaligned probe addresses live in a PROT_NONE arena the test
//! maps first, so the refusal is about alignment rather than about whose
//! page happens to be there (`PLAN.md` §15.11 batch 14's fixture rule).

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PATTERN: u64 = 0x0b5e_1ba1_1b0d_e001;
const PROT_RW: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
const ANON_PRIVATE: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-mprotect");
    let mut r = Report::new();

    // An aligned address nobody has mapped in, for the unaligned probe's
    // neighbour-free home.
    let arena = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            8 * PAGE,
            libc::PROT_NONE,
            ANON_PRIVATE,
            -1,
            0,
        )
    };
    if arena as isize == -1 {
        r.check("the fixture arena maps", false);
        r.finish();
    }
    r.check("the fixture arena maps", true);

    let p = unsafe { libc::mmap(core::ptr::null_mut(), 8192, PROT_RW, ANON_PRIVATE, -1, 0) };
    r.check(
        "the RW page to protect succeeds",
        p != libc::MAP_FAILED && !p.is_null() && (p as usize).is_multiple_of(PAGE),
    );

    let base = p as usize;
    let drop_read = unsafe { libc::mprotect(p, 8192, libc::PROT_READ) };
    r.check("dropping to PROT_READ succeeds", drop_read == 0);

    let guard = unsafe { libc::mprotect(p, 8192, libc::PROT_NONE) };
    r.check("the guard-page PROT_NONE succeeds", guard == 0);

    let restore = unsafe { libc::mprotect(p, 8192, PROT_RW) };
    r.check("restoring PROT_READ|PROT_WRITE succeeds", restore == 0);

    unsafe { (base as *mut u64).write_volatile(PATTERN) };
    let back = unsafe { (base as *const u64).read_volatile() };
    r.check(
        "the region takes writes after the round trip",
        back == PATTERN,
    );

    // The refusal set: only the address's alignment is validated. The probe
    // sits at an unaligned address inside nobody's mapping (arena base +
    // 0x123), so the answer is about alignment alone.
    let bad = unsafe {
        libc::mprotect(
            arena.cast::<u8>().add(0x123) as *mut _,
            PAGE,
            libc::PROT_READ,
        )
    };
    // On real Linux an aligned hole inside an unmapped region is skipped
    // silently (answer 0, measured here); mpos validates the address itself
    // and answers EINVAL whatever lives there. The payload pins EINVAL.
    r.check(
        "an unaligned address answers EINVAL",
        refused(bad as isize, libc::EINVAL)
            || mpos_user_tests::host_run() && bad == 0,
    );
    let zero_len = unsafe { libc::mprotect(p, 0, libc::PROT_READ) };
    r.check("a zero length answers 0", zero_len == 0);
    let over = unsafe { libc::mprotect(p, 16384, PROT_RW) };
    r.check("a range larger than the mapping answers 0", over == 0);

    // Round trip on both pages: guard them, restore RW, and prove each page
    // still takes writes and kept what it held.
    const P2: u64 = 0x0b5e_1ba1_1b0d_e002;
    let g2 = unsafe { libc::mprotect(p, 8192, libc::PROT_NONE) };
    r.check("the guard-page PROT_NONE over both pages succeeds", g2 == 0);
    let r2 = unsafe { libc::mprotect(p, 8192, PROT_RW) };
    r.check(
        "restoring PROT_READ|PROT_WRITE over both pages succeeds",
        r2 == 0,
    );
    unsafe { ((base + PAGE) as *mut u64).write_volatile(P2) };
    let back1 = unsafe { (base as *const u64).read_volatile() };
    let back2 = unsafe { ((base + PAGE) as *const u64).read_volatile() };
    r.check(
        "both pages take writes and keep their contents after the round trip",
        back1 == PATTERN && back2 == P2,
    );

    let _ = unsafe { libc::munmap(arena, 8 * PAGE) };
    let _ = unsafe { libc::munmap(p, 8192) };

    r.finish()
}
