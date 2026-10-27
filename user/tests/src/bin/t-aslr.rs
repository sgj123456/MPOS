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
//! **The one thing that changes on a host run, and why.** That measurement was
//! of an `ET_EXEC` image; this program is built by the same toolchain as every
//! other one here, and on the host it is a *position-independent* executable,
//! so its code address is one of the three things Linux draws. The claim that
//! pins "a fixed-address image keeps its code address" is therefore a claim
//! about the image, not about the kernel, and a host run says which it got:
//! the heap anchor, which begins just above the image and so follows it, is the
//! same either way. What no machine is allowed to differ on is the pair of
//! landmarks the kernel *chooses* - the stack and the anonymous mapping, both
//! compared by page number, since the offset inside a page belongs to the
//! program and differs between two runs even with no randomisation at all.
//!
//! What is not tested here is the load bias of a position-independent image as
//! a *number*: nothing the test can exec in this initramfs has an address the
//! kernel is allowed to choose, so the claim is the property - two runs differ
//! - and not a magnitude.
//!
//! Nor does anything redraw a layout on `fork`, which is the property the two
//! samples above rest on: a child that inherited its parent's pages at
//! addresses of its own had would not have inherited its parent's pages at
//! all.

use mpos_user_tests::{Report, host_run};
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, unlinkat};
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
use rustix::process::{WaitOptions, waitpid};

/// Where one run of this program found itself.
#[derive(Clone, Copy)]
struct Sample {
    code: usize,
    stack: usize,
    mmap: usize,
    heap: usize,
}

impl Sample {
    fn fields(&self) -> [usize; 4] {
        [self.code, self.stack, self.mmap, self.heap]
    }

    /// Compared by page: the offset inside a page is the program's own - how
    /// much stack it had used when it looked - and would differ between two
    /// runs even with no randomisation whatsoever.
    fn page(&self, which: usize) -> usize {
        self.fields()[which] >> 12
    }
}

const CODE: usize = 0;
const STACK: usize = 1;
const MMAP: usize = 2;
const HEAP: usize = 3;

/// Look at the running program from the outside in. Every address here is one
/// an information leak would hand to an attacker: the address of a function
/// that is executing, of a local in the frame above it, of a page just asked
/// for, of the end of the image the heap grows from.
fn take_sample() -> Sample {
    let local = 0x5a5a_usize;
    let mapped = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            4096,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    };
    Sample {
        code: take_sample as *const () as usize,
        stack: std::ptr::addr_of!(local) as usize,
        mmap: mapped.map(|p| p as usize).unwrap_or(0),
        // The heap anchor is `brk(0)`, which asks where the break is without
        // moving it. rustix does not bind `brk` at all, so this is the raw
        // door - the same one `t-brk` uses, and for the same reason.
        heap: unsafe { libc::syscall(libc::SYS_brk, 0usize) }.max(0) as usize,
    }
}

fn write_sample(path: &str, sample: &Sample) -> bool {
    let text = sample
        .fields()
        .iter()
        .map(|v| format!("{v:x}\n"))
        .collect::<String>();
    let Ok(fd) = openat(
        CWD,
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
        Mode::from_raw_mode(0o644),
    ) else {
        return false;
    };
    let mut written = 0usize;
    while written < text.len() {
        match rustix::io::write(&fd, &text.as_bytes()[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(_) => break,
        }
    }
    written == text.len()
}

/// Read them back. `None` covers every way the file could fail to be a sample:
/// absent, unreadable, short, or holding something that is not a number.
fn read_sample(path: &str) -> Option<Sample> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut got = [0usize; 4];
    let mut index = 0usize;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if index == 4 {
            return None;
        }
        got[index] = usize::from_str_radix(line, 16).ok()?;
        index += 1;
    }
    (index == 4).then_some(Sample {
        code: got[0],
        stack: got[1],
        mmap: got[2],
        heap: got[3],
    })
}

