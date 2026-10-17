//! The std-side tests' shared verdict protocol.
//!
//! Every program in this crate reports the same way: one printed line per claim
//! as it is made, then a verdict and an exit status the harness reads. Until this
//! file existed, each of the thirty wrote its own copy of that - a local
//! `struct Report` with a `check` method - which is also how they drifted:
//! `t-process-vm-libc` ends on "all passed" where its neighbours end on `PASS`.
//!
//! The `=== RUN` / `=== PASS` / `=== FAIL` lines around a program are the
//! *runner's*, not the program's (`xtask` writes them from the exit status); what
//! a program owes the runner is a clean status, and what it owes a person
//! reading the log is the per-claim detail. That division is why this helper
//! prints and exits rather than reporting to the runner.
//!
//! This is the same protocol the `#![no_std]` conformance suite spoke through
//! `lin::check_true`/`lin::check_eq`/`lin::done`, and the same exit codes - what
//! let that suite move here one program at a time and vanish when the last one
//! did (`NOTES.md` §15.11). One protocol, one reader, one crate now.

/// The claims one test has made, and whether any of them failed.
pub struct Report {
    failed: bool,
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

impl Report {
    pub fn new() -> Self {
        Self { failed: false }
    }

    /// One claim, printed as it is made: a run that hangs or dies still shows how
    /// far it got, which is the difference between "this failed" and "this never
    /// ran".
    pub fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    /// The verdict, and the exit status the runner turns into `=== PASS` or
    /// `=== FAIL`. Exits rather than panicking: a claim that did not hold is a
    /// result about the kernel, not a crash in the test.
    pub fn finish(self) -> ! {
        if self.failed {
            println!("FAILED");
            std::process::exit(1);
        }
        println!("PASS");
        std::process::exit(0);
    }
}

/// Whether this program is running on the host's real Linux rather than in a
/// payload sweep. A handful of migrated claims pin an answer that is mpos's by
/// design and differ from current Linux's (a refused TIMER_ABSTIME, a retired
/// madvise slot, an unaligned mprotect inside nobody's mapping); those checks
/// accept the host's own answer when - and only when - the run says it is a
/// host run, so `MPOS_ON_HOST=1 t-<name>` measures against real Linux while
/// the gate keeps pinning this kernel. The divergence itself is named in each
/// program's comment, never hidden behind this flag.
pub fn host_run() -> bool {
    std::env::var_os("MPOS_ON_HOST").is_some()
}

/// The uapi's `struct rusage`: two `timeval`s, then fourteen `long` counters.
pub const RUSAGE_BYTES: usize = 144;

/// The uapi's `struct rusage`, spelled here rather than taken from `libc`.
///
/// `libc::rusage` is a generic-unix superset and 272 bytes on this target, so
/// using it would mean a buffer larger than the wire - safe, but unable to say
/// anything about the wire's size or about a byte the kernel did not write. The
/// two named `timeval`s are what a caller reads; the fourteen counters are both
/// the tail a "the kernel wrote the whole record" claim is about and the tail
/// this kernel leaves zero because it measures CPU and nothing else.
///
/// One definition and three readers: `t-times` pins `getrusage`'s answers, and
/// `t-wait4`/`t-waitid` pin the `rusage` both wait doors fill. The record was
/// spelled in `t-times` alone until the second and third reader arrived.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rusage {
    pub ru_utime: Timeval,
    pub ru_stime: Timeval,
    pub counters: [libc::c_long; 14],
}

/// The uapi's `struct timeval`: two `__kernel_long_t`s. Spelled with
/// `libc::c_long` rather than `libc::time_t`/`suseconds_t`, which `libc`
/// deprecates on musl for a 32-bit-time change that does not reach these two
/// 64-bit targets - where those aliases *are* `c_long`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timeval {
    pub tv_sec: libc::c_long,
    pub tv_usec: libc::c_long,
}

impl Timeval {
    /// The pair as one whole microsecond count.
    pub fn micros(self) -> i64 {
        self.tv_sec * 1_000_000 + self.tv_usec
    }
}

impl Rusage {
    /// A record no kernel would produce: `-1` in every field. "The kernel wrote
    /// this struct" is then one predicate on every machine and every event
    /// class: Linux fills `ru_maxrss`, `ru_minflt` and `ru_nvcsw` from the
    /// child, this kernel measures only the CPU and zeroes the rest, and both
    /// differ from the seed.
    pub fn seeded() -> Self {
        Self {
            ru_utime: Timeval {
                tv_sec: -1,
                tv_usec: -1,
            },
            ru_stime: Timeval {
                tv_sec: -1,
                tv_usec: -1,
            },
            counters: [-1; 14],
        }
    }

