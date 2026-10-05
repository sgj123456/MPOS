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
//! * an exit status that is not success, read the way a shell reads `$?`;
//! * `/bin/ls`, the command a session is explored with, and `sh` reading its
//!   commands from a *pipe* rather than from `-c` - the standard-input arm the
//!   interactive loop is built on.
//!
//! Underneath, `std::process::Command` is `posix_spawn`, which musl implements as
//! `clone(CLONE_VM|CLONE_VFORK)`. Every check below is therefore also a check on
//! that flag word: if the kernel refused it, none of this would run at all.
//!
//! One thing this used to avoid was `Command::output()`. That path has the
//! parent read both of the child's pipes itself, which std does by putting them
//! in non-blocking mode and then polling them - and on Linux the non-blocking
//! step is `ioctl(FIONBIO)`, not `fcntl` (that is the non-Linux arm). The
//! request was missing here, and every `output()` died with "Not a tty", which
//! is what `rustup-init` panicked of. It is checked below now that it exists.

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
    let redirected = File::create(path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
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

    // 6. `Command::output()`, the path where the parent reads both of the
    //    child's pipes itself. std puts them in non-blocking mode first - with
    //    `ioctl(FIONBIO)` on Linux - and then polls them, so this also pins
    //    that request. Both streams are checked, because it is the two-pipe
    //    branch of `read_output` that used to panic.
    match Command::new("/bin/sh")
        .arg("-c")
        .arg("echo captured-out; echo captured-err >&2")
        .output()
    {
        Ok(out)
            if out.status.success()
                && out.stdout == b"captured-out\n"
                && out.stderr == b"captured-err\n" =>
        {
            println!("ok:   output() captured both of the child's streams");
        }
        Ok(out) => {
            println!(
                "FAIL: output() gave {:?} out={:?} err={:?}",
                out.status, out.stdout, out.stderr
            );
            failed = true;
        }
        Err(e) => {
            println!("FAIL: output() failed: {e}");
            failed = true;
        }
    }

    // 7. `/bin/ls`, listed the way a shell session uses it. The *names* are what
    //    is checked, not the formatting: this program is busybox's `ls` on the
    //    first machine and this repository's own applet on the second, and the
    //    one thing both owe a caller is the directory's names, one per line. A
    //    directory made here rather than a system one also pins that the listing
    //    is of the path it was given and not of the current directory.
    let lsdir = "/tmp/stdspawn-ls";
    let _ = std::fs::remove_dir_all(lsdir);
    let made = std::fs::create_dir(lsdir)
        .and_then(|()| std::fs::write(format!("{lsdir}/alpha"), b""))
        .and_then(|()| std::fs::write(format!("{lsdir}/beta"), b""));
    match made.and_then(|()| Command::new("/bin/ls").arg(lsdir).output()) {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let names: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            if names == ["alpha", "beta"] {
                println!("ok:   /bin/ls listed the directory's names");
            } else {
                println!("FAIL: ls {lsdir} listed {names:?}");
                failed = true;
            }
        }
        Ok(out) => {
            println!("FAIL: ls exited {}", out.status);
            failed = true;
        }
        Err(e) => {
            println!("FAIL: ls could not be spawned: {e}");
            failed = true;
        }
    }
    // The scratch tree is emptied and removed *by hand* rather than with
    // `std::fs::remove_dir_all`, and that is a named gap rather than tidiness.
    // The first version of this check cleaned up with `remove_dir_all`, and the
    // second machine faulted the process at `pc 0` inside the call - reproduced
    // three times, always with the `ok:` line above as the log's last line and
    // the marker written immediately before `remove_dir_all` never reached. A
    // *non-empty* `remove_dir_all` had never run on that machine before, so what
    // it exercises and the three calls below do not - the recursive
    // open/`getdents64` chain and the empty-name `unlinkat(fd, "",
    // AT_REMOVEDIR)` it finishes with - is not served there yet. It is named in
    // `PLAN.md` §13.9 with this reproduction rather than papered over, and the
    // hand-rolled equivalent below is what this check uses because its subject
    // is `ls`, not deleting a tree.
    if let Ok(entries) = std::fs::read_dir(lsdir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(lsdir);

    // 8. `sh` reading its commands from standard input rather than from `-c`.
    //    This is the arm an interactive session is built on - the terminal case
    //    of the same loop reads a line and runs it - and it is the arm a rewrite
    //    of the shell is most likely to break, because the reader used to slurp
    //    standard input to end-of-file. A script arriving on a pipe still has to
    //    run every command, in order, with no prompt written for a reader that
    //    is not a terminal.
    let scripted = Command::new("/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            {
                use std::io::Write;
                let stdin = child.stdin.as_mut().expect("piped stdin");
                stdin.write_all(b"echo stdin-first\necho stdin-second\n")?;
            }
            child.wait_with_output()
        });
    match scripted {
        Ok(out) if out.status.success() && out.stdout == b"stdin-first\nstdin-second\n" => {
            println!("ok:   sh ran a script fed on standard input");
        }
        Ok(out) => {
            println!(
                "FAIL: sh on a pipe gave {} out={:?}",
                out.status,
                String::from_utf8_lossy(&out.stdout)
            );
            failed = true;
        }
        Err(e) => {
            println!("FAIL: sh on a pipe could not be run: {e}");
            failed = true;
        }
    }

    // 9. `cat FILE` - the *file* argument, not only the standard input the
    //    pipeline above feeds it. A `cat` that copied nothing but its input would
    //    sit on the terminal instead of failing, and this machine's `/bin` is a
    //    shell's whole toolset, so a session with no way to read a file is a
    //    session with no way to read the disk.
    let catfile = "/tmp/stdspawn-cat.txt";
    match std::fs::write(catfile, b"cat-from-a-file\n")
        .and_then(|()| Command::new("/bin/cat").arg(catfile).output())
    {
        Ok(out) if out.status.success() && out.stdout == b"cat-from-a-file\n" => {
            println!("ok:   /bin/cat read the file it was named");
        }
        Ok(out) => {
            println!(
                "FAIL: cat {catfile} gave {} out={:?}",
                out.status,
                String::from_utf8_lossy(&out.stdout)
            );
            failed = true;
        }
        Err(e) => {
            println!("FAIL: cat could not be run: {e}");
            failed = true;
        }
    }
    let _ = std::fs::remove_file(catfile);

    if failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
