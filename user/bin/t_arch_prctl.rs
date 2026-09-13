//! t_arch_prctl - one test for arch_prctl, the TLS setup a musl binary's
//! strace opens with: `arch_prctl(ARCH_SET_FS, 0x...)`.
//!
//! musl's __init_tp calls ARCH_SET_FS once at startup to point FS at the
//! thread block, and every TLS variable afterwards reads through it. The
//! test mirrors that: set the base, then prove the base is really live by
//! reading two words through `fs:` segment overrides - a stand-in TLS block
//! with musl's self-pointer shape at fs:0 and a magic word at fs:8 - and
//! finish by restoring the zero base the process started with.
//!
//! Before that, the GET form is pinned: ARCH_GET_FS answers 0 and writes
//! the base into `*(u64 *)addr` - the value travels through memory, not
//! `rax`, which is the shape the man page and every libc wrapper agree on.
//! The probe sets a known base, reads it back through a static seeded with
//! a sentinel, and restores the zero base before the TLS checks run, so
//! the test still ends with FS base 0. An unknown op is EINVAL.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// A stand-in TLS block, laid out like musl's: the self pointer at fs:0 and
/// a magic word at fs:8 to read back through the segment override.
static mut TLS: [u64; 4] = [0; 4];
const MAGIC: u64 = 0x6d70_6f73_746c_7331; // "mpostls1"

/// The out-word the ARCH_GET_FS probe writes into, seeded with a sentinel
/// so a call that answered without writing could not hide.
static mut FS_OUT: u64 = 0xAAAA;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_arch_prctl\n");
    let mut failed = false;

    // The GET probe: set a base the test chose itself, read it back through
    // memory, and put the zero base back before anything else runs.
    let r = lin::arch_prctl(lin::ARCH_SET_FS, 0x2000);
    lin::check_eq(
        &mut failed,
        b"ARCH_SET_FS accepts the probe base",
        r,
        0,
    );
    let r = lin::arch_prctl(lin::ARCH_GET_FS, &raw mut FS_OUT as usize);
    lin::check_eq(
        &mut failed,
        b"ARCH_GET_FS answers 0",
        r,
        0,
    );
    unsafe {
        lin::check_eq(
            &mut failed,
            b"ARCH_GET_FS writes the base into memory, not rax",
            FS_OUT as isize,
            0x2000,
        );
    }
    let r = lin::arch_prctl(lin::ARCH_SET_FS, 0);
    lin::check_eq(
        &mut failed,
        b"ARCH_SET_FS restores the zero base after the probe",
        r,
        0,
    );

    // An op the kernel has no table row for is EINVAL.
    let r = lin::arch_prctl(0x9999, 0);
    lin::check_eq(
        &mut failed,
        b"an unknown arch_prctl op is EINVAL",
        r,
        lin::EINVAL,
    );

    let base = &raw mut TLS as usize;
    unsafe {
        TLS[0] = base as u64;
        TLS[1] = MAGIC;
    }

    let r = lin::arch_prctl(lin::ARCH_SET_FS, base);
    lin::check_eq(
        &mut failed,
        b"ARCH_SET_FS accepts the TLS base",
        r,
        0,
    );

    let mut self_ptr: u64;
    let mut magic: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, qword ptr fs:[0x0]",
            out(reg) self_ptr,
            options(nostack, preserves_flags)
        );
        core::arch::asm!(
            "mov {}, qword ptr fs:[0x8]",
            out(reg) magic,
            options(nostack, preserves_flags)
        );
    }
    lin::check_true(
        &mut failed,
        b"fs:[0] reads back the base itself",
        self_ptr == base as u64,
    );
    lin::check_true(
        &mut failed,
        b"fs:[8] reads back the magic word",
        magic == MAGIC,
    );

    let r = lin::arch_prctl(lin::ARCH_SET_FS, 0);
    lin::check_eq(
        &mut failed,
        b"ARCH_SET_FS restores the zero base",
        r,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_arch_prctl: panicked\n")
}
