//! t-aslr - a layout every process is given, drawn every time.
//!
//! Address-space randomisation is not something a program can notice from the
//! inside of one run: within a process everything is exactly where the kernel
//! put it, and the kernel put it there once. So this test is about two runs of
//! the *same* program, and it gets them the only way available to a program
//! that cannot assume a shell, a filesystem or a runner that passes arguments:
//! it forks, execs itself in a reporting mode, and reads back what the two
//! fresh processes found themselves at.
//!
//! What Linux randomises was measured rather than remembered, and it is three
//! landmarks, not four: where a position-independent image is loaded, where its
//! stack starts, and where its anonymous `mmap` region begins. A binary linked
//! at a fixed address keeps its code address through every exec - over 64 runs
//! of one on Linux it came back `0x401156` each time, while its stack and its
//! mapped page moved - because the absolute addresses its own instructions
//! carry were baked in at link time, so moving the image would break the
//! program rather than protect it. The *unchanging* code address below is
//! therefore not a gap in this kernel's ASLR. It is the ABI, asserted as such.
//!
//! What is not tested here is the load bias of a position-independent image,
//! and that omission is deliberate rather than a gap in the suite: every program
//! in this initramfs is `ET_EXEC`, so nothing the test can exec has an address
//! the kernel is allowed to choose. That path is covered by the desktop image,
//! where every binary is dynamic and the whole machine fails to start if
//! `AT_PHDR` and `AT_ENTRY` are not moved along with the pages they describe.
//!
//! Nor does anything redraw a layout on `fork`, which is the property the two
//! samples above rest on: a child that inherited its parent's pages at addresses
//! of its own had would not have inherited its parent's pages at all.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The word size of a sample: code, stack, mapping, heap.
const FIELDS: usize = 4;
/// Longest `argv[0]` this test will follow. A longer one is refused out loud
/// rather than truncated into a path for a different program.
const PATH_MAX: usize = 128;

/// Where one run of this program found itself.
struct Sample {
    code: usize,
    stack: usize,
    mmap: usize,
    heap: usize,
}

impl Sample {
    fn fields(&self) -> [usize; FIELDS] {
        [self.code, self.stack, self.mmap, self.heap]
    }
}

/// Look at the running program from the outside in. Every address here is one an
/// information leak would hand to an attacker: the address of a function that is
/// executing, of a local in the frame above it, of a page just asked for, of the
/// end of the image the heap grows from.
fn take_sample() -> Sample {
    let local = 0x5a5a_isize;
    let mmap = lin::mmap(
        0,
        4096,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS,
        -1,
        0,
    );
    Sample {
        code: take_sample as *const () as usize,
        stack: core::ptr::addr_of!(local) as usize,
        mmap: if mmap < 0 { 0 } else { mmap as usize },
        heap: lin::brk(0).max(0) as usize,
    }
}

fn hex(bytes: &mut [u8], value: usize) -> usize {
    let mut digits = [0u8; 16];
    let mut k = 0usize;
    let mut n = value;
    loop {
        let digit = (n & 0xf) as u8;
        digits[k] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        k += 1;
        n >>= 4;
        if n == 0 {
            break;
        }
    }
    let mut at = 0usize;
    while k > 0 {
        k -= 1;
        bytes[at] = digits[k];
        at += 1;
    }
    at
}

fn unhex(bytes: &[u8]) -> Option<usize> {
    let mut value = 0usize;
    for &byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit as usize)?;
    }
    (!bytes.is_empty()).then_some(value)
}

/// The four numbers, one per line, into `path`. False on any part of that
/// failing: a sample that was never written must not read as a sample whose
/// addresses stayed put.
fn write_sample(path: &[u8], sample: &Sample) -> bool {
    let mut text = [0u8; FIELDS * 17];
    let mut at = 0usize;
    for value in sample.fields() {
        let end = at + hex(&mut text[at..at + 16], value);
        text[end] = b'\n';
        at = end + 1;
    }
    // O_WRONLY | O_CREAT | O_TRUNC.
    let fd = lin::openat(lin::AT_FDCWD, path, 1 | 0o100 | 0o1000, 0o644);
    if fd < 0 {
        return false;
    }
    let wrote = lin::write(fd as i32, &text[..at]) as usize == at;
    let _ = lin::close(fd as i32);
    wrote
}

/// Read them back. `None` covers every way the file could fail to be a sample:
/// absent, unreadable, short, or holding something that is not a number.
fn read_sample(path: &[u8]) -> Option<Sample> {
    let fd = lin::openat(lin::AT_FDCWD, path, 0 /* O_RDONLY */, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; FIELDS * 17];
    let n = lin::read(fd as i32, &mut buf);
    let _ = lin::close(fd as i32);
    if n <= 0 {
        return None;
    }
    let mut got = [0usize; FIELDS];
    let mut index = 0usize;
    for line in buf[..n as usize].split(|&byte| byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if index == FIELDS {
            return None;
        }
        got[index] = unhex(line)?;
        index += 1;
    }
    (index == FIELDS).then_some(Sample {
        code: got[0],
        stack: got[1],
        mmap: got[2],
        heap: got[3],
    })
}

