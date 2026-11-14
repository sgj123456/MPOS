//! `FUTEX_LOCK_PI`, `FUTEX_UNLOCK_PI` and `FUTEX_TRYLOCK_PI`.
//!
//! The words a libc uses once a program asks a mutex for priority-inheritance
//! attributes. Everything here is Linux's own answer, measured on a current
//! host first (`build/scratch/pifutex/probe.c`): the word carries the owner's
//! task id in its low thirty bits with `FUTEX_WAITERS` and `FUTEX_OWNER_DIED`
//! above it, the owner's own repeat is `EDEADLK`, a stranger's unlock is
//! `EPERM`, a contended trylock is `EAGAIN` and leaves the waiters bit set,
//! and the operations refuse a clock flag with `ENOSYS`.
//!
//! The last two claims are the ones the report that started this came from:
//! musl's `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` probes the
//! kernel with `FUTEX_LOCK_PI` and returns whatever that says - a kernel that
//! answers `ENOSYS` makes every caller of that call believe the *program* is
//! wrong, which is how pulseaudio's `pa_mutex_new` came to abort chromium.

use mpos_user_tests::Report;

use std::io::Error;

const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const FUTEX_WAITERS: u32 = 1 << 31;
const FUTEX_OWNER_DIED: u32 = 1 << 30;

/// One `futex(2)` on a word, with no timeout.
fn futex(word: *mut u32, op: i32) -> Result<isize, i32> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word,
            op,
            0u32,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        )
    };
    if rc < 0 {
        Err(Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(rc as isize)
    }
}

fn word_of(word: *mut u32) -> u32 {
    unsafe { std::ptr::read_volatile(word) }
}

fn owner(word: u32) -> u32 {
    word & FUTEX_TID_MASK
}

/// A page shared with a forked child, so both name the same word.
fn shared_word() -> *mut u32 {
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(page, libc::MAP_FAILED, "the shared page");
    page.cast::<u32>()
}

fn read_line(fd: i32) -> String {
    let mut line = [0u8; 64];
    let got = unsafe { libc::read(fd, line.as_mut_ptr().cast(), line.len()) };
    String::from_utf8_lossy(&line[..got.max(0) as usize]).to_string()
}

