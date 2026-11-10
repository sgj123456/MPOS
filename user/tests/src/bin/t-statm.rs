//! `/proc/<pid>/statm`'s seven fields - the same memory books `status` prints,
//! through a reader that does **not** agree with it in one place.
//!
//! Linux fills four of the seven from `task_statm` and prints the other three,
//! `lib` and the last, as literal zeros; the four are measured separately
//! below because `status`'s nearest lines are not them:
//!
//! * field 1 `size` is `total_vm`, field 2 `resident` and field 3 `shared` are
//!   the resident set split into its file/shmem half and the rest - measured
//!   as movement, since an idle process's numbers are the binary's.
//! * field 4 `text` is `PAGE_ALIGN(end_code) - (start_code & PAGE_MASK)`, in
//!   pages, **without the `min()` that clamps `status`'s `VmExe` to the
//!   executable book**. Two one-page executable segments 1 MiB apart make this
//!   field 257 pages while the same binary's `VmExe:` line reads 16 kB - a
//!   probe in `build/scratch/vmlines/far.c` reads `statm: 43 4 2 1281 0 33 0`
//!   beside `VmExe: 16 kB` on one `mm`. A kernel that printed `VmExe / 4` here
//!   answers 4 for a binary whose text spans 1281 pages.
//! * field 6 `data` is `data_vm + stack_vm` - the stack is counted here, where
//!   `status` splits it out into `VmStk` and leaves it out of `VmData`.
//!   Traced on Linux 7.2.14 (`build/scratch/statm/d.c`): `data` is 84 pages at
//!   rest and equals `VmData/4 + VmStk/4` exactly; a 1 MiB stack frame moves
//!   `data` from 84 to 310 as `VmStk` goes 33 to 259, `VmData` unmoved.
//! * a zombie's `statm` is seven zeros, because `task_statm` is reached only
//!   through an `mm` and a zombie has none
//!   (`build/scratch/statm/z.c`: `zombie statm: 0 0 0 0 0 0 0`).
//!
//! The clock ticks `dt` at the end are Linux's to print and are zero here for
//! the same reason `status`'s are: this kernel counts no page-table accounting.

use mpos_user_tests::Report;

/// The seven numbers of a `statm`, in order.
fn statm_of(pid: i32) -> Option<[u64; 7]> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let mut fields = [0u64; 7];
    let mut seen = 0;
    for word in text.split_whitespace() {
        if seen == fields.len() {
            return None;
        }
        fields[seen] = word.parse().ok()?;
        seen += 1;
    }
    // Linux prints exactly seven; a shorter or longer line is not the file.
    (seen == fields.len()).then_some(fields)
}

/// One `Key:\t N kB` line's `N`.
fn kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
        .and_then(|number| number.parse().ok())
}

fn status_of(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default()
}

