//! `/proc/<pid>/auxv` is the vector the program was handed, in the bytes its
//! initial stack carries.
//!
//! Measured on a current host first (Linux 7.2.14, `build/scratch/auxv/`):
//!
//! ```text
//! mode 0400  size 0
//! read 368 bytes = 23 entries
//!   type 3  value 0x556eebed9040     (AT_PHDR)
//!   type 6  value 0x1000             (AT_PAGESZ)
//!   type 17 value 0x64               (AT_CLKTCK)
//!   type 9  value 0x556eebeda0b0     (AT_ENTRY)
//!   type 11 value 0x3e8              (AT_UID)
//!   type 25 value 0x7fffca0af9e9     (AT_RANDOM)
//!   type 31 value 0x7fffca0b1ff4     (AT_EXECFN)
//!   type 0  value 0x0                (AT_NULL)
//! getauxval: PHDR/ENTRY/PAGESZ/CLKTCK/UID all match the file
//! ```
//!
//! Three rules come out of it, and each is a place a plausible kernel gets it
//! wrong:
//!
//! * **The file is a read, not a rebuild.** Linux keeps one copy per address
//!   space (`mm->saved_auxv`), made when the program was executed, and a
//!   `setuid` afterwards does not move the ids it reports. A kernel that
//!   recomputed the vector on the read would answer the *current* ids - a
//!   different file with the same shape.
//! * **The words are the ones on the stack.** `AT_EXECFN` points at the path
//!   string the loader pushed, so the pointer can be followed back into this
//!   process's own memory; `AT_RANDOM` likewise points at sixteen bytes.
//! * **`st_size` is zero and the mode is `0400`.** The length is only known
//!   when the file is read, and the vector carries the ids the process was
//!   executed with.
//!
//! And two spellings, both answered from the same copy: `/proc/<pid>/auxv` and
//! `/proc/<pid>/task/<tid>/auxv`. A `fork` child, which runs the same program
//! from the same stack, reads its parent's bytes; a child that `exec`s gets a
//! new vector, and its `AT_EXECFN` names the path it was executed by
//! (`build/scratch/auxv/q.c`).

use mpos_user_tests::Report;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_CLKTCK: u64 = 17;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// The env var naming the pipe a re-executed child writes its `AT_EXECFN` to.
const EXEC_FD: &str = "MPOS_AUXV_EXEC_FD";

fn fd_from_env(key: &str) -> Option<i32> {
    std::env::var(key).ok()?.parse().ok()
}

fn bytes_of(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// The body as `(type, value)` pairs, or `None` when it is not a whole number
/// of them.
fn pairs(bytes: &[u8]) -> Option<Vec<(u64, u64)>> {
    if bytes.is_empty() || bytes.len() % 16 != 0 {
        return None;
    }
    let mut out = Vec::new();
    for chunk in bytes.chunks_exact(16) {
        let tag = u64::from_ne_bytes(chunk[..8].try_into().ok()?);
        let value = u64::from_ne_bytes(chunk[8..].try_into().ok()?);
        out.push((tag, value));
    }
    Some(out)
}

fn value_of(entries: &[(u64, u64)], tag: u64) -> Option<u64> {
    entries.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v)
}

/// Whether `addr` falls inside a readable, or executable, mapping of this
/// process.
fn mapped(addr: u64, executable: bool) -> bool {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return false;
    };
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
            continue;
        };
        let wanted = if executable {
            perms.split_at(2).1.starts_with('x')
        } else {
            perms.starts_with('r')
        };
        if !wanted {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        if addr >= start && addr < end {
            return true;
        }
    }
    false
}

/// The NUL-terminated string at `addr`, if `addr` is inside a readable mapping
/// - so a bogus word fails the claim instead of killing the test.
fn string_at(addr: u64) -> Option<String> {
    if !mapped(addr, false) {
        return None;
    }
    let mut bytes = Vec::new();
    // SAFETY: the address is inside a readable mapping of this process, and the
    // walk stops at a NUL or after 256 bytes.
    unsafe {
        let mut at = addr as *const u8;
        for _ in 0..256 {
            let byte = *at;
            if byte == 0 {
                break;
            }
            bytes.push(byte);
            at = at.add(1);
        }
    }
    String::from_utf8(bytes).ok()
}

