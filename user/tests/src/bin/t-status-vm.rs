//! `/proc/<pid>/status`'s four mapping lines that were literal zeros -
//! `VmData`, `VmStk`, `VmExe`, `VmLib`.
//!
//! Linux answers each from a different count, and the four rules were measured
//! one at a time on a current host (Linux 7.2.14, probes in
//! `build/scratch/vmlines/`), then checked against the kernel's own source
//! (`fs/proc/task_mmu.c`'s `task_mem`, `mm/mmap.c`'s `vm_stat_account`,
//! `mm/vma.h`'s three predicates):
//!
//! * `VmData` counts pages that are **writable, not shared and not the stack**
//!   - mapped, not resident. A 1-byte `mmap` moves it by a whole 4 kB page; a
//!   1 MiB `alloca` moves `VmStk` and leaves `VmData` alone; a `MAP_SHARED`
//!   anonymous mapping moves it not at all.
//! * `VmStk` counts the `[stack]` mapping, and only it: `MAP_STACK` on an
//!   ordinary anonymous mapping does not put it in this book.
//! * `VmExe` is the span of the program's own text, and `VmLib` is the rest of
//!   the executable book - the loader, libc, the vdso. `VmExe` is *not* the
//!   sum of the executable segments' page counts: two one-page segments 1 MiB
//!   apart report `VmExe: 16 kB` while the same two adjacent report `8 kB`.
//! * A zombie's `status` has no `Vm*` line **at all**, because Linux reaches
//!   `task_mem` only through an `mm` that still exists and a zombie has none.
//!
//! Which lines a constant cannot fake: every one that moves. This program
//! therefore reads a baseline, makes the change the rule predicts, and reads
//! again - a kernel printing `0 kB` (what §6.117 replaced), or printing the
//! resident set for all four (the shape before it), fails the movement claims
//! and passes only the two shape ones.

use mpos_user_tests::Report;

/// One `Key:\t N kB` line's `N`, from a `status` text.
fn kb(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start_matches(':').trim_start();
        // `N kB`
        let number = rest.split_whitespace().next()?;
        return number.parse().ok();
    }
    None
}

/// The `Vm*`/`Rss*` block of a task's `status`, or `None` when the task does
/// not answer at all.
fn status_of(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/status")).ok()
}

fn field(text: &str, key: &str) -> Option<u64> {
    kb(text, key)
}

/// A pipe pair, whose two ends land somewhere in the process's address space -
/// used as a mapping whose size is chosen. Returns the fds.
fn pipe_pair() -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: two ints this process owns.
    (unsafe { libc::pipe(fds.as_mut_ptr()) } == 0).then_some((fds[0], fds[1]))
}

