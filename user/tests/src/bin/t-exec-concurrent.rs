//! Four programs started at once from the same disk must all get to run.
//!
//! The reading this pins is the one `docs/notes/07-device-drivers.md` R.9 records:
//! on the desktop image, two or more background children that each `execve` a
//! binary off the ext4 root stop inside `execve` (state `S`, argv still the
//! shell's), while a single such child runs. The program is read through the
//! ext4 cache and the parked virtio-blk transfer, so the suspected hand-off is
//! between those two under contention - and the gate never saw it: the first
//! machine's root is an initramfs, where no program is read through a block
//! device, and no test forked concurrent readers on the second machine.
//!
//! The claim is deliberately cheap and door-shaped: four children, forked
//! before any of them draws a program, each `execve`ing the same suite binary,
//! and a bounded wait for all four. A machine whose exec path serialises them
//! passes; a machine that wedges one of them answers the same shape R.9 did -
//! a child still parked after seconds - which is what the timeout claim names
//! rather than a hang the lane's own budget would swallow.
//!
//! What this does *not* claim: the order of the underlying transfers. The
//! device completes them as it likes; only "every child reached its program in
//! bounded time" is the promise.

use mpos_user_tests::{Report, host_run};

/// How many children draw a program at once.
const CHILDREN: usize = 4;

/// The longest a child may take, in milliseconds, before the claim says it is
/// stuck. A program's `exec` on a 2 GiB machine is milliseconds; the R.9
/// reproduction parked for ever, so any bound wider than a second tells them
/// apart without pretending to measure latency.
const BOUND_MS: i32 = 5_000;

/// The program every child draws. A suite binary: present on both arms, small,
/// and it exits 0 at once, so the measuring instrument is the `execve` itself.
const PROGRAM: &str = "/bin/t-getpid";

fn main() {
    let mut r = Report::new();
    if host_run() {
        r.finish();
        return;
    }
    // `PROGRAM` has to exist, or every child answers the same way for the wrong
    // reason and the claim would pass on a broken image.
    let present = std::fs::metadata(PROGRAM).is_ok();
    r.check(
        &format!("the program every child draws is on this machine ({PROGRAM})"),
        present,
    );
    if !present {
        r.finish();
        return;
    }

    // The barrier: a child does not draw its program until the parent says so,
    // so the four `execve`s begin together rather than in fork order - the
    // contention R.9's reading needs.
    let mut gate = [0i32; 2];
    // SAFETY: `gate` is a live two-element array the call fills.
    if unsafe { libc::pipe(gate.as_mut_ptr()) } != 0 {
        r.check("the children's start gate could be opened", false);
        r.finish();
        return;
    }
    let mut pids = Vec::new();
    for index in 0..CHILDREN {
        // SAFETY: `fork` in the child draws the program immediately; the parent
        // only records the pid and never blocks inside the unsafe block.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // The child: wait for the gate, then its own program. `execve`
            // replaces the image, and its failure is the exit word the parent
            // reads.
            let mut byte = 0u8;
            // SAFETY: `byte` is live and the read end is the pipe above.
            unsafe {
                libc::close(gate[1]);
                libc::read(gate[0], (&mut byte as *mut u8).cast(), 1);
            }
            unsafe {
                libc::execl(
                    c"/bin/t-getpid".as_ptr(),
                    c"t-getpid".as_ptr(),
                    std::ptr::null::<libc::c_char>(),
                );
                libc::_exit(127);
            }
        }
        if pid < 0 {
            r.check(
                &format!("child {index} could be forked (fork answered {pid})"),
                false,
            );
            pids.push(0);
            continue;
        }
        pids.push(pid);
    }

    // Open the gate: one byte each, all four at once.
    // SAFETY: the write end is open in the parent and the array is live.
    unsafe {
        libc::close(gate[0]);
        libc::write(gate[1], [b'x'; CHILDREN].as_ptr().cast(), CHILDREN);
        libc::close(gate[1]);
    }

    // Every child must be reaped inside the bound. One loop over all of them,
    // so a stuck child is measured against the same clock as its siblings.
    let start = std::time::Instant::now();
    let mut statuses = vec![None; CHILDREN];
    let mut stuck = 0usize;
    while start.elapsed().as_millis() < BOUND_MS as u128 && statuses.iter().any(|s| s.is_none()) {
        for (index, pid) in pids.iter().enumerate() {
            if statuses[index].is_some() || *pid <= 0 {
                continue;
            }
            let mut status = 0;
            // SAFETY: `pid` is a child of this process and `status` is a live
            // `int` the call writes.
            let answered = unsafe { libc::waitpid(*pid, &mut status, libc::WNOHANG) };
            if answered == *pid {
                statuses[index] = Some(status);
            }
        }
        if statuses.iter().any(|s| s.is_none()) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    for (index, slot) in statuses.iter().enumerate() {
        if slot.is_none() {
            stuck += 1;
            r.check(
                &format!("child {index} drew its program inside {BOUND_MS} ms (still parked)"),
                false,
            );
        }
    }
    if stuck == 0 {
        r.check(
            &format!("all {CHILDREN} children drew their programs at once"),
            true,
        );
    }
    let clean = statuses
        .iter()
        .flatten()
        .filter(|status| libc::WIFEXITED(**status) && libc::WEXITSTATUS(**status) == 0)
        .count();
    r.check(
        &format!("every child's program exited 0 ({clean} of {CHILDREN})"),
        clean == CHILDREN,
    );

    // A stuck child is left parked on purpose: the lane's own per-program
    // budget (`tests.toml`'s `timeout`) kills the program, and killing this
    // process is what retires the stuck children with it.
    r.finish();
}