/// A re-executed child's own answer, on the pipe it was given.
fn child_main() -> ! {
    let Some(fd) = fd_from_env(EXEC_FD) else {
        std::process::exit(1);
    };
    let entries = bytes_of("/proc/self/auxv")
        .and_then(|b| pairs(&b))
        .unwrap_or_default();
    let execfn = value_of(&entries, AT_EXECFN)
        .and_then(string_at)
        .unwrap_or_default();
    let line = format!("{execfn}\n");
    unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
    std::process::exit(0);
}

fn main() {
    if std::env::var_os(EXEC_FD).is_some() {
        child_main();
    }
    let mut report = Report::new();

    // ---- the body's shape ----
    let body = bytes_of("/proc/self/auxv");
    report.check(
        &format!(
            "the auxv file exists and is not empty ({:?} bytes)",
            body.as_ref().map(|b| b.len())
        ),
        body.as_ref().is_some_and(|b| !b.is_empty()),
    );
    let entries = body.as_deref().and_then(pairs);
    report.check(
        &format!(
            "its body is whole (u64, u64) pairs ({:?} entries)",
            entries.as_ref().map(|e| e.len())
        ),
        entries.is_some(),
    );
    let entries = entries.unwrap_or_default();
    let last = entries.last().copied();
    report.check(
        &format!("and it ends with the AT_NULL terminator ({last:?})"),
        last == Some((AT_NULL, 0)),
    );
    report.check(
        &format!(
            "and nothing follows the terminator ({:?})",
            entries.iter().position(|(t, _)| *t == AT_NULL)
        ),
        entries.iter().position(|(t, _)| *t == AT_NULL) == Some(entries.len() - 1),
    );

    // ---- the words are this program's ----
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let pagesz = value_of(&entries, AT_PAGESZ);
    report.check(
        &format!("AT_PAGESZ is the page size ({pagesz:?} against {page})"),
        pagesz == Some(page as u64),
    );
    let clktck = value_of(&entries, AT_CLKTCK);
    report.check(
        &format!("AT_CLKTCK is 100 ({clktck:?})"),
        clktck == Some(100),
    );
    let ids: Vec<Option<u64>> = [AT_UID, AT_EUID, AT_GID, AT_EGID]
        .iter()
        .map(|tag| value_of(&entries, *tag))
        .collect();
    let mine = unsafe {
        [
            libc::getuid() as u64,
            libc::geteuid() as u64,
            libc::getgid() as u64,
            libc::getegid() as u64,
        ]
    };
    report.check(
        &format!("the four id words are this process's ({ids:?} against {mine:?})"),
        ids.iter().zip(mine.iter()).all(|(a, b)| *a == Some(*b)),
    );

    // `AT_EXECFN` points at the path string the loader pushed. Following it is
    // what makes this the *stack's* vector rather than a table that happens to
    // have the right shape: the pointer has to land in this process's memory and
    // spell the program it is running.
    let execfn_word = value_of(&entries, AT_EXECFN);
    let execfn = execfn_word.and_then(string_at);
    let exe = std::fs::read_link("/proc/self/exe")
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    report.check(
        &format!("AT_EXECFN points at a readable path ({execfn_word:?} -> {execfn:?})"),
        execfn.as_ref().is_some_and(|s| !s.is_empty()),
    );
    // `AT_EXECFN` is the path *as it was spelled* to `execve`, not the resolved
    // one: Linux records `bprm->filename`, so a program started as
    // `./target/.../t-proc-auxv` reads exactly that back, while
    // `/proc/self/exe` gives the absolute path. The part that must agree is the
    // file's own name.
    let execfn_name = execfn
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .map(str::to_owned);
    let exe_name = exe
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .map(str::to_owned);
    report.check(
        &format!(
            "and the string it points at names this program ({execfn_name:?} against {exe_name:?})"
        ),
        execfn_name.is_some() && execfn_name == exe_name,
    );
    let random = value_of(&entries, AT_RANDOM);
    report.check(
        &format!("AT_RANDOM points at readable bytes ({random:?})"),
        random.is_some_and(|addr| mapped(addr, false)),
    );
    let entry = value_of(&entries, AT_ENTRY);
    let phdr = value_of(&entries, AT_PHDR);
    // `AT_ENTRY` is code, so it must land in an executable mapping. `AT_PHDR` is
    // the ELF header table, which Linux places in the image's first *read-only*
    // segment - asking for it to be executable would fail on a correct kernel.
    report.check(
        &format!("AT_ENTRY is executable code and AT_PHDR is readable ({entry:?}, {phdr:?})"),
        entry.is_some_and(|a| mapped(a, true)) && phdr.is_some_and(|a| mapped(a, false)),
    );

    // ---- the file's own word ----
    let meta = std::fs::metadata("/proc/self/auxv");
    let (mode, size) = meta
        .as_ref()
        .map(|m| (m.permissions().mode() & 0o777, m.len()))
        .unwrap_or((0, 0));
    report.check(
        &format!("the file is 0400 and reports no size ({mode:o}, {size})"),
        mode == 0o400 && size == 0,
    );

    // ---- two spellings, one copy ----
    let tid = unsafe { libc::gettid() };
    let thread_spelling = bytes_of(&format!("/proc/self/task/{tid}/auxv"));
    report.check(
        &format!(
            "the thread's spelling is the process's bytes ({:?} against {:?})",
            thread_spelling.as_ref().map(|b| b.len()),
            body.as_ref().map(|b| b.len())
        ),
        thread_spelling.is_some() && thread_spelling == body,
    );

    // ---- a fork child reads its parent's copy ----
    let forked = fork_reporting();
    report.check(
        &format!(
            "a fork child that has not exec'd reads these bytes ({:?} against {:?})",
            forked.as_ref().map(|b| b.len()),
            body.as_ref().map(|b| b.len())
        ),
        forked.is_some() && forked == body,
    );

    // ---- an exec makes a new vector ----
    let reexec = reexec_child();
    let want = "/proc/self/exe\n";
    report.check(
        &format!("a child executed by a path names that path in AT_EXECFN ({reexec:?})"),
        reexec.as_deref().map(|s| s.trim_end()) == Some(want.trim_end()),
    );
    report.check(
        &format!("and that is not the path this process was started by ({execfn:?})"),
        reexec
            .as_deref()
            .map(|s| s.trim_end().to_owned())
            .is_some_and(|s| Some(s) != execfn),
    );

    // ---- a dead process has no vector ----
    // Linux refuses with `ESRCH` because the `mm` is gone; this kernel reports
    // the same "no vector" a collected task does, which its sibling files spell
    // `ENOENT`. The claim is the refusal itself: a zombie must not answer with
    // bytes (`docs/notes/` §6.136's named gap).
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) };
    let dead = bytes_of(&format!("/proc/{child}/auxv"));
    report.check(
        &format!(
            "a collected process has no auxv ({:?})",
            dead.as_ref().map(|b| b.len())
        ),
        dead.is_none(),
    );

    report.finish();
}