fn main() {
    let mut report = Report::new();
    let word = shared_word();
    let me = std::process::id();

    // A fresh word, taken.
    unsafe { *word = 0 };
    let taken = futex(word, libc::FUTEX_LOCK_PI);
    report.check("FUTEX_LOCK_PI takes a free word", taken == Ok(0));
    report.check("the word names its taker", owner(word_of(word)) == me);

    // The taker asking again.
    let again = futex(word, libc::FUTEX_LOCK_PI);
    report.check(
        "a second LOCK_PI by the owner is EDEADLK",
        again == Err(libc::EDEADLK),
    );
    let try_again = futex(word, libc::FUTEX_TRYLOCK_PI);
    report.check(
        "TRYLOCK_PI by the owner is EDEADLK",
        try_again == Err(libc::EDEADLK),
    );

    // Give it up.
    let unlocked = futex(word, libc::FUTEX_UNLOCK_PI);
    report.check("UNLOCK_PI by the owner answers 0", unlocked == Ok(0));
    report.check("and clears the word", word_of(word) == 0);

    // A free word, taken and given back by TRYLOCK_PI.
    unsafe { *word = 0 };
    let tried = futex(word, libc::FUTEX_TRYLOCK_PI);
    report.check("FUTEX_TRYLOCK_PI takes a free word", tried == Ok(0));
    report.check(
        "the trylock's word names its taker",
        owner(word_of(word)) == me,
    );
    report.check(
        "UNLOCK_PI after a trylock answers 0",
        futex(word, libc::FUTEX_UNLOCK_PI) == Ok(0),
    );

    // Somebody else's word.
    let stranger = futex(word, libc::FUTEX_UNLOCK_PI);
    report.check(
        "UNLOCK_PI by a task that does not own it is EPERM",
        stranger == Err(libc::EPERM),
    );

    // An address the word cannot live on.
    unsafe { *word = 0 };
    let misaligned = word.cast::<u8>().wrapping_add(1).cast::<u32>();
    report.check(
        "a misaligned address is EINVAL",
        futex(misaligned, libc::FUTEX_LOCK_PI) == Err(libc::EINVAL),
    );

    // A clock flag the operation does not take.
    report.check(
        "LOCK_PI with FUTEX_CLOCK_REALTIME is ENOSYS",
        futex(word, libc::FUTEX_LOCK_PI | libc::FUTEX_CLOCK_REALTIME) == Err(libc::ENOSYS),
    );

    // ---- the contended half: a child, two pipes, one handover -------------
    unsafe { *word = 0 };
    let mut report_pipe = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(report_pipe.as_mut_ptr()) }, 0);
    let mut go_pipe = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(go_pipe.as_mut_ptr()) }, 0);

    let ours = futex(word, libc::FUTEX_LOCK_PI);
    report.check("the parent holds the word before forking", ours == Ok(0));

    let child = unsafe { libc::fork() };
    assert!(child >= 0, "the child");
    if child == 0 {
        unsafe {
            libc::close(report_pipe[0]);
            libc::close(go_pipe[1]);
            // The blocked trylock first: it must answer EAGAIN and leave the
            // waiters bit set for the owner to see.
            let refused = futex(word, libc::FUTEX_TRYLOCK_PI);
            let after = word_of(word);
            let tried_line = format!(
                "{} {}\n",
                refused.err().unwrap_or(0),
                (after & FUTEX_WAITERS != 0) as u8
            );
            libc::write(report_pipe[1], tried_line.as_ptr().cast(), tried_line.len());
            // Then block in LOCK_PI until the parent hands the word over.
            let blocked = futex(word, libc::FUTEX_LOCK_PI);
            let learned = format!(
                "{} {} {}\n",
                blocked.err().unwrap_or(0),
                (owner(word_of(word)) == libc::getpid() as u32) as u8,
                (word_of(word) & FUTEX_OWNER_DIED != 0) as u8,
            );
            libc::write(report_pipe[1], learned.as_ptr().cast(), learned.len());
            // Hold it until the parent says it has seen the handover's word,
            // so the unlock below can find the parent queued as a waiter.
            let mut byte = [0u8; 1];
            libc::read(go_pipe[0], byte.as_mut_ptr().cast(), 1);
            let _ = futex(word, libc::FUTEX_UNLOCK_PI);
            libc::_exit(0);
        }
    }

    unsafe {
        libc::close(report_pipe[1]);
        libc::close(go_pipe[0]);
    }
    let text = read_line(report_pipe[0]);
    let mut fields = text.split_whitespace();
    let refused = fields.next().and_then(|f| f.parse::<i32>().ok());
    let waiters_set = fields.next() == Some("1");
    report.check(
        "a contended TRYLOCK_PI is EAGAIN",
        refused == Some(libc::EAGAIN),
    );
    report.check("and leaves FUTEX_WAITERS set", waiters_set);

    // Hand the word to the child, then let it go.
    let handed = futex(word, libc::FUTEX_UNLOCK_PI);
    report.check(
        "the owner's unlock answers 0 with a waiter present",
        handed == Ok(0),
    );
    let mut one = [1u8; 1];
    unsafe { libc::write(go_pipe[1], one.as_mut_ptr().cast(), 1) };
    let text2 = read_line(report_pipe[0]);
    let mut fields2 = text2.split_whitespace();
    let child_rc = fields2.next().and_then(|f| f.parse::<i32>().ok());
    let child_owns = fields2.next() == Some("1");
    let child_died = fields2.next() == Some("1");
    report.check(
        "the blocked LOCK_PI is handed the word and answers 0",
        child_rc == Some(0),
    );
    report.check("the handed word names the child", child_owns);
    report.check("a live owner's handover is not EOWNERDEAD", !child_died);

    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    report.check(
        "the child released the word on its way out",
        word_of(word) == 0,
    );

    // ---- the calls the report came from ----------------------------------
    unsafe {
        let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
        let a = libc::pthread_mutexattr_init(&mut attr);
        let b = libc::pthread_mutexattr_setprotocol(&mut attr, libc::PTHREAD_PRIO_INHERIT);
        let mut mutex: libc::pthread_mutex_t = std::mem::zeroed();
        let c = libc::pthread_mutex_init(&mut mutex, &attr);
        let d = libc::pthread_mutex_lock(&mut mutex);
        let e = libc::pthread_mutex_unlock(&mut mutex);
        let f = libc::pthread_mutex_destroy(&mut mutex);
        let g = libc::pthread_mutexattr_destroy(&mut attr);
        report.check(
            "pthread_mutexattr_setprotocol(PRIO_INHERIT) answers 0",
            a == 0 && b == 0,
        );
        report.check(
            "a PI mutex locks, unlocks and destroys",
            c == 0 && d == 0 && e == 0 && f == 0 && g == 0,
        );
    }

    let _ = FUTEX_OWNER_DIED;
    report.finish();
}
