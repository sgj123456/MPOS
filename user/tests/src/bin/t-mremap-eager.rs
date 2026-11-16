//! t-mremap-eager - a page carried by `mremap` stays reachable from ring 3 **when
//! it already was**, including pages of an eager (VMA-less) mapping.
//!
//! Found from Chromium (`docs/notes/07-chromium-analysis.md`, finding A2): a
//! present PTE with `U=0` faulted a ring-3 *read* with `error 5` from inside
//! `ld-musl`'s SSE `memcpy`. The leaf was written from `MapFlags::READ|WRITE`
//! with no `USER`.
//!
//! The chain was measured, not guessed: `mremap` asks `sched::vma_prot_at` per
//! page whether it is reachable from ring 3, and re-maps the frame with `USER`
//! only when the answer is yes. An **eager** range (a `MAP_SHARED` file copy, an
//! image placed by `exec`) has no VMA, so `vma_prot_at` answered `None`, the page
//! looked unreachable, and the leaf lost `USER_ACCESSIBLE`. The process's own
//! readable memory became supervisor-only.
//!
//! Measured Linux behaviour this pins: `mremap` preserves a mapping's
//! accessibility. A source page readable before the move is readable after it,
//! and `PROT_NONE` stays `PROT_NONE`.

use mpos_user_tests::Report;

fn page() -> usize {
    // SAFETY: `sysconf` with a valid name.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}

fn main() {
    println!("t-mremap-eager");
    let mut r = Report::new();
    let ps = page();

    // === an **eager** (MAP_SHARED file) page survives mremap and stays readable ==
    //
    // This is the arm that reaches the defect: `MAP_SHARED` of an ordinary file is
    // copied in at `mmap` time and recorded as an eager range with **no VMA**
    // (`record_eager_range`'s own note). `mremap` asks `vma_prot_at` per page
    // whether it is reachable; with no VMA the answer used to be `None`, so the
    // page looked unreachable and its new leaf lost `USER_ACCESSIBLE`.
    //
    // A fixture file is written first, because an eager mapping needs a real file
    // this kernel can read: `/bin/busybox` is on a read-only root.
    let path = "/tmp/t-mremap-eager.bin";
    let made = std::fs::write(path, [0x5a_u8; 8192]).is_ok();
    r.check(&format!("a fixture file is written ({made})"), made);
    if !made {
        r.finish();
    }
    let fixture = std::ffi::CString::new(path).unwrap();
    // SAFETY: a path this test just made.
    let fd = unsafe { libc::open(fixture.as_ptr(), libc::O_RDWR) };
    r.check(&format!("it opens ({fd})"), fd >= 0);
    if fd >= 0 {
        // SAFETY: the descriptor just opened, `MAP_SHARED` so the eager arm runs.
        let shared = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                ps,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        r.check(
            "a MAP_SHARED file page maps (the eager arm)",
            shared != libc::MAP_FAILED,
        );
        if shared != libc::MAP_FAILED {
            // SAFETY: the mapping made above.
            let before = unsafe { core::ptr::read_volatile(shared as *const u8) };
            r.check(
                &format!("and is readable before the move ({before:#x})"),
                before == 0x5a,
            );
            // **Force the move.** `MREMAP_MAYMOVE` only *permits* relocation -
            // a page that can stay in place is left alone, and a first version
            // of this pin therefore passed against a disabled fix because no
            // page ever travelled. `MREMAP_FIXED` names the destination, so the
            // frame-carrying loop runs.
            // SAFETY: a reservation this test owns, made only to name a
            // destination that is free.
            let dest = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    ps,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            let moved = if dest == libc::MAP_FAILED {
                libc::MAP_FAILED
            } else {
                // SAFETY: `dest` is a reservation this test just made; MREMAP_FIXED
                // unmaps it and places the source there.
                unsafe {
                    libc::mremap(
                        shared,
                        ps,
                        ps,
                        libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED,
                        dest,
                    )
                }
            };
            r.check("the eager page mremaps", moved != libc::MAP_FAILED);
            if moved != libc::MAP_FAILED {
                // **The claim**: still readable from ring 3 at the new address.
                // A leaf written without USER_ACCESSIBLE faults here instead -
                // the measured Chromium failure.
                // SAFETY: the new mapping, one page.
                let after = unsafe { core::ptr::read_volatile(moved as *const u8) };
                r.check(
                    &format!("and STILL readable after it ({after:#x})"),
                    after == 0x5a,
                );
                // SAFETY: the moved mapping.
                unsafe { libc::munmap(moved, ps) };
            }
        }
        // SAFETY: the descriptor this block opened.
        unsafe { libc::close(fd) };
    }
    let _ = std::fs::remove_file(path);

    // === a PROT_NONE page stays PROT_NONE across mremap ====================
    //
    // The other half: `mremap` must NOT hand ring 3 a page the mapping said was
    // inaccessible. Both halves matter; a fix that always set `USER` would pass
    // the claim above and fail this one.
    // SAFETY: a fresh mapping this test owns.
    let guarded = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ps,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if guarded != libc::MAP_FAILED {
        // SAFETY: the mapping made above.
        unsafe {
            *(guarded as *mut u8) = 1;
            libc::mprotect(guarded, ps, libc::PROT_NONE);
        }
        // SAFETY: as above.
        let moved = unsafe { libc::mremap(guarded, ps, ps, libc::MREMAP_MAYMOVE) };
        if moved != libc::MAP_FAILED {
            // Read in a child: if the page wrongly became readable, the child
            // exits 0; if the mapping is still PROT_NONE it dies of SIGSEGV.
            // SAFETY: `fork` and the mapping are this test's.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // SAFETY: reading a page this mapping says is PROT_NONE.
                let v = unsafe { core::ptr::read_volatile(moved as *const u8) };
                unsafe { libc::_exit(if v == 1 { 0 } else { 2 }) };
            }
            let mut st = 0;
            // SAFETY: the child just forked.
            unsafe { libc::waitpid(pid, &mut st, 0) };
            let died = libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGSEGV;
            r.check("a PROT_NONE page stays unreachable after mremap", died);
            // SAFETY: the moved mapping.
            unsafe { libc::munmap(moved, ps) };
        }
    }

    // errno is read by `refused`-style helpers elsewhere; keep the import used.
    let _ = errno();
    r.finish()
}