fn main() {
    let mut report = Report::new();
    let me = std::process::id() as i32;

    let Some(base) = statm_of(me) else {
        report.check("/proc/self/statm answers seven numbers", false);
        report.finish();
    };
    report.check(
        "/proc/self/statm answers seven numbers",
        statm_of(me).is_some(),
    );

    // The three literal zeros Linux prints: `lib`, and the two it has no
    // book for. Asserting them keeps a reader from filling a field in that
    // the file's own shape says is not this kernel's to answer.
    report.check(
        "lib and the page-table tick are Linux's own zeros",
        base[4] == 0 && base[6] == 0,
    );

    // --- `data` is the stack's book too -----------------------------------
    // A mapping grows `data` by its own pages, and a stack frame grows it by
    // the stack's - whereas `VmData` moves for the first and not the second.
    let before = statm_of(me).unwrap_or(base);
    let before_status = status_of(me);
    let before_data = kb(&before_status, "VmData:");
    let before_stk = kb(&before_status, "VmStk:");
    // SAFETY: 1 MiB of fresh private anonymous memory, unmapped below.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            1 << 20,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped = map != libc::MAP_FAILED;
    let after_map = statm_of(me).unwrap_or(before);
    report.check(
        &format!(
            "a megabyte of private mapping is 256 more pages of statm data ({} -> {} pages)",
            before[5], after_map[5]
        ),
        mapped && after_map[5] > before[5],
    );
    // The same mapping, seen through the other reader: `VmData` moved too, by
    // exactly the same amount, in kB. The two readers agree here.
    let after_status = status_of(me);
    let after_data = kb(&after_status, "VmData:");
    report.check(
        &format!(
            "and status's VmData moved too ({} kB -> {:?} kB)",
            before_data.unwrap_or(0),
            after_data
        ),
        mapped && before_data.zip(after_data).is_some_and(|(a, b)| b > a),
    );
    // The stack book is *inside* `data` and outside `VmData`, and this process
    // has not touched its stack enough to move either; assert the invariant
    // rather than a number: data == data_vm + stack_vm, in pages.
    report.check(
        "statm's data is VmData's pages plus the stack's",
        after_data
            .zip(kb(&after_status, "VmStk:"))
            .is_some_and(|(data, stk)| after_map[5] == data / 4 + stk / 4),
    );
    if mapped {
        // SAFETY: the mapping this function just made.
        unsafe { libc::munmap(map, 1 << 20) };
    }

    // --- `text` is the span, and it is *not* clamped ----------------------
    // This is the field `status` cannot answer: `VmExe` is clamped to the
    // executable book, so a binary whose two executable segments sit 1 MiB
    // apart reads 16 kB there. `statm` reports the span's pages instead. The
    // claim this kernel can be held to without a custom-linked binary is the
    // relation the clamp breaks: `text` pages are at least as many as
    // `VmExe`/4, and they are equal only when the image is compact. A kernel
    // printing `VmExe / 4` fails whenever the image is not.
    let status = status_of(me);
    let exe_kb = kb(&status, "VmExe:");
    let text_pages = statm_of(me).unwrap_or(base)[3];
    report.check(
        &format!("statm's text is the image's executable span, in pages ({text_pages})"),
        text_pages >= 1 && text_pages <= 4096,
    );
    report.check(
        "and never fewer pages than status's clamped VmExe reports",
        exe_kb.is_none_or(|exe| text_pages >= exe.div_ceil(4)),
    );

    // --- the image's own numbers, against `stat` --------------------------
    // `statm`'s first field and `stat`'s `vsize` are one number in two units,
    // and the address space the mapping above added must show in both.
    let stat = std::fs::read_to_string(format!("/proc/{me}/stat")).unwrap_or_default();
    let stat_fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    // Field 23 is `vsize`; after the `)` the words begin at field 3.
    let vsize: Option<u64> = stat_fields.get(20).and_then(|word| word.parse().ok());
    report.check(
        "statm's size is stat's vsize in pages",
        vsize.is_some_and(|bytes| bytes / 4096 == statm_of(me).unwrap_or(base)[0]),
    );

    // --- a zombie is seven zeros ------------------------------------------
    let mut go = [0i32; 2];
    let mut ready = [0i32; 2];
    // SAFETY: four ints this process owns.
    let made = unsafe { libc::pipe(go.as_mut_ptr()) == 0 && libc::pipe(ready.as_mut_ptr()) == 0 };
    if !made {
        report.check("a zombie's statm is seven zeros", false);
        report.finish();
    }
    // SAFETY: `fork` in a single-threaded test; the child only moves a byte
    // down a pipe and exits.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: no Rust runs after this point in the child.
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
    // SAFETY: the pipes this function made.
    unsafe {
        libc::close(go[0]);
        libc::close(ready[1]);
        let mut sink: u8 = 0;
        libc::read(ready[0], (&mut sink as *mut u8).cast(), 1);
        libc::close(ready[0]);
        let byte: u8 = 1;
        libc::write(go[1], (&byte as *const u8).cast(), 1);
        libc::close(go[1]);
    }
    // Wait for the state rather than for the file, which existed all along.
    let mut zombie = None;
    for _ in 0..200 {
        if status_of(pid)
            .lines()
            .any(|line| line == "State:\tZ (zombie)")
        {
            zombie = statm_of(pid);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    report.check(
        "a zombie's statm is seven zeros",
        zombie.is_some_and(|fields| fields == [0u64; 7]),
    );
    // SAFETY: the child this function forked.
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };

    report.finish();
}
