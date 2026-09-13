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

/// The `syscall` gate carries both ABIs on one entry, so mpos's own numbers
/// cannot ride in raw: 0-16 collide with Linux's read/write/open. Every stub
/// tags the number with "mpos" in the top 32 bits; the kernel strips the tag
/// and dispatches to the compact table. `syscall` itself destroys rcx and
/// r11, so the stubs name them as outputs.
const MPOS_TAG: u64 = 0x6D70_6F73 << 32; // "mpos"

fn sys0(number: u64) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") (MPOS_TAG | number) as isize => ret,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys1(number: u64, a1: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") (MPOS_TAG | number) as isize => ret,
            in("rdi") a1,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys2(number: u64, a1: usize, a2: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") (MPOS_TAG | number) as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys3(number: u64, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") (MPOS_TAG | number) as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys4(number: u64, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") (MPOS_TAG | number) as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

/// Six arguments, the kernel's whole allowance: number in `rax`, then
/// `rdi, rsi, rdx, r10, r8, r9`. The last two registers are why the block
/// above stops at four - a call with five or six needs `r8` and `r9`, which
/// nothing smaller does.
#[allow(clippy::too_many_arguments)]
fn sys6(
    number: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") (MPOS_TAG | number) as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
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

/// Make an empty file at `path` and open it for writing. Returns the fd, or a
/// negative errno - EEXIST when the name is taken, ENOENT when its directory
/// is not there. Shell redirection unlinks first, which is what `>` means.
pub fn create(path: &[u8]) -> isize {
    sys2(nr::CREATE, path.as_ptr() as usize, path.len())
}

/// Remove the name `path`. ENOENT when nothing answers to it.
pub fn unlink(path: &[u8]) -> isize {
    sys2(nr::UNLINK, path.as_ptr() as usize, path.len())
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
    spawn_with(path, args, &[])
}

/// [`spawn`] with the child's descriptors laid out by the caller.
///
/// Each pair is (an fd of ours, the number it should have in the child), so
/// `(pipe_read, 0)` gives the child its input from a pipe and `(file, 1)`
/// sends its output to a file. The child is born with the standard three and
/// then these, and with nothing else of ours - which is what makes a
/// pipeline's end-of-file arrive.
///
/// At most `REDIRECT_MAX` pairs; more is EINVAL.
pub fn spawn_with(path: &[u8], args: &[&[u8]], redirects: &[(i32, i32)]) -> isize {
    const BLOB_MAX: usize = 512;
    const REDIRECT_MAX: usize = 8;
    if redirects.len() > REDIRECT_MAX {
        return -22; // EINVAL
    }
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
    // Laid out as the kernel reads it: two i32 per pair, (from, to) each.
    let mut pairs = [0i32; REDIRECT_MAX * 2];
    for (at, &(from, to)) in redirects.iter().enumerate() {
        pairs[at * 2] = from;
        pairs[at * 2 + 1] = to;
    }
    sys6(
        nr::SPAWN,
        path.as_ptr() as usize,
        path.len(),
        blob.as_ptr() as usize,
        n,
        pairs.as_ptr() as usize,
        redirects.len(),
    )
}

/// Open a pipe: `fds[0]` reads, `fds[1]` writes. Returns 0, or a negative
/// errno. Both ends must be closed - the reader sees end of input only when
/// every writer is gone.
pub fn pipe(fds: &mut [i32; 2]) -> isize {
    sys1(nr::PIPE, fds.as_mut_ptr() as usize)
}

/// Block until the child `pid` exits, and take its exit code. Negative on
/// error - ECHILD for a pid that is not a child of ours.
pub fn wait(pid: isize) -> isize {
    wait_flags(pid as usize, 0)
}

/// [`wait`](fn.wait.html) with flags. Bit 0 is WNOHANG: a child still running
/// answers EAGAIN instead of blocking, which is how a shell collects
/// background jobs without stopping for any of them.
pub fn wait_flags(pid: usize, flags: usize) -> isize {
    sys2(nr::WAIT, pid, flags)
}

/// Move this process to the directory `path` names. Relative paths start
/// where the process already is; children inherit wherever this lands.
pub fn chdir(path: &[u8]) -> isize {
    sys2(nr::CHDIR, path.as_ptr() as usize, path.len())
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
