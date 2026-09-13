//! sleep: do nothing for a whole number of seconds.

use std::env;
use std::process;
use std::thread;
use std::time::Duration;

fn main() {
    let mut args = env::args();
    let _prog = args.next();

    let Some(arg) = args.next() else {
        eprintln!("sleep: how long?");
        process::exit(1);
    };

    let mut secs: usize = 0;
    for b in arg.bytes() {
        if !b.is_ascii_digit() || secs > 100_000 {
            eprintln!("sleep: not a number");
            process::exit(1);
        }
        secs = secs * 10 + (b - b'0') as usize;
    }

    thread::sleep(Duration::from_secs(secs as u64));
    process::exit(0);
}
