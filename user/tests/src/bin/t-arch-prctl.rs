//! t-arch-prctl - one test for arch_prctl, the TLS setup a musl binary's
//! strace opens with: `arch_prctl(ARCH_SET_FS, 0x...)`.
//!
//! musl's __init_tp calls ARCH_SET_FS once at startup to point FS at the
//! thread block, and every TLS variable afterwards reads through it. The
//! test mirrors that: set the base, then prove the base is really live by
//! reading two words through `fs:` segment overrides - a stand-in TLS block
//! with musl's self-pointer shape at fs:0 and a magic word at fs:8.
//!
//! Before that, the GET form is pinned: ARCH_GET_FS answers 0 and writes
//! the base into `*(u64 *)addr` - the value travels through memory, not
//! `rax`, which is the shape the man page and every libc wrapper agree on.
//! An unknown op is EINVAL.
//!
//! # Why every pair runs in a child
//!
//! The no-std version could end with "restore the zero base the process
//! started with", because its FS base *was* zero. Under a dynamic musl the
//! base belongs to the runtime's own TLS, and the damage of moving it is
//! immediate: musl's own `syscall` wrapper reads the stack canary through
//! `fs:[0x28]` on entry, so the first raw call after an ARCH_SET_FS to any
//! foreign base faults in the caller (measured on this host). Every SET /
//! read-back pair therefore lives in a forked child that `_exit`s with a
//! code naming which half answered; the parent only waits. The claims are
//! unchanged, only their home moved. The `fs:` reads themselves stay inline
//! asm - that is the subject, clause (c) against any wrapper.
//!
//! This call exists on x86_64 only (`nr_aarch64.rs` carries no ARCH_PRCTL;
//! arm64 reaches TLS through other doors), so the program joins tests.toml's
//! `aarch64` exclusion group rather than pretending both machines answer it.

#[cfg(target_arch = "x86_64")]
use mpos_user_tests::Report;

#[cfg(target_arch = "x86_64")]
const ARCH_SET_FS: libc::c_int = 0x1002;
const ARCH_GET_FS: libc::c_int = 0x1003;

/// A stand-in TLS block, laid out like musl's: the self pointer at fs:0 and
/// a magic word at fs:8 to read back through the segment override.
#[cfg(target_arch = "x86_64")]
static mut TLS: [u64; 4] = [0; 4];
#[cfg(target_arch = "x86_64")]
const MAGIC: u64 = 0x6d70_6f73_746c_7331; // "mpostls1"

#[cfg(target_arch = "x86_64")]
fn arch_prctl(op: libc::c_int, addr: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_arch_prctl, op, addr) as isize }
}

/// Fork, run `body`, wait, and return the child's exit code (or -1 if the
/// child died some other way or the fork failed).
#[cfg(target_arch = "x86_64")]
fn child(body: impl FnOnce() -> i32) -> i32 {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return -1;
    }
    if pid == 0 {
        unsafe { libc::_exit(body()) };
    }
    let mut status = 0i32;
    let waited = unsafe { libc::wait4(pid, &mut status, 0, core::ptr::null_mut()) };
    if waited != pid || (status & 0x7f) != 0 {
        return -1;
    }
    (status >> 8) & 0xff
}

fn main() {
    // The call is x86_64's own: arm64 has no `arch_prctl` number at all, so
    // on the second machine this program says so and leaves - its real pin
    // runs only where the syscall exists.
    #[cfg(not(target_arch = "x86_64"))]
    {
        println!("t-arch-prctl: skipped (no arch_prctl on this machine)");
        return;
    }
    #[cfg(target_arch = "x86_64")]
    x86_main();
}

#[cfg(target_arch = "x86_64")]
fn x86_main() {
    let mut r = Report::new();

    // The GET probe, as one child: set a base the test chose itself, read it
    // back through memory, check what arrived. Code 0 = both halves right;
    // 1 = SET refused; 2 = GET refused; 3 = GET wrote the wrong thing.
    let c = child(|| {
        if arch_prctl(ARCH_SET_FS, 0x2000) != 0 {
            return 1;
        }
        let mut fs_out: u64 = 0xAAAA;
        if arch_prctl(ARCH_GET_FS, &raw mut fs_out as usize) != 0 {
            return 2;
        }
        if fs_out != 0x2000 {
            return 3;
        }
        0
    });
    r.check("ARCH_SET_FS accepts the probe base", c >= 0 && c != 1);
    r.check("ARCH_GET_FS answers 0", c >= 0 && c != 2);
    r.check("ARCH_GET_FS writes the base into memory, not rax", c == 0);

    // An op the kernel has no table row for is EINVAL.
    let c = child(|| {
        let ret = arch_prctl(0x9999, 0);
        if ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            0
        } else {
            1
        }
    });
    r.check("an unknown arch_prctl op is EINVAL", c == 0);

    // The live-base proof: set FS to our stand-in TLS block and read two
    // words through the segment override. Code 1 = SET refused; 2 = fs:[0]
    // wrong; 3 = fs:[8] wrong.
    let c = child(|| {
        let base = &raw mut TLS as usize;
        unsafe {
            TLS[0] = base as u64;
            TLS[1] = MAGIC;
        }
        if arch_prctl(ARCH_SET_FS, base) != 0 {
            return 1;
        }
        let self_ptr: u64;
        let magic: u64;
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
        if self_ptr != base as u64 {
            return 2;
        }
        if magic != MAGIC {
            return 3;
        }
        0
    });
    r.check("ARCH_SET_FS accepts the TLS base", c >= 0 && c != 1);
    r.check("fs:[0] reads back the base itself", c == 0);
    r.check("fs:[8] reads back the magic word", c == 0);

    r.finish()
}
