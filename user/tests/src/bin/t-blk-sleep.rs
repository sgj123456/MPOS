//! t-blk-sleep - a block transfer **parks its task**: the scheduler runs other
//! work during the device's own latency, and that is visible from userland as a
//! sibling's counter moving *inside* a device call.
//!
//! The shape this pins is §10.5's last landed row. The transfer used to hold
//! the core from submit to retire - first masked, then unmasked - and the
//! history of why is the `RefCell already borrowed` the first parking cut
//! died of: the ext4 cache held a borrow across the device call. The cache
//! claims its slot now and parks holding nothing, the driver parks the
//! submitting task on the completion mailbox, and the machine's answer is the
//! one this program measures: a task waiting on the device is not running.
//!
//! # The instrument, and the confound it is built against
//!
//! A parent makes many small raw reads of a block device - one sector each -
//! and a forked sibling spins on a counter in a shared `MAP_SHARED` page.
//! Before and after each read the parent reads the sibling's counter, and a
//! probe *hits* when the counter moved across one read.
//!
//! The confound is the tick: a read whose window lasts past a tick can lose
//! the core to preemption, and a hit it is anyway. So each probe is one
//! 64-KiB read - window tens of microseconds under TCG against a 4 ms tick -
//! and the claim is *most* probes hit, not all-or-nothing. Under the parked
//! transfer every probe hits: the task sleeps, the sibling is scheduled, and
//! the wake finds it mid-counter. Under the shapes this replaced a hit
//! needed a tick to land inside a probe's few-instruction windows, which is
//! a rate the bound below is nowhere near. The claim is a proportion
//! precisely because the counterfactual is not zero - it is *rare* - and
//! naming the difference is what makes one number pin the shape on both
//! machines, under TCG or KVM.
//!
//! The device is the first of `/dev/vda`, `/dev/vdb`, `/dev/sda` that opens
//! and holds at least a megabyte - both sweep arms register one (the first
//! machine's boot medium over PCI, the second machine's root over MMIO) - and
//! the reads are raw, so no filesystem and no cache sit in the way: every
//! probe is the transport's own park.
//!
//! The sibling sees the shared page through the frames §5.1 aliased across a
//! plain `fork`; it leaves through the raw `SYS_exit_group` a forked std
//! child must leave through, and the parent collects it with `wait4`.

use mpos_user_tests::Report;
use rustix::fs::{CWD, Mode, OFlags, fstat, openat};
use rustix::io::pread;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};
use rustix::process::WaitStatus;
use rustix::process::{Pid, WaitOptions, waitpid};

/// One probe's read: **one sector** - a window tens of microseconds wide
/// against a 4 ms tick. The whole instrument lives on that ratio, because
/// the confound is a tick landing inside a probe: the shapes this test
/// separates differ by whether the device call yields, and a window an
/// order of magnitude under the tick is one a preemption can essentially
/// never reach. (The first cut read 64 KiB a probe - a window ~a tick wide
/// under TCG - and measured, on the non-parking shape, 33 hits of 64: the
/// tick got in through the read's own duration half the time. One sector
/// answers that; the 33 is the calibration this bound leans on.)
const PROBE_BYTES: usize = 512;

/// How many probes one run makes.
const PROBES: usize = 64;

/// The share of probes that must hit. Under the parked transfer the rate is
/// every probe: the task sleeps, the runnable sibling is scheduled, and the
/// counter moves before the completion is even looked at. Under the shapes
/// this replaced a hit needed a tick inside the probe's window, which for a
/// one-sector read is a few percent - the measured 33 of 64 above was the
/// *old*, tick-wide window, and it is why the bound sits where it does.
const HIT_BOUND: usize = PROBES * 5 / 8;

/// The smallest device worth probing: under it, later probes read past the
/// end and answer empty, and an empty read parks nothing.
const MIN_DEVICE_BYTES: u64 = 1024 * 1024;

/// The shared page: a counter the sibling spins on, and the word that tells
/// it to stop. Offsets, because the parent and the sibling map the same
/// frames and each keeps its own pointer.
const STOP: usize = 0;
const COUNT: usize = 8;

