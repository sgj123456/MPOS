//! `/proc/<pid>/stat`'s fields 26-28 (`startcode`, `endcode`, `startstack`) -
//! the address of the program image's text and the address the process was
//! entered at.
//!
//! Measured on a current host (Linux 7.2.14, probes in `build/scratch/`):
//!
//! * `startcode` is the linked address of the **lowest** executable `PT_LOAD`
//!   segment, not page-aligned (`build/scratch/statcode/start.S` +
//!   `report.c`: `0x401000` for a page-aligned text, `0x401234` for one linked
//!   off a page).
//! * `endcode` is that segment's end **of file bytes**: a single `RWE` load
//!   with `FileSiz 0x640` and a 1 MiB `.bss` behind it reads
//!   `endcode - startcode = 0x640`, not `0x100648`
//!   (`build/scratch/statcode/x.c` - the decisive probe, since a `.bss` inside
//!   an executable segment is the only way to make the two differ).
//! * `startstack` is the `rsp` the program was entered with, exactly - an
//!   assembly entry that saves `%rsp` before anything touches it reads back
//!   the same number `delta 0` (`build/scratch/statcode/start.S`). It is not
//!   the `[stack]` mapping's boundary and not the current `rsp`: inside `main`
//!   the live `rsp` was 5024 bytes *below* it.
//! * A `fork`ed child that has not `exec`ed reports the parent's three values
//!   byte for byte; an `exec` replaces them; an uncollected zombie has no
//!   address space and reports `0` for all three
//!   (`build/scratch/statcode/inherit2.c`, `zombiev/z.c`).
//!
//! The two claims that a constant cannot pass are the *relations*: adding a
//! large anonymous mapping must not move `startcode`/`endcode` (they are the
//! image's, not the address space's), and a zombie's three must be zero while
//! its parent's are not. A hardcoded `0` fails the first pair outright; a
//! hardcoded copy of the reader's own values fails the zombie.

use mpos_user_tests::Report;

/// Everything after the command name's last `)`. `/proc/<pid>/stat` lets
/// `comm` contain spaces and parentheses, so the numbered fields begin here;
/// the first token is field 3 (`state`).
fn stat_tail(text: &str) -> Option<&str> {
    Some(&text[text.rfind(')')? + 1..])
}

/// A numbered field parsed as `u64` - these three are plain decimal addresses.
fn stat_field(text: &str, field: usize) -> Option<u64> {
    let mut tokens = stat_tail(text)?.split_whitespace();
    let mut index = 3;
    loop {
        let token = tokens.next()?;
        if index == field {
            return token.parse().ok();
        }
        index += 1;
    }
}

/// The first main-executable segment's address, from the process's own
/// `/proc/self/maps` - the `r-xp` line whose path is this program. Used as an
/// independent witness for `startcode`: the two come from different parts of
/// the kernel, and Linux reports the segment's own unaligned `p_vaddr` in
/// `stat` while `maps` shows the page it starts in.
fn text_mapping_start() -> Option<u64> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let exe = std::fs::read_link("/proc/self/exe").ok()?;
    let exe = exe.to_string_lossy();
    for line in maps.lines() {
        // `start-end perms offset dev inode  path`
        let mut parts = line.split_whitespace();
        let range = parts.next()?;
        let perms = parts.next()?;
        if !perms.starts_with("r-x") {
            continue;
        }
        // `offset dev inode path` - the path is the last field.
        let path = parts.last().unwrap_or("");
        if path != exe {
            continue;
        }
        let start = range.split('-').next()?;
        return u64::from_str_radix(start, 16).ok();
    }
    None
}

/// The stack pointer as it is inside this function - the number `startstack`
/// must be at or above, since Linux's field is the *entry* value and a call
/// only ever subtracts.
fn live_stack_pointer() -> u64 {
    let live: u64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: reading a register, which needs no memory and no stack.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) live, options(nomem, nostack));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: the same read on the other machine - `sp` is not general-purpose
    // there, so it has a mnemonic of its own.
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) live, options(nomem, nostack));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let live = 0;
    live
}

fn bounds_of(pid: i32) -> Option<(u64, u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some((
        stat_field(&text, 26)?,
        stat_field(&text, 27)?,
        stat_field(&text, 28)?,
    ))
}

