//! The shell: a user program that reads lines from fd 0 and runs them.
//!
//! One word or many, the first names a program under /bin; the words after it
//! become its argv. The shell spawns it, waits for it, and prints nothing
//! unless something went wrong - the quiet contract between a shell and the
//! programs it runs. End of input (Ctrl-D on an empty line) is the way out.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

/// How many words one line may hold, and the longest line we will read.
const MAX_WORDS: usize = 16;
const LINE_MAX: usize = 256;

/// A word boundary. The newline is one because a canonical read hands back the
/// line it terminates: without it the last word of every command would carry
/// the Enter that ended it, and `/bin/true\n` is not a program.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    let mut line = [0u8; LINE_MAX];

    loop {
        if sys::write_all(1, b"$ ") < 0 {
            sys::exit(1);
        }
        // A canonical read: one line, newline included, 0 at end of input.
        let n = sys::read(0, &mut line);
        if n <= 0 {
            break;
        }
        let line = &line[..n as usize];

        let mut words: [&[u8]; MAX_WORDS] = [&[]; MAX_WORDS];
        let mut count = 0usize;
        let mut i = 0usize;
        while i < line.len() && count < MAX_WORDS {
            while i < line.len() && is_space(line[i]) {
                i += 1;
            }
            if i == line.len() {
                break;
            }
            let start = i;
            while i < line.len() && !is_space(line[i]) {
                i += 1;
            }
            words[count] = &line[start..i];
            count += 1;
        }
        if count == 0 {
            continue;
        }

        // Every command lives in /bin; there is no PATH to search yet.
        let mut path = [0u8; 80];
        if 5 + words[0].len() >= path.len() {
            sys::exit(1);
        }
        path[..5].copy_from_slice(b"/bin/");
        path[5..5 + words[0].len()].copy_from_slice(words[0]);
        let path = &path[..5 + words[0].len()];

        let pid = sys::spawn(path, &words[..count]);
        if pid < 0 {
            let _ = sys::write_all(2, b"sh: ");
            let _ = sys::write_all(2, words[0]);
            let _ = sys::write_all(2, b": cannot run\n");
            continue;
        }
        // The exit code is the child's own business; a shell this small does
        // not report it. A negative code is not a failure here - it is what a
        // program killed by a signal leaves behind (a Ctrl-C'd child exits
        // with -SIGINT), and only ECHILD means the wait itself went wrong.
        let code = sys::wait(pid);
        if code == -10 {
        }
    }

    sys::exit(0);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