/// Leave through `exit_group`: a forked std child holds a copy of the
/// parent's buffered stdout, and only a non-flushing exit leaves it behind.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

fn shared_page() -> (*mut u64, usize) {
    let page = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            4096,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
        )
    }
    .expect("a shared anonymous page maps") as *mut u64;
    unsafe {
        page.add(STOP / 8).write_volatile(0);
        page.add(COUNT / 8).write_volatile(0);
    }
    (page, 4096)
}

/// The first block device that opens and holds at least a megabyte, with the
/// name the probe print names. `None`: every candidate refused, and the
/// caller says so.
fn find_device() -> Option<(rustix::fd::OwnedFd, String)> {
    for name in ["/dev/vda", "/dev/vdb", "/dev/sda"] {
        let Ok(fd) = openat(CWD, name, OFlags::RDONLY, Mode::empty()) else {
            continue;
        };
        let Ok(stat) = fstat(&fd) else {
            continue;
        };
        if stat.st_size as u64 >= MIN_DEVICE_BYTES {
            return Some((fd, name.to_string()));
        }
    }
    None
}

fn main() {
    let mut r = Report::new();
    let Some((fd, name)) = find_device() else {
        r.check("a block device at least a megabyte big exists", false);
        r.finish();
    };
    r.check(&format!("{name} opens and holds the probe"), true);
    let (page, page_len) = shared_page();

    let count = || unsafe { page.add(COUNT / 8).read_volatile() };
    let set_stop = |v: u64| unsafe { page.add(STOP / 8).write_volatile(v) };

    match unsafe { libc::fork() } {
        -1 => {
            r.check("fork() succeeds", false);
            r.finish();
        }
        0 => {
            // The sibling: count until told to stop. It never yields on its
            // own - a tick preempts it, and the parked transfer's wake
            // requeues the parent behind it - which is the whole instrument.
            loop {
                if unsafe { page.add(STOP / 8).read_volatile() } == 1 {
                    leave(0);
                }
                unsafe {
                    let at = page.add(COUNT / 8);
                    at.write_volatile(at.read_volatile().wrapping_add(1));
                }
            }
        }
        pid => {
            // The sibling is runnable the moment it is forked; hand it the
            // core once so the first probe is not also the first schedule.
            let mut spins = 0u64;
            while count() == 0 {
                unsafe { libc::sched_yield() };
                spins += 1;
                if spins > 1_000_000 {
                    r.check("the sibling was scheduled", false);
                    set_stop(1);
                    let _ = waitpid(Pid::from_raw(pid), WaitOptions::empty());
                    r.finish();
                }
            }
            r.check("the sibling was scheduled", true);

            // The probes: one raw read each, the counter read on both sides
            // of it, one buffer for all of them (an allocation in the gap
            // would widen the window the confound lives in). The offsets
            // walk the device's first 32 KiB.
            let mut buf = vec![0u8; PROBE_BYTES];
            let mut hits = 0usize;
            let mut answered = 0usize;
            for probe in 0..PROBES {
                let offset = (probe * PROBE_BYTES) as u64;
                let before = count();
                let n = pread(&fd, &mut buf, offset).expect("pread answers");
                let after = count();
                if n == PROBE_BYTES {
                    answered += 1;
                    if after > before {
                        hits += 1;
                    }
                }
            }
            println!("BLKSLEEP probes={PROBES} answered={answered} hits={hits}");

            r.check(
                "every probe's read was answered in full",
                answered == PROBES,
            );
            r.check(
                &format!(
                    "the sibling ran inside {hits} of {PROBES} device calls \
                     (bound {HIT_BOUND}) - the transfer parks its task"
                ),
                hits >= HIT_BOUND,
            );

            set_stop(1);
            let collected = waitpid(Pid::from_raw(pid), WaitOptions::empty());
            let ok = match collected {
                Ok(Some((_, status))) => {
                    WaitStatus::exited(status) && WaitStatus::exit_status(status) == Some(0)
                }
                _ => false,
            };
            r.check(&format!("the sibling left cleanly ({collected:?})"), ok);
            unsafe { munmap(page.cast(), page_len) };
            r.finish();
        }
    }
}