/// Copy a NUL-terminated argument into `buf` and hand back the copy, terminator
/// included - which is the form both `execve`'s vectors and a path argument
/// want, and the form that keeps the caller from having to trust a length it
/// cannot see. `None` for an empty or over-long argument, since following a
/// pointer past a buffer this test does not own is not a thing it should do
/// quietly.
fn arg(ptr: *const u8, buf: &mut [u8; PATH_MAX + 1]) -> Option<&[u8]> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while len <= PATH_MAX && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    if len == 0 || len == PATH_MAX {
        return None;
    }
    buf[..len].copy_from_slice(unsafe { core::slice::from_raw_parts(ptr, len) });
    buf[len] = 0;
    Some(&buf[..len + 1])
}

/// One fresh process, asked where it is, collected again.
fn redraw(me: &[u8], where_to: &[u8], out: &mut Option<Sample>) -> bool {
    let pid = lin::fork();
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        // The child half. `execve` only returns when it failed, and a child that
        // fell through after failing would run the parent's checks from a
        // child's body - which reports as a layout that differs from itself.
        static MODE: &[u8] = b"sample\0";
        let argv: [*const u8; 4] = [
            me.as_ptr(),
            MODE.as_ptr(),
            where_to.as_ptr(),
            core::ptr::null(),
        ];
        let env: [*const u8; 1] = [core::ptr::null()];
        let rc = lin::execve(me.as_ptr(), argv.as_ptr(), env.as_ptr());
        lin::puts(b"FAIL: could not exec myself again (rc=");
        lin::puts_isize(rc);
        lin::puts(b")\n");
        lin::exit(1);
    }
    let mut status = 0i32;
    let _ = lin::waitpid(pid as usize, &mut status, 0);
    *out = read_sample(where_to);
    out.is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(argc: i32, argv: *const *const u8) -> ! {
    // The child half's whole job: report and leave. Two arguments say so - the
    // word `sample` and the file to write.
    if argc >= 3 && !argv.is_null() {
        let mut mode_buf = [0u8; PATH_MAX + 1];
        let mut file_buf = [0u8; PATH_MAX + 1];
        let mode = arg(unsafe { *argv.add(1) }, &mut mode_buf);
        let file = arg(unsafe { *argv.add(2) }, &mut file_buf);
        if mode == Some(b"sample\0") {
            let Some(path) = file else {
                lin::die(b"t-aslr: asked to sample with no file\n");
            };
            let sample = take_sample();
            if !write_sample(path, &sample) {
                lin::puts(b"FAIL: the sample could not be written\n");
                lin::done(true);
            }
            lin::exit(0);
        }
    }

    lin::puts(b"t-aslr\n");
    let mut failed = false;

    if argv.is_null() {
        lin::puts(b"FAIL: no argument vector at all\n");
        lin::done(true);
    }
    let mut me_buf = [0u8; PATH_MAX + 1];
    let Some(me) = arg(unsafe { *argv }, &mut me_buf) else {
        lin::puts(b"FAIL: no argv[0] this test can exec itself by\n");
        lin::done(true);
    };
    // `me` borrows `me_buf`, and the fork below copies both of them into the
    // child, which is the only reason a borrowed path can be used across it.
    let mut first_buf = [0u8; PATH_MAX + 1];
    let mut second_buf = [0u8; PATH_MAX + 1];
    let first_path = match arg(b"/tmp/t-aslr.1\0".as_ptr(), &mut first_buf) {
        Some(path) => path,
        None => lin::die(b"t-aslr: sample path\n"),
    };
    let second_path = match arg(b"/tmp/t-aslr.2\0".as_ptr(), &mut second_buf) {
        Some(path) => path,
        None => lin::die(b"t-aslr: sample path\n"),
    };
    let _ = lin::unlinkat(lin::AT_FDCWD, first_path, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, second_path, 0);

    let mut first = None;
    let mut second = None;
    let ok_first = redraw(me, first_path, &mut first);
    let ok_second = redraw(me, second_path, &mut second);
    lin::check_true(
        &mut failed,
        b"two fresh runs of this program reported where each one was",
        ok_first && ok_second,
    );
    let (Some(a), Some(b)) = (first, second) else {
        lin::done(true);
    };

    // Both landmarks that are free to choose, chose. The comparison is of page
    // numbers because that is what the kernel draws at: the offset *inside* a
    // page belongs to the program - how much stack it had used when it looked -
    // and would differ between two runs even with no randomisation whatsoever.
    lin::check_true(
        &mut failed,
        b"the stack does not start at the same page twice",
        a.stack >> 12 != b.stack >> 12,
    );
    lin::check_true(
        &mut failed,
        b"and neither does the mmap region",
        a.mmap != 0 && b.mmap != 0 && a.mmap >> 12 != b.mmap >> 12,
    );
    // And the one that must not move, did not: this image was linked for the
    // addresses it names, so a kernel that shuffled them would have produced a
    // process that cannot run rather than a harder one to attack.
    lin::check_eq(
        &mut failed,
        b"a fixed-address image keeps its code address - that is what fixed means",
        (a.code >> 12) as isize,
        (b.code >> 12) as isize,
    );
    // The heap begins just above the image, so on this one address it stays
    // with it. The pair of the two checks above is the point: the region a
    // program *asks* for is drawn, the region it was *linked into* is not.
    lin::check_eq(
        &mut failed,
        b"and its heap anchor stays with its image",
        (a.heap >> 12) as isize,
        (b.heap >> 12) as isize,
    );
    // The sample files are the test's own litter.
    let _ = lin::unlinkat(lin::AT_FDCWD, first_path, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, second_path, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-aslr: panicked\n")
}
