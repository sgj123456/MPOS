//! The thin libc: one module, included by every user program out of this
//! directory (`#[path = "../lib/sys.rs"] mod sys;`), the same way they include
//! the kernel's syscall table. No allocator, no std: fixed buffers and raw
//! bytes, which is all a program this small needs.
//!
//! The entry stub lives here. The kernel starts every process with the
//! argument block at the bottom of its stack - `argc` at `[rsp]`, then the
//! `argv` pointers - but a Rust function's prologue moves the stack before any
//! code of ours runs, so `_start` is a few instructions of assembly: it lifts
//! `argc` and `argv` into the argument registers and calls
//! `mpos_user_main(argc, argv)`, which every program defines.

use core::arch::{asm, global_asm};

#[allow(dead_code)]
#[path = "../../src/syscall/nr.rs"]
pub mod nr;

global_asm!(
    // Process entry. The kernel's `iretq` lands here with the argument block
    // at [rsp]; read it before anything else runs, then call into Rust. The
    // trailing `ud2` is unreachable: mpos_user_main is `-> !`.
    ".globl _start",
    ".p2align 4",
    "_start:",
    "    xor ebp, ebp",
    "    mov rdi, [rsp]",
    "    lea rsi, [rsp + 8]",
    "    call mpos_user_main",
    "    ud2",
);

/// The gate's register convention: number in `rax`, arguments in
/// `rdi, rsi, rdx, r10`, result in `rax`. The kernel side preserves every
/// register but `rax`, so nothing needs naming as clobbered.
fn sys0(number: u64) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number as isize => ret,
        );
    }
    ret
}

fn sys1(number: u64, a1: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
        );
    }
    ret
}

fn sys2(number: u64, a1: usize, a2: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
        );
    }
    ret
}

fn sys3(number: u64, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
        );
    }
    ret
}

fn sys4(number: u64, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
        );
    }
    ret
}

/// The process arguments, read out of the block the kernel laid on the stack.
pub struct Args {
    pub argc: usize,
    pub argv: *const *const u8,
}

impl Args {
    /// Argument `i` as bytes, without its terminator.
    ///
    /// The strings sit above the initial stack pointer, where no call frame
    /// ever reaches them, so the slice stays valid for the life of the
    /// process.
    pub fn get(&self, i: usize) -> &[u8] {
        assert!(i < self.argc, "argv[{i}] is past the end");
        unsafe {
            let start = *self.argv.add(i);
            let mut len = 0usize;
            while *start.add(len) != 0 {
                len += 1;
            }
            core::slice::from_raw_parts(start, len)
        }
    }
}

pub fn write(fd: i32, bytes: &[u8]) -> isize {
    sys3(
        nr::WRITE,
        fd as usize,
        bytes.as_ptr() as usize,
        bytes.len(),
    )
}

/// Write every byte, in as many syscalls as it takes. Positive is bytes
/// written; negative is the first error, when nothing at all went out.
pub fn write_all(fd: i32, mut bytes: &[u8]) -> isize {
    let mut total = 0isize;
    while !bytes.is_empty() {
        let n = write(fd, bytes);
        if n <= 0 {
            return if total == 0 { n } else { total };
        }
        total += n;
        bytes = &bytes[n as usize..];
    }
    total
}

pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    sys3(nr::READ, fd as usize, buf.as_mut_ptr() as usize, buf.len())
}

/// Open a path, read-only. The kernel takes the length, so the path needs no
/// terminator.
pub fn open(path: &[u8]) -> isize {
    sys3(nr::OPEN, path.as_ptr() as usize, path.len(), 0)
}

pub fn close(fd: i32) -> isize {
    sys1(nr::CLOSE, fd as usize)
}

/// Make `new` name what `old` names, closing whatever `new` named first.
/// Returns `new`, or a negative errno - EBADF when `old` names nothing.
pub fn dup2(old: i32, new: i32) -> isize {
    sys2(nr::DUP2, old as usize, new as usize)
}

/// This process's id. Init uses it to say who it is; nothing else needs it.
pub fn getpid() -> isize {
    sys0(nr::GETPID)
}

/// Run the program at `path` with `args` - the full argument vector, argv[0]
/// being the program's name. Returns the child's pid, or a negative errno.
///
/// The arguments cross as one NUL-separated blob in a fixed buffer; the
/// kernel's cap is 512 bytes of blob and 16 strings.
pub fn spawn(path: &[u8], args: &[&[u8]]) -> isize {
    const BLOB_MAX: usize = 512;
    let mut blob = [0u8; BLOB_MAX];
    let mut n = 0usize;
    for arg in args {
        if n + arg.len() + 1 > BLOB_MAX {
            return -22; // EINVAL: the arguments are more than the kernel takes
        }
        blob[n..n + arg.len()].copy_from_slice(arg);
        n += arg.len();
        blob[n] = 0;
        n += 1;
    }
    sys4(
        nr::SPAWN,
        path.as_ptr() as usize,
        path.len(),
        blob.as_ptr() as usize,
        n,
    )
}

/// Block until the child `pid` exits, and take its exit code. Negative on
/// error - ECHILD for a pid that is not a child of ours.
pub fn wait(pid: isize) -> isize {
    sys1(nr::WAIT, pid as usize)
}

/// List a directory into `buf`: per entry one kind byte (nr::kind), the name,
/// a NUL. A zero-length `buf` only asks how big the buffer must be; a too
/// small one is EINVAL.
pub fn listdir(path: &[u8], buf: &mut [u8]) -> isize {
    sys4(
        nr::LISTDIR,
        path.as_ptr() as usize,
        path.len(),
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
}

pub fn sleep_ms(ms: usize) -> isize {
    sys1(nr::SLEEP, ms)
}

pub fn exit(code: i32) -> ! {
    sys1(nr::EXIT, code as usize);
    unreachable!("EXIT never returns")
}

/// What a panicking program does instead of unwinding: say so, and die.
pub fn abort() -> ! {
    let _ = write_all(2, b"panicked\n");
    exit(101)
}