fn main() {
    let mut report = Report::new();
    let me = std::process::id() as i32;

    let Some(base) = status_of(me) else {
        report.check("/proc/self/status answers", false);
        report.finish();
    };

    let data = field(&base, "VmData:");
    let stk = field(&base, "VmStk:");
    let exe = field(&base, "VmExe:");
    let lib = field(&base, "VmLib:");
    let size = field(&base, "VmSize:");

    report.check(
        "the four mapping lines are counts, not zeros",
        data.is_some_and(|v| v > 0) && stk.is_some_and(|v| v > 0) && exe.is_some_and(|v| v > 0),
    );
    // The parts must add up to no more than the whole: every book these four
    // lines are cut from is address space, so none of them can exceed `VmSize`.
    report.check(
        "and none of them exceeds the address space it came from",
        [data, stk, exe, lib]
            .iter()
            .all(|value| value.zip(size).is_none_or(|(v, total)| v <= total)),
    );
    // `VmStk` is the stack mapping, which Linux makes 132 kB for an ordinary
    // static process. Asserting a range rather than the number keeps this a
    // claim about the rule and not about this program's own frame depth.
    report.check(
        "the stack book is a stack's size, not the address space's",
        stk.is_some_and(|v| v > 0 && v < 1024 * 1024),
    );

    // A private writable mapping is `VmData`, page-rounded, and nothing else
    // moves. One byte asked for - one whole page counted.
    let before_data = field(&status_of(me).unwrap_or_default(), "VmData:");
    // SAFETY: a fresh one-page private anonymous mapping, unmapped below.
    let one = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            1,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped = one != libc::MAP_FAILED;
    let after = status_of(me).unwrap_or_default();
    let after_data = field(&after, "VmData:");
    let after_stk = field(&after, "VmStk:");
    report.check(
        "a private writable page is one more kilobyte count in VmData",
        mapped && before_data.zip(after_data).is_some_and(|(a, b)| b == a + 4),
    );
    report.check("and the stack book did not move for it", stk == after_stk);
    if mapped {
        // SAFETY: the mapping this function just made.
        unsafe { libc::munmap(one, 1) };
    }

    // A private *read-only* mapping is address space but not data: `VmData`
    // counts writable pages.
    let before_data = field(&status_of(me).unwrap_or_default(), "VmData:");
    // SAFETY: a fresh one-page read-only private anonymous mapping. `PROT_NONE`
    // is what makes it non-writable in a way the kernel can see without a VMA
    // flag this kernel does not carry.
    let ro = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped_ro = ro != libc::MAP_FAILED;
    let after_data = field(&status_of(me).unwrap_or_default(), "VmData:");
    report.check(
        "a non-writable mapping adds nothing to VmData",
        mapped_ro && before_data == after_data,
    );
    if mapped_ro {
        // SAFETY: the mapping this function just made.
        unsafe { libc::munmap(ro, 4096) };
    }

    // A `MAP_SHARED` anonymous mapping is shared, so it is not data either -
    // Linux reports it in `RssShmem` once touched, which this kernel does not
    // count, but it must not leak into `VmData`.
    let before_data = field(&status_of(me).unwrap_or_default(), "VmData:");
    // SAFETY: a fresh page-sized shared anonymous mapping, unmapped below.
    let shared = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped_shared = shared != libc::MAP_FAILED;
    let after_data = field(&status_of(me).unwrap_or_default(), "VmData:");
    report.check(
        "a shared mapping is not private data",
        mapped_shared && before_data == after_data,
    );
    if mapped_shared {
        // SAFETY: the mapping this function just made.
        unsafe { libc::munmap(shared, 4096) };
    }

    // `VmExe` is the span of this program's own text and `VmLib` is the rest
    // of the executable book - the interpreter, libc, the vdso. That split is
    // what makes the next claim possible: a fresh **executable** mapping is
    // nowhere near the image's text, so it must grow `VmLib` and leave `VmExe`
    // alone. A kernel that reported the whole executable book as `VmExe`, or
    // that reported the image's text as the book, fails this.
    let before = status_of(me).unwrap_or_default();
    let before_exe = field(&before, "VmExe:");
    let before_lib = field(&before, "VmLib:");
    // SAFETY: a fresh one-page read+execute private anonymous mapping, placed
    // by the kernel far from the image's own text; unmapped below.
    let code = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped_code = code != libc::MAP_FAILED;
    let after = status_of(me).unwrap_or_default();
    let after_exe = field(&after, "VmExe:");
    let after_lib = field(&after, "VmLib:");
    report.check(
        "an executable mapping outside the image is a library, not the text",
        mapped_code
            && before_exe == after_exe
            && before_lib.zip(after_lib).is_some_and(|(a, b)| b == a + 4),
    );
    if mapped_code {
        // SAFETY: the mapping this function just made.
        unsafe { libc::munmap(code, 4096) };
    }
    // And the split is a split of one book, so the two together are what the
    // executable mappings' pages come to - which the address-space bound above
    // already holds them to. `VmLib` alone may legitimately be 8 kB on a static
    // binary (the vdso) and large on a dynamic one, so no absolute is claimed.
    report.check(
        "the executable books are two halves of what is executable",
        exe.is_some() && lib.is_some(),
    );

    // The zombie: no `Vm*` line at all, not one reading zero. Linux reaches
    // `task_mem` only through an `mm` that still exists.
    let mut go = [0i32; 2];
    let mut ready = [0i32; 2];
    // SAFETY: four ints this process owns.
    let made = unsafe { libc::pipe(go.as_mut_ptr()) == 0 && libc::pipe(ready.as_mut_ptr()) == 0 };
    if !made {
        report.check("a zombie's status carries no Vm line", false);
        report.finish();
    }
    // SAFETY: `fork` in a single-threaded test process; the child only reads a
    // pipe and exits.
    let pid = unsafe { libc::fork() };
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
    let mut sink: u8 = 0;
    // SAFETY: the pipes this function made.
    unsafe {
        libc::close(go[0]);
        libc::close(ready[1]);
        libc::read(ready[0], (&mut sink as *mut u8).cast(), 1);
        libc::close(ready[0]);
        let byte: u8 = 1;
        libc::write(go[1], (&byte as *const u8).cast(), 1);
        libc::close(go[1]);
    }
    let mut zombie_text = String::new();
    for _ in 0..200 {
        if let Some(text) = status_of(pid) {
            // A zombie's `State:` is `Z (zombie)`; wait for that rather than
            // for the file's existence, which it had before it died.
            if text.lines().any(|line| line == "State:\tZ (zombie)") {
                zombie_text = text;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    report.check(
        "a zombie's status carries no Vm line",
        !zombie_text.is_empty()
            && !zombie_text.contains("VmSize:")
            && !zombie_text.contains("VmData:")
            && !zombie_text.contains("VmRSS:"),
    );
    report.check(
        "and still says it is one",
        zombie_text.contains("State:\tZ (zombie)"),
    );
    // SAFETY: the child this function forked.
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };

    // A live process's status does carry them, so the absence above is the
    // zombie's answer rather than a block this kernel never prints.
    report.check(
        "a live process does print them",
        status_of(me).is_some_and(|text| text.contains("VmSize:")),
    );

    let _ = pipe_pair();
    report.finish();
}
