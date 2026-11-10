//! `/proc/<pid>/stat`'s fields 45-51 - where the program's data segment sits,
//! where its break started, and where its argument and environment strings
//! are.
//!
//! Measured on a current host (Linux 7.2.14, probes in `build/scratch/stataddr/`):
//!
//! * `start_data` is the **highest** `p_vaddr` over every `PT_LOAD`, `end_data`
//!   the highest `p_vaddr + p_filesz`, and `start_brk` the page-aligned highest
//!   `p_vaddr + p_memsz` (`fs/binfmt_elf.c`'s own loop). Against a static
//!   binary whose loads are `0x400000/0x518`, `0x401000/0x7ea3d`,
//!   `0x480000/0x344e0`, `0x4b5168/0x58b0 memsz 0xb180` (`a.c`) that reads
//!   `0x4b5168`, `0x4baa18`, `0x4c1000` - each one that rule and nothing else.
//! * None of the three is a live view. A `malloc(1 MiB)` and an 8 KiB local
//!   leave all seven fields where they were (`a.c`), and `sbrk(0)` is not
//!   `start_brk` (`b.c`, `0x2d4df000` against `0x2d4bd000` - the static
//!   binary's own growth).
//! * `arg_start` is exactly `argv[0]`'s address, `env_start` is exactly
//!   `environ[0]`'s, `env_start == arg_end`, and the strings run **ascending**
//!   from `arg_start`: `argv` in order first, then the environment, then the
//!   `AT_EXECFN` path (`c.c`, `d.c` - `execfn - env_end == 0`).
//! * A dynamic binary's six image fields describe the **program's** segments,
//!   not the interpreter's (`e.c`), and a zombie has no `mm` and reads seven
//!   zeros (`zombiev/z.c`).
//!
//! The decisive claims here are the ones a constant cannot pass: the byte
//! range `arg_start..arg_end` must *be* this program's `argv`, NUL-separated
//! and in order - which pins both the bounds and Linux's ascending layout -
//! and a large `malloc` must move none of the image addresses. A hardcoded
//! zero fails the first; a live `sbrk`/`rsp` substituted for the recorded
//! values fails the second.

use mpos_user_tests::Report;

/// Everything after the command name's last `)`. `/proc/<pid>/stat` lets
/// `comm` contain spaces and parentheses, so the numbered fields begin here;
/// the first token is field 3 (`state`).
fn stat_tail(text: &str) -> Option<&str> {
    Some(&text[text.rfind(')')? + 1..])
}

/// A numbered field parsed as `u64` - these seven are plain decimal addresses.
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

/// All seven addresses, in Linux's field order.
fn addresses_of(pid: i32) -> Option<[u64; 7]> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut out = [0u64; 7];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = stat_field(&text, 45 + index)?;
    }
    Some(out)
}

/// The `[stack]` mapping's range, from this process's own maps.
fn stack_mapping() -> Option<(u64, u64)> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in maps.lines() {
        if !line.ends_with("[stack]") {
            continue;
        }
        let range = line.split_whitespace().next()?;
        let (low, high) = range.split_once('-')?;
        return Some((
            u64::from_str_radix(low, 16).ok()?,
            u64::from_str_radix(high, 16).ok()?,
        ));
    }
    None
}

/// Read `len` bytes at `address` in this, our own, address space.
///
/// # Safety
///
/// The caller passes a range the kernel has just reported as this process's
/// argument or environment strings, which are mapped and readable by
/// construction.
unsafe fn bytes_at(address: u64, len: usize) -> &'static [u8] {
    // SAFETY: the caller's contract - a mapped readable range of this process.
    unsafe { core::slice::from_raw_parts(address as *const u8, len) }
}

/// Split a NUL-terminated run of strings into its pieces.
fn nul_strings(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == 0 && index > start {
            out.push(&bytes[start..index]);
            start = index + 1;
        }
    }
    out
}

