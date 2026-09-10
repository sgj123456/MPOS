//! The first mpos user program: proof that ring 3 works end to end.
//!
//! scripts/mkiso.sh compiles this file standalone with rustc - statically
//! linked, fixed addresses, exactly the ET_EXEC image the kernel's loader
//! accepts - and drops the binary into the initramfs at /bin/hello, where the
//! kernel loads and runs it. The syscall table is included straight out of the
//! kernel source, so there is exactly one set of numbers.

#![no_std]
#![no_main]

// The table declares numbers this program does not use; that is the point of
// sharing one table rather than trimming a copy.
#[allow(dead_code)]
#[path = "../../src/syscall/nr.rs"]
mod nr;

use core::arch::asm;
use core::panic::PanicInfo;

/// The exit code the kernel's milestone check expects. Both sides agree on it
/// the way they agree on the syscall numbers.
const EXIT_CODE: usize = 42;

/// The gate's register convention: number in `rax`, first argument in `rdi`,
/// result in `rax`. The stub on the other side preserves every register but
/// `rax`, so nothing needs naming as clobbered.
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

fn write(bytes: &[u8]) -> isize {
    sys3(nr::WRITE, 1, bytes.as_ptr() as usize, bytes.len())
}

fn exit(code: usize) -> ! {
    sys1(nr::EXIT, code);
    unreachable!("EXIT never returns")
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    if write(b"hello from ring 3\n") < 0 {
        exit(1);
    }

    // Voluntarily leave the CPU twice from ring 3: once by yielding, once by
    // sleeping. Both come back through the gate's iretq, which is the path
    // every later return to user mode will take.
    if sys0(nr::YIELD) != 0 || sys1(nr::SLEEP, 150) != 0 {
        write(b"hello: yield or sleep failed\n");
        exit(1);
    }

    let pid = sys0(nr::GETPID);
    if pid < 2 {
        write(b"hello: getpid returned a kernel task id\n");
        exit(1);
    }

    write(b"hello: back from sleep, exiting\n");
    exit(EXIT_CODE);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    write(b"hello panicked\n");
    exit(1);
}
