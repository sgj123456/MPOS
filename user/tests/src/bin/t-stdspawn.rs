//! t-stdspawn - can a program built on Rust's *standard library* start and drive
//! other programs on mpos?
//!
//! This is the question the std `sh` stands or falls on, so it is asked here,
//! directly, with the same primitives the shell uses:
//!
//! * `Command::status()` - start a child and wait for it;
//! * `Stdio::piped()` on one command and `Stdio::from(ChildStdout)` on the next -
//!   which is how a pipeline is built without the shell ever touching the data;
//! * `File` standing in for stdout - which is what `>` is;
//! * `env::set_current_dir` / `current_dir` - the `cd` and `pwd` builtins;
//! * an exit status that is not success, read the way a shell reads `$?`.
//!
//! Underneath, `std::process::Command` is `posix_spawn`, which musl implements as
//! `clone(CLONE_VM|CLONE_VFORK)`. Every check below is therefore also a check on
//! that flag word: if the kernel refused it, none of this would run at all.
//!
//! One thing this deliberately does *not* use is `Command::output()`. That path
//! has the parent read its children's pipes itself, and on Linux it does so with
//! a non-blocking `fcntl` plus `poll` - neither of which this kernel implements.
//! A shell can live without it: the pipeline form below gets the same data to the
//! same place without the parent in the middle. It is worth knowing which half of
//! `std` works here, and this is the line between them.

use std::fs::File;
use std::process::{Command, Stdio};

fn main() {
    let mut failed = false;
    println!("t-stdspawn: a std program is running");

    // 1. Start a child and wait for it.
    match Command::new("/bin/true").status() {
        Ok(status) => {
            if status.success() {
                println!("ok:   /bin/true ran and exited 0");
            } else {
                println!("FAIL: /bin/true exited {status}");
                failed = true;
            }
        }
        Err(e) => {
            println!("FAIL: /bin/true could not be spawned: {e}");
            failed = true;
        }
    }

    // 2. A pipeline, built the way a shell builds one: the first command writes
    //    into a pipe, the second reads out of it, and the shell holds neither end.
    //    "std-pipe-ok" proves the bytes made it through - the console shows it.
    let piped = Command::new("/bin/echo")
        .arg("std-pipe-ok")
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut first| {
            let pipe = first.stdout.take().expect("piped stdout");
            let second = Command::new("/bin/cat").stdin(Stdio::from(pipe)).status();
            let _ = first.wait();
            second
        });
    match piped {
        Ok(status) if status.success() => println!("ok:   a two-stage pipeline ran to completion"),
        Ok(status) => {
            println!("FAIL: the pipeline's reader exited {status}");
            failed = true;
        }
        Err(e) => {
            println!("FAIL: a pipeline could not be built: {e}");
            failed = true;
        }
    }

    // 3. A redirection, and the file it left behind: `>` is a `File` in the
    //    child's stdout slot.
    let path = "/tmp/stdspawn.txt";
    let redirected = File::create(path).map_err(|e| e.to_string()).and_then(|file| {
        Command::new("/bin/echo")
            .arg("through-a-file")
            .stdout(Stdio::from(file))
            .status()
            .map_err(|e| e.to_string())
    });
    match redirected {
        Ok(status) if status.success() => match std::fs::read_to_string(path) {
            Ok(text) if text.trim_end() == "through-a-file" => {
                println!("ok:   a redirected child wrote the file the shell named");
            }
            Ok(text) => {
                println!("FAIL: {path} holds {text:?}");
                failed = true;
            }
            Err(e) => {
                println!("FAIL: {path} could not be read back: {e}");
                failed = true;
            }
        },
        Ok(status) => {
            println!("FAIL: the redirected child exited {status}");
            failed = true;
        }
        Err(e) => {
            println!("FAIL: a redirection could not be set up: {e}");
            failed = true;
        }
    }

    // 4. The working directory, which is what `cd` and `pwd` are built on.
    match (std::env::set_current_dir("/tmp"), std::env::current_dir()) {
        (Ok(()), Ok(here)) if here == std::path::Path::new("/tmp") => {
            println!("ok:   cd moved the shell and pwd read the move back");
        }
        (Ok(()), Ok(here)) => {
            println!("FAIL: pwd reported {here:?} after cd /tmp");
            failed = true;
        }
        (Err(e), _) => {
            println!("FAIL: cd /tmp failed: {e}");
            failed = true;
        }
        (_, Err(e)) => {
            println!("FAIL: pwd failed: {e}");
            failed = true;
        }
    }
    let _ = std::env::set_current_dir("/");

    // 5. An exit that is not success, which is what `$?` has to report.
    match Command::new("/bin/false").status() {
        Ok(status) if status.success() => {
            println!("FAIL: /bin/false reported success");
            failed = true;
        }
        Ok(status) => {
            println!("ok:   /bin/false reported failure ({status})");
        }
        Err(e) => {
            println!("FAIL: /bin/false could not be spawned: {e}");
            failed = true;
        }
    }

    if failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