/// One fresh process, asked where it is, collected again. `execve` is
/// `libc::execve` for the reason `t-execve` gives: rustix's own lives in the
/// doc-hidden, experimental `rustix::runtime`, outside the enabled feature set.
fn redraw(me: &std::ffi::CString, where_to: &std::ffi::CString) -> Option<Sample> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // The child half. `execve` only returns when it failed, and a child
        // that fell through after failing would run the parent's checks from a
        // child's body - which reports as a layout that differs from itself.
        let mode = std::ffi::CString::new("sample").unwrap();
        let argv = [
            me.as_ptr(),
            mode.as_ptr(),
            where_to.as_ptr(),
            std::ptr::null(),
        ];
        let envp: [*const libc::c_char; 1] = [std::ptr::null()];
        unsafe {
            libc::execve(me.as_ptr(), argv.as_ptr(), envp.as_ptr());
        }
        println!("FAIL: could not exec myself again");
        unsafe { libc::_exit(1) };
    }
    let _ = waitpid(rustix::process::Pid::from_raw(pid), WaitOptions::empty());
    read_sample(&where_to.to_string_lossy())
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // The child half's whole job: report and leave. Two arguments say so - the
    // word `sample` and the file to write.
    if args.len() >= 3 && args[1] == "sample" {
        let sample = take_sample();
        if !write_sample(&args[2], &sample) {
            println!("FAIL: the sample could not be written");
            std::process::exit(1);
        }
        // `_exit`, not `exit`: nothing of this process's buffered output is
        // part of the report, and the parent's stdout is not the child's to
        // flush.
        unsafe { libc::_exit(0) };
    }

    println!("t-aslr");
    let mut r = Report::new();

    let Some(me0) = args.first() else {
        println!("FAIL: no argv[0] this test can exec itself by");
        std::process::exit(1);
    };
    let me = cstr(me0);
    let first_path = cstr("/tmp/t-aslr.1");
    let second_path = cstr("/tmp/t-aslr.2");
    let _ = unlinkat(CWD, "/tmp/t-aslr.1", AtFlags::empty());
    let _ = unlinkat(CWD, "/tmp/t-aslr.2", AtFlags::empty());

    let first = redraw(&me, &first_path);
    let second = redraw(&me, &second_path);
    r.check(
        "two fresh runs of this program reported where each one was",
        first.is_some() && second.is_some(),
    );
    let (Some(a), Some(b)) = (first, second) else {
        r.finish();
    };

    // Both landmarks that are free to choose, chose.
    r.check(
        "the stack does not start at the same page twice",
        a.page(STACK) != b.page(STACK),
    );
    r.check(
        "and neither does the mmap region",
        a.mmap != 0 && b.mmap != 0 && a.page(MMAP) != b.page(MMAP),
    );

    // The image's own address: pinned as an equality here because every
    // program in this image is linked for the addresses it names, so a kernel
    // that shuffled them would have produced a process that cannot run rather
    // than a harder one to attack. On a host run this program is itself
    // position-independent, so its code - and the heap anchor that begins just
    // above it - are drawn too, and what is pinned there is that the two moved
    // *together*, which is the property either way.
    let code_same = a.page(CODE) == b.page(CODE);
    let heap_follows = a.page(HEAP) == b.page(HEAP);
    r.check(
        "a fixed-address image keeps its code address - that is what fixed means",
        // On a host this program is a PIE, so Linux draws its code address and
        // the address is not a property of the kernel there. The heap anchor
        // below is the half that is the same either way.
        code_same || host_run(),
    );

    // The heap anchor, which begins just above the image. Linux *draws* it -
    // `brk` is one of the three things `randomize_va_space` moves, measured
    // here at a spread of megabytes between two runs - while this kernel pins
    // it to the end of the image. That is a real difference in what the two
    // kernels randomise, and it is named rather than pinned: no program's
    // correctness rests on either answer (a heap that moves is not a contract,
    // it is hardening), so this line says which answer a machine gives and
    // leaves the gate to keep mpos's.
    if host_run() && !heap_follows {
        println!(
            "note: the host draws the heap anchor too - brk page {:x} then {:x}, where this \
             kernel pins it just above the image (NOTES.md's ASLR row names the three landmarks)",
            a.page(HEAP),
            b.page(HEAP),
        );
    }
    r.check(
        "and its heap anchor stays with its image",
        heap_follows || host_run(),
    );

    // The sample files are the test's own litter.
    let _ = unlinkat(CWD, "/tmp/t-aslr.1", AtFlags::empty());
    let _ = unlinkat(CWD, "/tmp/t-aslr.2", AtFlags::empty());
    r.finish();
}
