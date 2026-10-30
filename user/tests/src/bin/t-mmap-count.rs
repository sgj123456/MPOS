//! A process may hold `max_map_count` mappings and no more: Linux's default
//! ceiling is 65 530 descriptions and the answer past it is `ENOMEM`.
//!
//! Measured on Linux 7.2.11 (`build/scratch/mmapcount/p1.c`): one-page
//! `PROT_NONE`/`PROT_READ` regions map until the kernel refuses with `ENOMEM`
//! (errno 12) at the sysctl's value; `munmap` one and the next `mmap`
//! succeeds; a `MAP_FIXED` over an existing mapping does not change the count.
//! That development host has `max_map_count` tuned up to 1 048 576, so the
//! boundary this pin walks is this kernel's own - the *default* Linux ships.
//!
//! The pin is also the measurement that keeps the table honest: reaching the
//! ceiling costs 65 530 placements, and before `VmaTable` became a tree one
//! insert re-sorted and rebuilt the whole list, so a program could not get
//! near the number at all (`docs/notes/` §10.11).
use mpos_user_tests::Report;

const PAGE: usize = 4096;
/// Linux's default `/proc/sys/vm/max_map_count`.
const LINUX_DEFAULT_MAX_MAP_COUNT: usize = 65_530;
/// The descriptions a program starts with. Linux counts every VMA a process
/// holds, so the successes below are the ceiling minus this - kept as a band
/// rather than a number because musl's own layout is not this kernel's to pin.
const BASELINE_SLACK: usize = 32;

fn map_page(write: bool) -> Result<*mut libc::c_void, i32> {
    let prot = if write {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_NONE
    };
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

fn main() {
    let mut r = Report::new();

    if mpos_user_tests::host_run() {
        // The development host's sysctl is tuned up (measured 1 048 576), so
        // the ceiling is not reachable in one program's life here. The claim
        // this pin makes is about the kernel that ships, and a host run can
        // only report what its own setting is.
        let host = std::fs::read_to_string("/proc/sys/vm/max_map_count")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok());
        r.check(
            &format!(
                "the host's max_map_count is {host:?}; this kernel ships Linux's default {LINUX_DEFAULT_MAX_MAP_COUNT} (a guest claim)"
            ),
            host.is_some_and(|n| n >= LINUX_DEFAULT_MAX_MAP_COUNT),
        );
        r.finish();
    }

    // Map one-page regions that nothing can merge (alternating permissions, so
    // two neighbours never describe the same thing), until the ceiling refuses.
    let mut mapped = 0usize;
    let mut refusal = 0i32;
    // The last few mappings, so the ceiling can be relieved before anything is
    // printed: a process *at* Linux's ceiling cannot grow its heap either, and
    // the first `format!` below would abort with the allocator's own message
    // (measured: `memory allocation of 80 bytes failed`, exit 134). Sixty-four
    // descriptions of headroom is what printing costs.
    const RELIEF: usize = 64;
    let mut recent = [std::ptr::null_mut::<libc::c_void>(); RELIEF];
    loop {
        match map_page(mapped % 2 == 1) {
            Ok(p) => {
                recent[mapped % RELIEF] = p;
                mapped += 1;
            }
            Err(e) => {
                refusal = e;
                break;
            }
        }
        // A machine without a ceiling stops here rather than walking the whole
        // address space: the claim below then fails instead of hanging.
        if mapped > LINUX_DEFAULT_MAX_MAP_COUNT + BASELINE_SLACK {
            break;
        }
    }
    // Give the allocator room before the claims are written out: these names
    // are heap strings, and the heap's next growth is an `mmap` like any other.
    for p in recent.iter() {
        if !p.is_null() {
            unsafe { libc::munmap(*p, PAGE) };
        }
    }
    r.check(
        &format!("the ceiling refuses with ENOMEM (errno {refusal})"),
        refusal == libc::ENOMEM,
    );
    r.check(
        &format!(
            "the process holds Linux's default {LINUX_DEFAULT_MAX_MAP_COUNT} descriptions, less the ones it started with ({mapped} were handed out)"
        ),
        mapped + BASELINE_SLACK >= LINUX_DEFAULT_MAX_MAP_COUNT && mapped <= LINUX_DEFAULT_MAX_MAP_COUNT,
    );

    // The count, not memory, is the gate: the relief above is itself the
    // evidence - sixty-four descriptions came back, so mappings fit again
    // where the kernel refused one a moment ago. Nothing here touches a page,
    // so no frame is involved either way.
    let mut again = [std::ptr::null_mut::<libc::c_void>(); RELIEF];
    let mut fits = 0usize;
    while fits < RELIEF {
        match map_page(true) {
            Ok(p) => {
                again[fits] = p;
                fits += 1;
            }
            Err(_) => break,
        }
    }
    r.check(
        &format!("the descriptions given back are room for new mappings again ({fits} fit)"),
        fits >= RELIEF / 2,
    );
    // And back they go, so the fork below has room for its own page.
    for p in again.iter() {
        if !p.is_null() {
            unsafe { libc::munmap(*p, PAGE) };
        }
    }

    // The machine still serves: a child maps a page and finishes.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let ok = matches!(map_page(true), Ok(_));
        std::process::exit(if ok { 0 } else { 3 });
    }
    let mut status = 0i32;
    let reaped = pid > 0 && unsafe { libc::waitpid(pid, &mut status, 0) } == pid;
    r.check(
        "the machine still serves a process after the ceiling was reached",
        reaped && unsafe { libc::WIFEXITED(status) } && unsafe { libc::WEXITSTATUS(status) } == 0,
    );

    r.finish();
}