/// Fork a child that parks until told to die, for the zombie claim.
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
    let Some(before) = addresses_of(me) else {
        report.check("/proc/self/stat answers its seven address fields", false);
        report.finish();
    };
    let [
        start_data,
        end_data,
        start_brk,
        arg_start,
        arg_end,
        env_start,
        env_end,
    ] = before;

    report.check(
        "the seven fields are addresses, not zeros",
        before.iter().all(|value| *value != 0),
    );
    report.check(
        "the data segment ends at or above where it starts",
        end_data >= start_data && start_data != 0,
    );
    report.check(
        "and the break starts on a page at or above the data's end",
        start_brk >= end_data && start_brk % 4096 == 0,
    );

    // The argument area must *be* this program's argv, in order and
    // NUL-separated. This is the claim that pins the ascending layout: a
    // writer that placed argv[0] highest would still report a start and an
    // end, but the bytes between them would not read as the arguments.
    let argv: Vec<Vec<u8>> = std::env::args().map(|arg| arg.into_bytes()).collect();
    let arg_bytes = unsafe { bytes_at(arg_start, (arg_end - arg_start) as usize) };
    let found = nul_strings(arg_bytes);
    report.check(
        "the bytes from arg_start to arg_end are this program's argv",
        found == argv.iter().map(|arg| arg.as_slice()).collect::<Vec<_>>(),
    );
    report.check(
        "and argv[0] is the first string, so the strings ascend",
        found.first().map(|first| *first) == argv.first().map(|first| first.as_slice()),
    );

    // The environment follows immediately: `env_start` is `arg_end` itself, so
    // the two areas are one contiguous run with no gap and no overlap.
    report.check(
        "the environment starts where the arguments end",
        env_start == arg_end,
    );
    if env_end > env_start {
        let env_bytes = unsafe { bytes_at(env_start, (env_end - env_start) as usize) };
        let names = nul_strings(env_bytes);
        report.check(
            "and the bytes from env_start to env_end are environment entries",
            !names.is_empty() && names.iter().all(|name| name.contains(&b'=')),
        );
    } else {
        report.check(
            "and the bytes from env_start to env_end are environment entries",
            false,
        );
    }

    // All four live in the stack's own mapping - that is where the entry block
    // is placed, and a value outside it would be a pointer into nothing.
    match stack_mapping() {
        Some((low, high)) => {
            report.check(
                "all four string bounds are inside the stack's mapping",
                arg_start >= low && env_end <= high,
            );
        }
        None => report.check(
            "all four string bounds are inside the stack's mapping",
            false,
        ),
    }

    // The three image addresses are a record, not a live view: a megabyte of
    // heap must leave every one of them exactly where it was. This is what a
    // reader that substituted `sbrk(0)` or a VMA boundary would fail.
    let touched = {
        // SAFETY: one anonymous mapping of our own, touched so the pages are
        // really there rather than merely reserved.
        unsafe {
            let len = 1 << 20;
            let base = libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if base == libc::MAP_FAILED {
                None
            } else {
                core::ptr::write_bytes(base.cast::<u8>(), 0x5a, len);
                Some(base)
            }
        }
    };
    let after = addresses_of(me).unwrap_or([0; 7]);
    report.check(
        "a megabyte of heap moves none of the image's addresses",
        after == before,
    );
    if let Some(base) = touched {
        // SAFETY: the mapping made above, still ours.
        unsafe {
            libc::munmap(base, 1 << 20);
        }
    }

    // A fork inherits the whole record: the child is running the same program
    // from the same stack, so its seven fields are the parent's.
    match zombie_child() {
        Some((pid, go, ready)) => {
            let mut byte: u8 = 0;
            // SAFETY: the parent's end of the ready pipe.
            unsafe {
                libc::read(ready, (&mut byte as *mut u8).cast(), 1);
                libc::close(ready);
            }
            let child = addresses_of(pid).unwrap_or([0; 7]);
            report.check(
                "a forked child reports the parent's seven addresses",
                child == before,
            );

            // ...and a zombie has no address space at all, so all seven read
            // zero rather than the value they had while it was alive.
            // SAFETY: the parent's end of the go pipe; one byte tells it to
            // exit.
            unsafe {
                let byte: u8 = 1;
                libc::write(go, (&byte as *const u8).cast(), 1);
                libc::close(go);
            }
            // Wait for the death without reaping it: `waitpid(WNOHANG)` until
            // the state reads `Z`, because a zombie that has been collected has
            // no `/proc` entry at all and would report `None` rather than seven
            // zeros.
            let mut zombie = [1u64; 7];
            for _ in 0..1000 {
                let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                if stat_tail(&text).map(|tail| tail.trim_start().starts_with('Z')) == Some(true) {
                    zombie = addresses_of(pid).unwrap_or([1; 7]);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            report.check("a zombie reports seven zeros", zombie == [0; 7]);

            let mut status = 0i32;
            // SAFETY: `waitpid` on this process's own child, still unreaped.
            unsafe {
                libc::waitpid(pid, &mut status, 0);
            }
        }
        None => {
            report.check("a forked child reports the parent's seven addresses", false);
            report.check("a zombie reports seven zeros", false);
        }
    }

    report.finish();
}
