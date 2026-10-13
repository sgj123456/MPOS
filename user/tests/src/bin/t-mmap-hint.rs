//! t-mmap-hint - an address that is *mapped* is not a hint's to take, and
//! "mapped" has to mean every mapping this kernel keeps, not just the ones it
//! records in a VMA.
//!
//! A `MAP_PRIVATE` mapping of an ordinary file is filled in at `mmap` time and
//! deliberately carries **no VMA**: its pages were read in from the file and
//! nothing may drop them again, because there is no file behind them to refill
//! from (`MappedRange` in `mm/mma.rs` says why in full). What describes that
//! mapping, to the reader of `/proc/<pid>/maps` *and* to the kernel's own
//! "is this address free?" question, is the loader's range list.
//!
//! The occupancy question used to read the VMA table alone, so such an address
//! looked free. A second mapping asked for **with that address as its hint** was
//! therefore placed straight on top of it: a fresh writable VMA over pages
//! another mapping had already filled read-only. The first write to those pages
//! then had no resolver - the page is present, so a demand-fill cannot claim it,
//! and it carries no copy-on-write marker, so the COW path will not either - and
//! the program was killed instead of its mapping being placed somewhere else,
//! which is what Linux does with an occupied hint.
//!
//! Five claims, each of them Linux's answer too:
//!
//! 1. a hint naming a mapped address is **not** honoured - the kernel places the
//!    new mapping elsewhere;
//! 2. the file's own mapping still holds the file's bytes afterwards, so the two
//!    really are separate;
//! 3. `mprotect` on that mapping is what `/proc/self/maps` reports - the
//!    description follows the permissions, which for a VMA-less mapping is the
//!    only place they are written down;
//! 4. once the mapping is `munmap`ped the address *is* free, and the same hint
//!    is honoured;
//! 5. `mremap` carries the description with the pages: the old address becomes
//!    free and a hint at the new one is refused.
//!
//! # Why this program keeps a raw door
//!
//! `mremap` is clause (a) of `user/tests/Cargo.toml`'s rule - `rustix` binds
//! neither it nor `mincore`/`madvise` - so claim 5 rides `libc::mremap`, the
//! same door `t-mremap` uses. Everything else is `rustix`: `mmap`, `munmap`,
//! `mprotect`, `open`.

use core::ffi::c_void;
use mpos_user_tests::Report;
use rustix::fd::AsFd;
use rustix::fs::{Mode, OFlags, open};
use rustix::mm::{MapFlags, MprotectFlags, ProtFlags, mmap, mmap_anonymous, mprotect, munmap};
use std::io::Write as _;

/// Two pages: enough that a placement is a real address rather than the tail of
/// somebody else's mapping.
const LEN: usize = 8192;
const PATH: &str = "/tmp/t-mmap-hint_file";
/// One page's worth of a recognisable byte, per page, so a page read back from
/// the wrong mapping is visible rather than merely unlikely.
const BYTE: u8 = 0xAB;

/// A hinted anonymous mapping, read-write. `rustix`'s `mmap_anonymous` passes
/// the pointer straight through as the kernel's `addr`, which without
/// `MAP_FIXED` is a *hint* - the thing this program is about.
fn anon_at_hint(at: *mut u8) -> rustix::io::Result<*mut u8> {
    unsafe {
        mmap_anonymous(
            at as *mut c_void,
            LEN,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    }
    .map(|p| p as *mut u8)
}

/// The permissions column of the `/proc/self/maps` line covering `at`, or `None`
/// when no line covers it.
fn maps_perms(at: usize) -> Option<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in maps.lines() {
        let (range, rest) = line.split_once(' ')?;
        let (lo, hi) = range.split_once('-')?;
        let (lo, hi) = (
            usize::from_str_radix(lo, 16).ok()?,
            usize::from_str_radix(hi, 16).ok()?,
        );
        if at >= lo && at < hi {
            return rest.split_whitespace().next().map(str::to_owned);
        }
    }
    None
}

fn main() {
    let mut report = Report::new();

    if std::fs::metadata(PATH).is_ok() {
        let _ = std::fs::remove_file(PATH);
    }
    let mut file = std::fs::File::create(PATH).expect("a file to map");
    file.write_all(&[BYTE; LEN]).expect("its bytes");
    drop(file);

    let fd = open(PATH, OFlags::RDONLY, Mode::empty()).expect("open the fixture");
    let file_map = unsafe {
        mmap(
            core::ptr::null_mut(),
            LEN,
            ProtFlags::READ,
            MapFlags::PRIVATE,
            fd.as_fd(),
            0,
        )
    }
    .expect("map the file read-only") as *mut u8;
    report.check(
        "the file's mapping reads its bytes",
        unsafe { *file_map } == BYTE,
    );

    // (1) and (2): the address is mapped, so the hint must be refused - and the
    // mapping refused it must be *whole*, not a page of it taken away.
    let second = anon_at_hint(file_map).expect("a second mapping, hinted at the first");
    report.check(
        "a hint naming a mapped address is not honoured",
        second as usize != file_map as usize,
    );
    report.check(
        "the file's bytes survived the second mapping",
        (0..LEN)
            .step_by(4096)
            .all(|at| unsafe { *file_map.add(at) } == BYTE),
    );
    unsafe { second.write_volatile(!BYTE) };
    report.check(
        "the mapping that landed elsewhere is writable",
        unsafe { *second } == !BYTE,
    );
    report.check(
        "writing it left the file's mapping alone",
        (0..LEN)
            .step_by(4096)
            .all(|at| unsafe { *file_map.add(at) } == BYTE),
    );

    // (3): the permissions live in the range list for this mapping, so a change
    // has to reach it.
    unsafe {
        mprotect(
            file_map as *mut c_void,
            LEN,
            MprotectFlags::READ | MprotectFlags::WRITE,
        )
    }
    .expect("mprotect the file's mapping");
    report.check(
        "mprotect is what /proc/self/maps reports",
        maps_perms(file_map as usize).is_some_and(|perms| perms.starts_with("rw")),
    );

    // (4): unmapped means free, and a free hint is the address that comes back.
    unsafe { munmap(file_map as *mut c_void, LEN) }.expect("unmap the file's mapping");
    let reused = anon_at_hint(file_map).expect("map the address the file's mapping held");
    report.check(
        "a hint is honoured once the mapping is gone",
        reused as usize == file_map as usize,
    );
    unsafe { reused.write_volatile(0x5A) };
    report.check(
        "the mapping that took the address is writable",
        unsafe { *reused } == 0x5A,
    );

    // (5): a move takes the pages *and* what describes them. The move is an
    // explicit destination, so the answer is exact on both machines.
    let dest = anon_at_hint(core::ptr::null_mut()).expect("a destination to reserve");
    unsafe { munmap(dest as *mut c_void, LEN) }.expect("and give it back");
    let moved = unsafe {
        libc::mremap(
            reused as *mut c_void,
            LEN,
            LEN,
            libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED,
            dest as *mut c_void,
        )
    };
    report.check(
        "mremap moved the mapping to the address it was given",
        moved == dest as *mut c_void,
    );
    let at_old = anon_at_hint(reused).expect("map the address the move left behind");
    report.check(
        "the moved mapping released the address it left",
        at_old as usize == reused as usize,
    );
    let at_new = anon_at_hint(dest).expect("map the address the move arrived at");
    report.check(
        "and the address it moved to is still occupied",
        at_new as usize != dest as usize,
    );

    let _ = std::fs::remove_file(PATH);
    report.finish()
}
