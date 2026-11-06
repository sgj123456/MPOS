//! t-kslot - what a *stale* pid number answers once somebody else has the slot.
//!
//! The instrument behind `docs/notes/` §6.89 and `docs/uapi/` Item 202, and the
//! reason the roadmap now carries "a pid number is a promise, and here it is the
//! task slot" as a named gap.
//!
//! On Linux a pid is a refcounted `struct pid` whose lifetime is its own, so a
//! number a caller remembers is a *promise*: after the child is collected the
//! number answers `ESRCH` to `kill` and `ENOENT` under `/proc`, for ever, even
//! once an unrelated process is given it. Here the number **is** the task slot
//! (`crates/pidns/src/lib.rs`'s identity map at the root), and the slot allocator
//! hands out the lowest free index - so the very next `fork` takes the freed
//! number, and `kill(<stale>, SIGTERM)` ends a process the caller has never had
//! anything to do with.
//!
//! **This program is a reading, not a check, and it is in the `diagnostic` group
//! for that reason** (`assets/configs/tests.toml`). The claims it would make are
//! red here every run until the init namespace stops numbering by slot, and a
//! sweep's job is to be a statement about the kernel, not about a gap the
//! roadmap already names. What it therefore prints is the divergence itself,
//! side by side with the Linux reading recorded in §6.89, so that whoever closes
//! the gap has the instrument already in the tree - and turns the two `expect`
//! lines into `Report::check` calls on the day it goes green.
//!
//! Run it by hand:
//!
//! ```text
//! cargo build -p mpos-user-tests --release --target x86_64-unknown-linux-musl
//! cp target/x86_64-unknown-linux-musl/release/t-kslot build/probe/oneshot/stage/bin/
//! MEM=3072 bash .workbuddy/one-test-boot.sh t-kslot 120
//! ```
//!
//! The safe half of the same question - a *collected* child's number in `wait` -
//! is a real check and lives in `t-waitid` (*a second wait for a collected child
//! answers ECHILD, not a park*) and `t-wait4` (*and a second wait for it is
//! ECHILD*). This program exists for the doors those two do not cover.

use std::io::Read;

/// What `/proc/<pid>/stat` says the process's state letter is, or how the read
/// failed. The letter is field 3, right after the parenthesised `comm` - which is
/// why the split starts after the *last* `)`: a `comm` may contain everything.
fn state_of(pid: libc::pid_t) -> String {
    let path = format!("/proc/{pid}/stat");
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return "ENOENT".to_string(),
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return "ENOENT".to_string();
    }
    match text
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().next())
    {
        Some(Some(state)) => state.to_string(),
        _ => "?".to_string(),
    }
}

/// One fork/pipe pair so that a child of *another* process can be given the slot
/// this program's own child has just released.
///
/// The ordering is the whole instrument: the helper occupies its own slot first,
/// **then** this program collects its child, **then** the helper forks - so the
/// grandchild is allocated out of the pool at the moment the number is free, and
/// it is the number under test rather than a coincidence of ordering.
fn main() {
    let mut to_helper = [0i32; 2];
    let mut from_helper = [0i32; 2];
    if unsafe { libc::pipe(to_helper.as_mut_ptr()) } != 0
        || unsafe { libc::pipe(from_helper.as_mut_ptr()) } != 0
    {
        println!("kslot: pipes failed");
        std::process::exit(1);
    }

    let helper = unsafe { libc::fork() };
    if helper == 0 {
        unsafe {
            libc::close(to_helper[1]);
            libc::close(from_helper[0]);
        }
        // Wait for the signal that our slot is taken and the number is free.
        let mut byte = [0u8; 1];
        if unsafe { libc::read(to_helper[0], byte.as_mut_ptr().cast(), 1) } != 1 {
            unsafe { libc::_exit(1) };
        }
        let grandchild = unsafe { libc::fork() };
        if grandchild == 0 {
            loop {
                unsafe { libc::pause() };
            }
        }
        let text = format!("{grandchild}");
        let written = unsafe {
            libc::write(
                from_helper[1],
                text.as_ptr().cast(),
                text.len() as libc::size_t,
            )
        };
        let _ = written;
        // Hold the slot until the parent says it has finished reading.
        let _ = unsafe { libc::read(to_helper[0], byte.as_mut_ptr().cast(), 1) };
        unsafe { libc::_exit(0) };
    }
    unsafe {
        libc::close(to_helper[0]);
        libc::close(from_helper[1]);
    }

    // A child of our own, collected immediately: this is the number we hold.
    let mine = unsafe { libc::fork() };
    if mine == 0 {
        unsafe { libc::_exit(0) };
    }
    let mut status = 0i32;
    unsafe { libc::waitpid(mine, &mut status, 0) };
    println!("my child {mine} collected");

    // Now let somebody else's child take the pool slot this number just left.
    let _ = unsafe { libc::write(to_helper[1], b"g".as_ptr().cast(), 1) };
    let mut buffer = [0u8; 32];
    let read = unsafe {
        libc::read(
            from_helper[0],
            buffer.as_mut_ptr().cast(),
            buffer.len() as libc::size_t,
        )
    };
    let stranger: libc::pid_t = std::str::from_utf8(&buffer[..read.max(0) as usize])
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(-1);
    println!("stranger (helper's child) {stranger}");

    // The two questions. Linux: ESRCH and ENOENT, both 0-answered here.
    let probe = unsafe { libc::kill(mine, 0) };
    let probe_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    println!(
        "kill({mine}, 0) = {probe} errno={probe_errno} ({})",
        if probe == -1 && probe_errno == libc::ESRCH {
            "ESRCH"
        } else {
            "NOT ESRCH"
        }
    );
    println!("the stale number: /proc/{mine} -> {}", state_of(mine));

    let terminated = unsafe { libc::kill(mine, libc::SIGTERM) };
    let terminated_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    println!(
        "kill({mine}, SIGTERM) = {terminated} errno={terminated_errno} ({})",
        if terminated == -1 && terminated_errno == libc::ESRCH {
            "ESRCH"
        } else {
            "NOT ESRCH"
        }
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("after SIGTERM: /proc/{mine} -> {}", state_of(mine));

    if stranger == mine {
        println!("NOTE: the stranger reused my number");
    }
    // The one thing that *is* Linux's here, and is pinned as a check elsewhere.
    let mut again = 0i32;
    let second = unsafe { libc::waitpid(mine, &mut again, libc::WNOHANG) };
    let second_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    println!(
        "waitpid({mine}, WNOHANG) = {second} errno={second_errno} ({})",
        if second == -1 && second_errno == libc::ECHILD {
            "ECHILD - Linux's answer, kept"
        } else {
            "NOT Linux's answer"
        }
    );

    // Let the helper and its child go, so this program exits rather than leaving
    // two tasks for the runner to reap.
    let _ = unsafe { libc::write(to_helper[1], b"x".as_ptr().cast(), 1) };
    unsafe { libc::kill(stranger, libc::SIGKILL) };
    let _ = unsafe { libc::waitpid(helper, &mut status, 0) };
    println!("DONE");
    // A fixed, deliberate 0: the readings are the point, and a non-zero exit
    // would report this program as a failed *check* on a machine whose gap the
    // roadmap already names.
    std::process::exit(0)
}
