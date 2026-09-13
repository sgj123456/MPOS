//! cat: every file named on the command line, in order, to fd 1. With no
//! arguments it copies stdin, the way the manual says.
//!
//! A Rust `std` program, built for `x86_64-unknown-linux-musl`: `std::fs`,
//! `std::io` and `std::env` reach the kernel through musl, over the Linux
//! startup contract the kernel lays out for any Linux binary. Nothing here
//! knows mpos exists - which is the point of the std-built programs in this
//! directory, and what `t_stdargs` pins from the other side.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut out = io::stdout().lock();
    let mut names = std::env::args_os().skip(1).peekable();
    let mut failed = false;

    if names.peek().is_none() {
        // No operands: stdin is the file, as POSIX says.
        let stdin = io::stdin();
        failed = copy_to(&mut stdin.lock(), &mut out).is_err();
    } else {
        for name in names {
            match File::open(&name) {
                Ok(mut file) => failed |= copy_to(&mut file, &mut out).is_err(),
                Err(_) => {
                    // The operand is reported as it was given: a name that is
                    // not valid UTF-8 is still a name.
                    let _ = writeln!(io::stderr(), "cat: {}: cannot open", name.to_string_lossy());
                    failed = true;
                }
            }
        }
    }

    // A write that only reached the buffer is not a write yet.
    failed |= out.flush().is_err();
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// One reader to stdout, until end of input or the first real error. A short
/// read is not the end - only a zero-length one is, which is what a pipe hands
/// back once its write end is gone.
fn copy_to(input: &mut impl Read, out: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => out.write_all(&buf[..n])?,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}
