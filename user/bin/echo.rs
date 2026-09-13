//! echo: its arguments, one space apart, one newline at the end.
//!
//! A Rust `std` program, built for `x86_64-unknown-linux-musl`: `std::env`
//! reads the argument block through musl's startup, and `std::io` writes to fd
//! 1. Nothing here knows mpos exists - which is what the shell's `echo` lines
//! and the std programs beside this one make checkable.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Operands are bytes, not `String`s: the shell hands over whatever it was
    // given, and a word that is not valid UTF-8 is still a word to echo.
    let mut line = Vec::new();
    for (index, arg) in std::env::args_os().enumerate().skip(1) {
        if index > 1 {
            line.push(b' ');
        }
        line.extend_from_slice(arg.as_bytes());
    }
    line.push(b'\n');

    let mut out = io::stdout().lock();
    let written = out.write_all(&line).and_then(|()| out.flush());
    if written.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