/// Fork a child that reports its own vector's bytes on a pipe, and return them.
///
/// The child does not `exec`, so it is running the same program from the same
/// stack: Linux's `copy_mm` copies `saved_auxv` with the rest of the `mm`,
/// which makes its file the parent's bytes.
fn fork_reporting() -> Option<Vec<u8>> {
    let mut pipe = [0i32; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return None;
    }
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::close(pipe[0]) };
        let bytes = bytes_of("/proc/self/auxv").unwrap_or_default();
        unsafe { libc::write(pipe[1], bytes.as_ptr().cast(), bytes.len()) };
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::close(pipe[1]) };
    let mut out = Vec::new();
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(pipe[0]) };
    let _ = file.read_to_end(&mut out);
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    Some(out)
}

/// Fork a child that re-executes this program by `/proc/self/exe`, and return
/// the `AT_EXECFN` path it reads for itself.
fn reexec_child() -> Option<String> {
    let mut pipe = [0i32; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return None;
    }
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::close(pipe[0]) };
        let path = c"/proc/self/exe";
        let arg0 = c"/proc/self/exe";
        let key = format!("{EXEC_FD}={}", pipe[1]);
        let Ok(key) = std::ffi::CString::new(key) else {
            unsafe { libc::_exit(1) };
        };
        let argv = [arg0.as_ptr(), std::ptr::null()];
        let envp = [key.as_ptr(), std::ptr::null()];
        unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        unsafe { libc::_exit(1) };
    }
    unsafe { libc::close(pipe[1]) };
    let mut out = String::new();
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(pipe[0]) };
    let _ = file.read_to_string(&mut out);
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Some(out)
    } else {
        None
    }
}