    /// Whether a kernel has written this record since it was [`seeded`].
    ///
    /// [`seeded`]: Rusage::seeded
    pub fn written(&self) -> bool {
        self.counters.iter().any(|c| *c != -1)
    }

    /// `ru_utime + ru_stime` as one whole microsecond count - the CPU a wait
    /// reports, on the machine and on the host alike.
    pub fn cpu_micros(&self) -> i64 {
        self.ru_utime.micros() + self.ru_stime.micros()
    }
}

/// The record is exactly the uapi's 144 bytes, and that is not decoration: the
/// kernel writes all of it, so a shorter buffer would be written past its end.
/// A claim the compiler can make is better than one a run has to.
const _: () = assert!(core::mem::size_of::<Rusage>() == RUSAGE_BYTES);

/// Copy §10.5's ext4 fixture onto the SATA disk, through the block door.
///
/// Both programs of that instrument need the filesystem *on* the disk before they
/// can read a block of it, and neither may depend on the other having run: the
/// runner walks `/bin/t-*` in name order, so `t-diskbench` is staged before
/// `t-ext4-mount` and would find a disk still made of zeros — which is exactly
/// what the first gate run of the pair measured (the disk is laid fresh by the
/// sweep, `disk::reset_sata_image`, so nothing is inherited from an earlier run
/// either). Each therefore lays a copy of its own: 80 MiB through `write(2)` on
/// the block node, which is the AHCI write path at volume, idempotent, and the
/// one place this loop lives.
///
/// Returns the bytes copied. The error is the door's own errno, wrapped so a
/// caller can print which one it was.
pub fn lay_ext4_fixture(source: &str, disk: &str) -> std::io::Result<u64> {
    use rustix::fs::{CWD, Mode, OFlags, openat};

    /// One read per chunk: big enough that the loop is not the cost, small
    /// enough to keep the payload out of any second mapping while the block
    /// layer does the moving.
    const CHUNK: usize = 1024 * 1024;

    fn errno(e: rustix::io::Errno) -> std::io::Error {
        std::io::Error::from_raw_os_error(e.raw_os_error())
    }

    let src = openat(CWD, source, OFlags::RDONLY, Mode::empty()).map_err(errno)?;
    let dst = openat(CWD, disk, OFlags::WRONLY, Mode::empty()).map_err(errno)?;
    let mut buf = vec![0u8; CHUNK];
    let mut copied = 0u64;
    loop {
        let n = rustix::io::read(&src, &mut buf).map_err(errno)?;
        if n == 0 {
            return Ok(copied);
        }
        // Sequential writes to the raw device: the file position *is* the sector
        // address, so no `pwrite` dance is needed - and a short write is
        // continued rather than read as the end of the copy.
        let mut done = 0;
        while done < n {
            let k = rustix::io::write(&dst, &buf[done..n]).map_err(errno)?;
            if k == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
            }
            done += k;
        }
        copied += n as u64;
    }
}

/// Burn `ms` of *user* CPU in this thread and return.
///
/// Time-bounded rather than counted, because the guest runs under TCG where a
/// fixed iteration count is a different amount of work on each machine - and
/// because several claims need "this process has some CPU behind it", which only
/// the clock can promise. The arithmetic is opaque to the optimiser on purpose;
/// a burner that folded away would take every claim built on it with it.
///
/// One definition and three readers: `t-signalfd` needs a child with CPU in it
/// before a signal lands, and `t-wait4`/`t-waitid` need one whose CPU a `wait`
/// has to report. Each carried its own copy until the second and third arrived.
pub fn burn_cpu_ms(ms: i64) {
    fn now_ns() -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a `timespec` this call owns.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec * 1_000_000_000 + ts.tv_nsec
    }
    let start = now_ns();
    let mut acc: u64 = 0;
    loop {
        for i in 0..1_000_000u64 {
            acc = acc
                .wrapping_add(i)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .rotate_left(7)
                ^ acc;
        }
        std::hint::black_box(acc);
        if now_ns() - start >= ms * 1_000_000 {
            return;
        }
    }
}
