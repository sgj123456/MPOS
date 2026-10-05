//! t-lazy-buffer - does a syscall that *fills* a caller's buffer take a buffer
//! the program has mapped and not yet written?
//!
//! This is the shape every `read` into a fresh buffer has, and it is where two
//! halves of the kernel have to agree. The range check that runs before the copy
//! must accept a page with no page-table entry when a mapping backs it, and the
//! machine's fault handler must take the fault the copy then raises - on the
//! *kernel's* behalf, because it is the kernel writing, not the program.
//!
//! Measured on the second machine, where both halves were missing: a shell's very
//! first `read(0)`, into its `BufReader`'s 8 KiB heap buffer, answered `EFAULT` -
//! the second page of a freshly allocated buffer has no table entry yet, and the
//! check refused it (`PLAN.md` §13.9). The same check then refused the buffer a
//! `fork` child reads into, because a shared page is copy-on-write and therefore
//! mapped read-only.
//!
//! Both cases are exercised here over memory whose state is certain rather than
//! allocator-dependent:
//!
//! * an anonymous `mmap` of a megabyte, which this kernel maps lazily - no page
//!   of it exists until something touches it - read into whole;
//! * a buffer one page of which the parent writes before `fork`, so that page is
//!   present *and* copy-on-write in the child while the page beside it is absent.
//!   One `read` then covers both rules, and the parent's own byte is checked
//!   afterwards, because a split that was not a split would show up there.
//!
//! It runs on both machines, and it is not a test of one machine: it is a test of
//! the rule that a user pointer names memory the process *has*, not memory the
//! process has already touched.

use std::io::{Read, Write};

/// The lazy region's length: 256 pages, so the range check walks a run of absent
/// pages rather than one or two that a neighbouring allocation could have
/// faulted in.
const LEN: usize = 1 << 20;

fn main() {
    let mut failed = false;
    println!("t-lazy-buffer: filling buffers the program has mapped and not written");

    let path = "/tmp/lazy-buffer.txt";
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    match std::fs::write(path, &payload) {
        Ok(()) => println!("ok:   wrote {} bytes to {path}", payload.len()),
        Err(e) => {
            println!("FAIL: could not write {path}: {e}");
            failed = true;
            finish(failed);
        }
    }

    // 1. A lazily mapped region, read into whole.
    //
    // SAFETY: `mmap` with a null hint is the kernel's to place, and the flags ask
    // for private anonymous read/write memory - the same call `malloc` makes for a
    // large allocation, made here so the region's state is known rather than
    // assumed. Nothing is written before the `read`, which is the point.
    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            LEN,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if region == libc::MAP_FAILED {
        println!(
            "FAIL: mmap of {LEN} bytes: {}",
            std::io::Error::last_os_error()
        );
        failed = true;
    } else {
        // SAFETY: `region` is `LEN` bytes of readable, writable, mapped memory,
        // and `i8` is the byte `mmap` returns.
        let buf: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(region as *mut u8, LEN) };
        // Deliberately no scan of `buf` before the read: touching it is exactly
        // what would fault the pages in and leave nothing to check. The mapping's
        // contents are verified *after* - the file's bytes, and zeros past them.
        match std::fs::File::open(path).and_then(|mut f| f.read(buf)) {
            Ok(n) if n == payload.len() && buf[..n] == payload[..] && buf[n] == 0 => {
                println!("ok:   read {n} bytes into a {LEN}-byte mapping nothing had touched");
            }
            Ok(n) => {
                println!("FAIL: read answered {n} bytes, and the bytes are not the file's");
                failed = true;
            }
            Err(e) => {
                println!("FAIL: reading into the lazily mapped region: {e}");
                failed = true;
            }
        }
        // SAFETY: the region was mapped by the `mmap` above and is not used after.
        unsafe { libc::munmap(region, LEN) };
    }

    // 2. A page the parent wrote, so the child shares it copy-on-write.
    let mut shared = vec![0u8; 8192];
    shared[0] = 0xA1;
    // SAFETY: `fork` in a test that immediately goes on to `waitpid` the child,
    // and whose child only reads a file, writes its own private copy of `shared`
    // and `_exit`s - no allocation, no buffered output and no lock is held across
    // the call.
    match unsafe { libc::fork() } {
        0 => {
            // The child. Its copy of page 0 is present and read-only, and page 1
            // is absent; one `read` into the whole buffer needs both rules.
            let n = std::fs::File::open(path)
                .and_then(|mut f| f.read(&mut shared))
                .unwrap_or(0);
            let ok = n == payload.len() && shared[..n] == payload[..];
            // Writing the page the read just split proves the split landed on the
            // child's own frame rather than on the parent's.
            shared[0] = 0xB2;
            // SAFETY: `_exit` ends the child without running the parent's
            // destructors or flushing a buffer the parent owns.
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        pid if pid > 0 => {
            let mut status = 0i32;
            // SAFETY: `pid` is the child `fork` just returned, and `status` is a
            // live i32 for the call to fill.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited != pid {
                println!("FAIL: waitpid({pid}) returned {waited}");
                failed = true;
            } else if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                println!("FAIL: the fork child answered status {status:#x}");
                failed = true;
            } else {
                println!("ok:   a fork child read into a shared page and split it");
            }
            // The split is only real if the parent kept its own bytes.
            if shared[0] != 0xA1 {
                println!(
                    "FAIL: the parent's page changed under it: {:#04x}",
                    shared[0]
                );
                failed = true;
            } else {
                println!("ok:   the parent's own copy was left alone");
            }
        }
        _ => {
            println!("FAIL: fork: {}", std::io::Error::last_os_error());
            failed = true;
        }
    }

    let _ = std::fs::remove_file(path);
    finish(failed);
}

fn finish(failed: bool) -> ! {
    if failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
    std::process::exit(0);
}