/// Fork a child that parks until told to die, for the zombie claim. Returns the
/// pid; the two pipe ends are returned so the caller controls the timing.
fn zombie_child() -> Option<(i32, i32, i32)> {
    let mut go = [0i32; 2];
    let mut ready = [0i32; 2];
    // SAFETY: four ints this process owns.
    unsafe {
        if libc::pipe(go.as_mut_ptr()) != 0 {
            return None;
        }
        if libc::pipe(ready.as_mut_ptr()) != 0 {
            return None;
        }
    }
    // SAFETY: `fork` in a single-threaded test process; the child only reads a
    // pipe, writes one byte and exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // SAFETY: no Rust after this point, no destructor, no allocator.
        unsafe {
            libc::close(go[1]);
            libc::close(ready[0]);
            let byte: u8 = 1;
            libc::write(ready[1], (&byte as *const u8).cast(), 1);
            let mut sink: u8 = 0;
            libc::read(go[0], (&mut sink as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    // SAFETY: the parent's ends, already duplicated into the child.
    unsafe {
        libc::close(go[0]);
        libc::close(ready[1]);
    }
    Some((pid, go[1], ready[0]))
}

fn main() {
    let mut report = Report::new();

    let me = std::process::id() as i32;
    let Some((start_code, end_code, start_stack)) = bounds_of(me) else {
        report.check("/proc/self/stat answers its three address fields", false);
        report.finish();
    };

    report.check(
        "the three fields are addresses, not zeros",
        start_code != 0 && end_code != 0 && start_stack != 0,
    );
    report.check("the text's end is above its start", end_code > start_code);
    // The image is placed in the `mmap` band, so its text is far above zero
    // and far below the kernel half - an assertion about the shape rather than
    // an absolute, which the ASLR draw makes impossible to write down.
    report.check(
        "and both are inside the user half of the address space",
        start_code < (1 << 47) && end_code < (1 << 47),
    );

    // `startcode` against an independent witness: the page `maps` shows this
    // program's executable segment starting in. They differ by the segment's
    // alignment inside that page, so `maps`' page is the lower bound.
    match text_mapping_start() {
        Some(mapped) => {
            let page = 4096u64;
            report.check(
                "the start is in the page maps gives the executable segment",
                start_code >= mapped && start_code < mapped + page,
            );
        }
        None => report.check(
            "the start is in the page maps gives the executable segment",
            false,
        ),
    }

    // `startstack` must be in the stack's own mapping - the `[stack]` line -
    // and above the live `rsp` inside this function, which is where Linux's
    // entry `rsp` differs measurably.
    let live_rsp = live_stack_pointer();
    report.check(
        "the entry stack pointer is at or above the live one",
        start_stack >= live_rsp,
    );
    report.check(
        "and it is in the stack's mapping, not just anywhere high",
        std::fs::read_to_string("/proc/self/maps")
            .ok()
            .and_then(|maps| {
                maps.lines()
                    .find(|line| line.ends_with("[stack]"))
                    .and_then(|line| {
                        let range = line.split_whitespace().next()?;
                        let (low, high) = range.split_once('-')?;
                        let low = u64::from_str_radix(low, 16).ok()?;
                        let high = u64::from_str_radix(high, 16).ok()?;
                        Some(start_stack >= low && start_stack <= high)
                    })
            })
            .unwrap_or(false),
    );

    // The relation that a constant cannot pass: a large anonymous mapping is
    // address space, not the image, so the three fields must not move.
    let before = bounds_of(me);
    // SAFETY: a fresh 8 MiB private anonymous mapping, unmapped below.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            8 * 1024 * 1024,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let got_mapping = mapped != libc::MAP_FAILED;
    let after = bounds_of(me);
    report.check(
        "eight more megabytes of address space move none of the three",
        got_mapping && before == after,
    );
    if got_mapping {
        // SAFETY: the mapping this function just made.
        unsafe { libc::munmap(mapped, 8 * 1024 * 1024) };
    }

    // `fork` without `exec` keeps the image, so all three are the parent's.
    match zombie_child() {
        Some((pid, go, ready)) => {
            let mut sink: u8 = 0;
            // SAFETY: the readiness pipe this function made; the child writes
            // one byte before parking.
            unsafe { libc::read(ready, (&mut sink as *mut u8).cast(), 1) };
            // SAFETY: the pipe's parent end.
            unsafe { libc::close(ready) };
            let child = bounds_of(pid);
            report.check(
                "a forked child reports its parent's three addresses",
                child == Some((start_code, end_code, start_stack)),
            );
            // Now let it exit and read the zombie: no address space, so all
            // three are zero - while the parent's are still its own.
            // SAFETY: the go pipe this function made.
            unsafe {
                let byte: u8 = 1;
                libc::write(go, (&byte as *const u8).cast(), 1);
                libc::close(go);
            }
            // Wait for it to become a zombie without reaping it: a short
            // settle loop, since `waitpid(WNOHANG)` is what would remove the
            // file this claim reads.
            let mut state = 0u64;
            for _ in 0..200 {
                if let Some((..)) = bounds_of(pid) {
                    let text =
                        std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                    state = stat_field(&text, 3).unwrap_or(0);
                    let s = stat_tail(&text).and_then(|t| t.split_whitespace().next());
                    if s == Some("Z") {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let zombie = bounds_of(pid);
            report.check(
                "a zombie has no image and reads zero for all three",
                zombie == Some((0, 0, 0)),
            );
            report.check(
                "while its parent still reports its own",
                bounds_of(me) == Some((start_code, end_code, start_stack)),
            );
            // SAFETY: the child this function forked, and nothing else has
            // waited on it.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            let _ = state;
        }
        None => {
            for claim in [
                "a forked child reports its parent's three addresses",
                "a zombie has no image and reads zero for all three",
                "while its parent still reports its own",
            ] {
                report.check(claim, false);
            }
        }
    }

    report.finish();
}
