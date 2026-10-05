//! The second machine's `/bin`: the programs the user tests start.
//!
//! One caller, one payload. `t-stdspawn` and `t-socketpair` are tests *about
//! other programs*: the first runs `/bin/true`, `/bin/echo`, `/bin/cat`,
//! `/bin/false` and `/bin/sh -c`, the second runs `/bin/cat`. The x86_64 harness
//! has those because its test root is an initramfs with busybox in `/bin`; the
//! second machine's root is the ext4 disk the sweep builds, which carried no
//! `/bin` at all - so both programs sat in `tests.toml`'s `aarch64` group, a gap
//! in the harness rather than in the kernel's spawn path (which every program in
//! the payload already exercises, since the runner starts each one through
//! `posix_spawn`). The interactive boot used to stage this same image as its
//! shell; since §13.19 it boots the real Alpine tree instead, whose busybox is
//! the multi-call `/bin` of a distribution rather than one built here. What
//! stays is the sweep's need: a gate that fetches nothing answers for the
//! kernel alone.
//!
//! # Why these are built in-tree rather than fetched
//!
//! The obvious way to fill `/bin` is to install busybox, the way
//! [`crate::initramfs`] does for the x86_64 image. This payload does not, and the
//! reason is the one `tests.toml` states as the whole point of the sweep's
//! groups: **a red gate has to mean the kernel**. Installing busybox means asking
//! an Alpine mirror for an aarch64 package at build time - a network fetch inside
//! the aarch64 gate, whose failure would read as a broken kernel, plus a new host
//! prerequisite (`apk`, and a working route to the mirror) that no other aarch64
//! command needs. The toolchain, by contrast, is *already* required: the sweep's
//! own runner ([`crate::aarch64_init::build_sweep_init`]) and its musl helper are
//! static `aarch64-unknown-linux-musl` binaries built by `rustc`, and so is this.
//!
//! So the tools are [`APPLETS`]' five names, one multi-call binary, built the same
//! way. That is busybox's own shape - a single image dispatched on the name it was
//! run as - and it is honestly *not* busybox: it is a handful of programs, and
//! [`TOOLS_SRC`]'s own comment says exactly which shell the `sh` applet is.
//!
//! # What is pinned
//!
//! Every applet here is pinned by a program that runs it: `true`, `false` and
//! `echo` by `t-stdspawn`'s exit-status, pipeline and redirection checks, `cat` by
//! that pipeline and by `t-socketpair`'s descriptor-survives-`execve` check, and
//! `sh` by `t-stdspawn`'s `Command::output()` check - the one that reads *both* of
//! a child's pipes, which is why the shell has to honour `>&2` onto the real
//! descriptor rather than print into a buffer of its own.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// `EM_AARCH64` - the machine the loader must see in the built image.
const EM_AARCH64: u16 = 183;

/// The names the tools are staged under, in `/bin`. A program that asks for one
/// of them runs the same image with that name as its `argv[0]`, which is what
/// the image dispatches on - busybox's arrangement, and the reason there is one
/// build here and not six.
pub const APPLETS: [&str; 6] = ["true", "false", "echo", "cat", "ls", "sh"];

/// The six applets, as one program.
///
/// Deliberately small programs, so every byte of behaviour is here to be read:
/// `true` and `false` are their exit status, `echo` joins its arguments with
/// spaces and a newline, `cat` copies standard input or the files it is named,
/// `ls` lists the names in a directory, and `sh` runs a command string or, on a
/// terminal, the commands it is typed.
///
/// No file, directory or descriptor the kernel owns is touched except through
/// libc calls every one of which this machine has already been asked to serve
/// by the sweep's other programs (`read`, `write`, `dup`, `dup2`, `close`, the
/// `open`/`execve`/`wait4` behind `std::process::Command`).
pub const TOOLS_SRC: &str = r##"
//! Six programs in one image, dispatched on the name it was run as.
//!
//! `true` and `false` are their exit status; `echo` writes its arguments and a
//! newline to standard output; `cat` copies standard input - or the files it is
//! named - to standard output; `ls` lists a directory's names; `sh` is the small
//! shell its own comment describes. Every one of them is reached by the name in
//! `/bin`, which is how a multi-call binary works and how busybox's own applets
//! do - this image is not busybox, it is these six.

use std::io::{IsTerminal, Read, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::fs::PermissionsExt;

// The libc calls this program makes that `std` does not expose: the shell
// redirects with the *real* descriptors, so a builtin and a child process see
// the same thing, and its prompt reports the identity it is really running as.
// Reading and writing need no declaration - `std::io`'s `stdin`/`stdout` are
// fd 0 and fd 1, and a `dup2` onto either moves them with the descriptor.
unsafe extern "C" {
    fn dup(fd: i32) -> i32;
    fn dup2(old: i32, new: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn geteuid() -> u32;
}

fn main() {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let name = argv0.rsplit('/').next().unwrap_or(&argv0).to_string();
    let rest: Vec<String> = args.collect();
    let status = match name.as_str() {
        "true" => 0,
        "false" => 1,
        "echo" => applet_echo(&rest),
        "cat" => applet_cat(&rest),
        "ls" => applet_ls(&rest),
        "sh" => applet_sh(&rest),
        other => {
            eprintln!(
                "{other}: not an applet of this multi-call binary (true, false, echo, cat, ls, sh)"
            );
            127
        }
    };
    std::process::exit(status);
}

/// `echo`: the arguments, one space between them, then a newline. No options -
/// `echo -n` would print "-n", which is what a shell without options does and
/// what none of the tests ask for.
fn applet_echo(args: &[String]) -> i32 {
    let mut line = args.join(" ");
    line.push('\n');
    write_stdout(line.as_bytes())
}

/// Write every byte to standard output, or fail. Nothing is left in a buffer:
/// the shell's `>&2` reaches the descriptor because this flushes, and a `write`
/// that short-counted on a pipe is retried by `write_all`.
fn write_stdout(bytes: &[u8]) -> i32 {
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// `cat`: with no argument, standard input to standard output; with arguments,
/// each named file in turn. A buffer at a time and streamed rather than slurped,
/// which is what makes it usable as the far end of a pipe - the shape both tests
/// use it in.
///
/// The file arguments are not decoration. This image is a shell's whole toolset,
/// so a `cat` that could only echo its input would leave a session with no way to
/// read a file, and the failure is not an error message: the program sits on the
/// terminal waiting for a line the person has already typed. Measured exactly
/// that on the machine - `cat /etc/inittab` blocked until the next keystroke and
/// then printed *that* - which is how a shell's first real command turned into a
/// hang.
fn applet_cat(args: &[String]) -> i32 {
    if args.is_empty() {
        return copy_to_stdout(&mut std::io::stdin().lock());
    }
    let mut status = 0;
    for name in args {
        match std::fs::File::open(name) {
            Ok(mut file) => {
                if copy_to_stdout(&mut file) != 0 {
                    status = 1;
                }
            }
            Err(e) => {
                eprintln!("cat: {name}: {e}");
                status = 1;
            }
        }
    }
    status
}

/// Every byte of `input` on standard output, or a failure status.
fn copy_to_stdout(input: &mut impl Read) -> i32 {
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() {
                    return 1;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 1,
        }
    }
    if out.flush().is_err() {
        return 1;
    }
    0
}

/// `ls`: the names in each directory argument, or in the current directory when
/// none is given, one per line. Several arguments are headed with the name they
/// came from, which is the shape `ls dir1 dir2` has and the reason a bare list
/// is not one.
///
/// Deliberately not `ls -l`: the options a real `ls` carries are a program of
/// their own, and what a shell session needs first is to see what is there.
/// `std::fs::read_dir` is `getdents64` and the name comes from the dirent, so
/// this is the same call the shell's own completion would use.
fn applet_ls(args: &[String]) -> i32 {
    let dirs: Vec<&str> = if args.is_empty() {
        vec!["."]
    } else {
        args.iter().map(String::as_str).collect()
    };
    let headed = dirs.len() > 1;
    let mut status = 0;
    for (i, dir) in dirs.iter().enumerate() {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("ls: {dir}: {e}");
                status = 1;
                continue;
            }
        };
        if headed {
            if i > 0 {
                println!();
            }
            println!("{dir}:");
        }
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            println!("{name}");
        }
    }
    status
}

/// `sh`: a small shell, and only what this comment says it is.
///
/// It runs the string after `-c` - or, with no `-c`, the commands it reads from
/// standard input - as a sequence of simple commands separated by `;` or a
/// newline. Each command is a word list with `'...'`, `"..."` and `\` quoting, an
/// optional redirection (`< FILE`, `> FILE`, `>> FILE`, `>&N`), and either a
/// builtin (`echo`, `exit`, `cd`, `pwd`) or an external program by path or from
/// `PATH`.
///
/// It is deliberately *not* a POSIX shell and does not claim to be. There are no
/// variables or expansion, no `$?`, no pipes, no `&&`/`||`, no globbing, no
/// functions, no control flow and no `#` comments. What it has to get right is
/// what `t-stdspawn` asks of it: `echo captured-out; echo captured-err >&2` has
/// to put the first line on standard output and the second on standard error,
/// which is why the redirection moves the real descriptor instead of being
/// modelled inside this program.
///
/// Both shapes a shell is used in are the same loop over [`run_script`], and the
/// difference is only where the script comes from: `-c` takes it as an argument,
/// and no argument reads it a line at a time from standard input - see
/// [`repl`], which is also where the interactive boot's prompt comes from.
fn applet_sh(args: &[String]) -> i32 {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-c" {
            let Some(script) = args.get(i + 1) else {
                eprintln!("sh: -c needs a command");
                return 2;
            };
            return run_script(script);
        }
        i += 1;
    }
    repl()
}

/// Read commands a line at a time and run each as it arrives.
///
/// One loop covers both shapes a shell reads a script in. With a terminal it
/// prompts and runs what a person types; without one it runs a file or a pipe,
/// silently. A prompt is written only when the input is a terminal
/// ([`IsTerminal`], i.e. `ioctl(TCGETS)`), which is `ash`'s rule and the reason a
/// script fed on standard input does not get its output interleaved with
/// prompting.
///
/// A line at a time rather than to end-of-file is what makes the terminal case
/// work at all: a canonical read returns as soon as the line is complete, so a
/// command runs when Enter is pressed instead of waiting for `Ctrl-D`. It also
/// means the two cases agree on semantics - [`run_script`] splits on newlines
/// and `;` either way, so a piped script runs the same commands in the same
/// order.
///
/// The prompt reports the identity the shell is really running as, the way every
/// shell's does: `#` for root, `$` otherwise. It goes to standard *error*, which
/// is where a diagnostic belongs and what keeps it out of a redirected command's
/// output.
fn repl() -> i32 {
    let interactive = std::io::stdin().is_terminal();
    let prompt = if unsafe { geteuid() } == 0 { "# " } else { "$ " };
    let mut status = 0;
    let mut line = String::new();
    loop {
        if interactive {
            eprint!("{prompt}");
        }
        line.clear();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => status = run_script(&line),
            // A signal that ended the read is not the end of the session; the
            // shell is the process the console's `Ctrl-C` lands on, and a shell
            // that exited on one would take the session with it.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("sh: read: {e}");
                return 2;
            }
        }
    }
    if interactive {
        // A shell leaving a terminal owes it a fresh line; a pipe does not care.
        eprintln!();
    }
    status
}

/// Run a whole script: `;` inside a line, and a newline between lines. The
/// status is the last command's, which is what a shell reports.
fn run_script(text: &str) -> i32 {
    let mut status = 0;
    for line in text.lines() {
        for command in line.split(';') {
            let command = command.trim();
            if command.is_empty() {
                continue;
            }
            status = run_command(command);
        }
    }
    status
}

/// Run one simple command: split off its redirections, apply them, and run it.
fn run_command(line: &str) -> i32 {
    let words = split_words(line);
    let mut argv: Vec<&str> = Vec::new();
    let mut from: Option<&str> = None;
    let mut to: Option<&str> = None;
    let mut append = false;
    let mut to_fd: Option<i32> = None;
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            ">" | ">>" => {
                let Some(target) = words.get(i + 1) else { break };
                to = Some(target);
                append = words[i] == ">>";
                i += 2;
            }
            "<" => {
                let Some(target) = words.get(i + 1) else { break };
                from = Some(target);
                i += 2;
            }
            word if word.starts_with(">&") => {
                match word[2..].parse::<i32>() {
                    Ok(fd) => to_fd = Some(fd),
                    Err(_) => {
                        eprintln!("sh: {word}: not a descriptor");
                        return 2;
                    }
                }
                i += 1;
            }
            word => {
                argv.push(word);
                i += 1;
            }
        }
    }
    if argv.is_empty() {
        return 0;
    }

    // Open both ends before redirecting anything, so a failure leaves the shell
    // as it found it rather than half-wired.
    let in_fd = match from {
        Some(path) => match std::fs::File::open(path) {
            Ok(file) => Some(file.into_raw_fd()),
            Err(e) => {
                eprintln!("sh: {path}: {e}");
                return 1;
            }
        },
        None => None,
    };
    let out_fd = match to {
        Some(path) => {
            let opened = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(path);
            match opened {
                Ok(file) => Some(file.into_raw_fd()),
                Err(e) => {
                    eprintln!("sh: {path}: {e}");
                    if let Some(fd) = in_fd {
                        unsafe { close(fd) };
                    }
                    return 1;
                }
            }
        }
        None => None,
    };

    // The redirection moves *our* descriptors, so a builtin and a child see the
    // same thing: `echo x >&2` writes to fd 2 because fd 1 became fd 2, and a
    // child inherits it because a `Command`'s default is `Stdio::inherit`.
    let saved_out = unsafe { dup(1) };
    let saved_in = unsafe { dup(0) };
    if let Some(fd) = in_fd {
        unsafe { dup2(fd, 0) };
    }
    if let Some(fd) = out_fd {
        unsafe { dup2(fd, 1) };
    } else if let Some(fd) = to_fd {
        unsafe { dup2(fd, 1) };
    }

    let status = run_simple(&argv);

    restore(saved_in, 0);
    restore(saved_out, 1);
    if let Some(fd) = in_fd {
        unsafe { close(fd) };
    }
    if let Some(fd) = out_fd {
        unsafe { close(fd) };
    }
    status
}

/// Put `fd` back the way `run_command` found it, and hang up the saved copy.
fn restore(saved: i32, fd: i32) {
    if saved >= 0 {
        unsafe {
            dup2(saved, fd);
            close(saved);
        }
    }
}

/// A builtin, or an external program.
fn run_simple(argv: &[&str]) -> i32 {
    match argv[0] {
        "echo" => {
            let mut line = argv[1..].join(" ");
            line.push('\n');
            write_stdout(line.as_bytes())
        }
        "exit" => {
            let code = argv
                .get(1)
                .and_then(|word| word.parse::<i32>().ok())
                .unwrap_or(0);
            std::process::exit(code);
        }
        "cd" => {
            let dir = argv.get(1).copied().unwrap_or("/");
            match std::env::set_current_dir(dir) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("sh: cd: {dir}: {e}");
                    1
                }
            }
        }
        "pwd" => match std::env::current_dir() {
            Ok(dir) => {
                let mut line = dir.to_string_lossy().into_owned();
                line.push('\n');
                write_stdout(line.as_bytes())
            }
            Err(e) => {
                eprintln!("sh: pwd: {e}");
                1
            }
        },
        name => run_external(name, &argv[1..]),
    }
}

/// Run a program: the path as written when it holds a `/`, otherwise a walk down
/// `PATH`. The child inherits the descriptors the redirection just moved.
fn run_external(name: &str, args: &[&str]) -> i32 {
    let program = if name.contains('/') {
        name.to_string()
    } else {
        match search_path(name) {
            Some(path) => path,
            None => {
                eprintln!("sh: {name}: not found");
                return 127;
            }
        }
    };
    match std::process::Command::new(&program).args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("sh: {program}: {e}");
            127
        }
    }
}

/// The first executable named `name` in `PATH` (`/bin:/usr/bin` when it is
/// unset). Running is the question, so the mode bit is checked and not only the
/// existence.
fn search_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_string());
    for dir in path.split(':') {
        let candidate = if dir.is_empty() {
            format!("/{name}")
        } else {
            format!("{dir}/{name}")
        };
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

/// Split one command line into words, dropping the quotes and honouring `\` -
/// outside quotes and inside double ones, which is the whole of the shell's
/// quoting. A quote that is never closed runs to the end of the line rather than
/// being an error; a shell this size has nobody to report it to.
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' | '"' => {
                started = true;
                for c in chars.by_ref() {
                    if c == ch {
                        break;
                    }
                    word.push(c);
                }
            }
            '\\' => {
                started = true;
                if let Some(c) = chars.next() {
                    word.push(c);
                }
            }
            _ => {
                started = true;
                word.push(ch);
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}
"##;

/// Build the multi-call binary and answer its bytes.
///
/// `rustc` for `aarch64-unknown-linux-musl` with `rust-lld`, exactly the flags
/// [`crate::aarch64_init::build_sweep_init`] uses: static, stripped, optimized.
/// The target is a prerequisite the sweep already needs (the runner and the musl
/// helper are built the same way); a missing one is an error naming the install
/// command rather than a silent skip.
pub fn build(build_dir: &Path) -> Result<Vec<u8>> {
    let src = build_dir.join("aarch64-bin.rs");
    let out = build_dir.join("aarch64-bin");
    std::fs::write(&src, TOOLS_SRC)
        .with_context(|| format!("Failed to write {}", src.display()))?;

    let status = Command::new("rustc")
        .arg("--target")
        .arg("aarch64-unknown-linux-musl")
        .args(["--edition", "2021"])
        .args(["-C", "target-feature=+crt-static"])
        .args(["-C", "linker=rust-lld"])
        .args(["-C", "strip=symbols"])
        .arg("-O")
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .context("Failed to run rustc for the aarch64 /bin tools")?;
    if !status.success() {
        bail!(
            "rustc failed building the aarch64 /bin tools ({status}); the \
             aarch64-unknown-linux-musl target must be installed: \
             `rustup target add aarch64-unknown-linux-musl`"
        );
    }
    let bytes = std::fs::read(&out).with_context(|| format!("Failed to read {}", out.display()))?;
    if bytes.len() < 20
        || &bytes[0..4] != b"\x7fELF"
        || u16::from_le_bytes([bytes[18], bytes[19]]) != EM_AARCH64
    {
        bail!("the aarch64 /bin tools are not an EM_AARCH64 ELF");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The applet list and the dispatcher have to agree: a name staged in `/bin`
    /// whose arm is missing runs the image and gets "not an applet", which reads
    /// as a broken program rather than a broken payload. The names are also the
    /// tests' - `t-stdspawn` runs true/false/echo/cat/sh - and the interactive
    /// boot's, where `sh` is pid 1 and `ls` is how a person sees the disk - so
    /// this is the list one shell session and two spawn tests need.
    #[test]
    fn every_staged_name_is_an_applet_the_dispatcher_knows() {
        for applet in APPLETS {
            assert!(
                TOOLS_SRC.contains(&format!("\"{applet}\"")),
                "APPLETS names {applet} but the dispatcher never matches it"
            );
        }
        for required in ["true", "false", "echo", "cat", "ls", "sh"] {
            assert!(APPLETS.contains(&required), "the tools lost {required}");
        }
    }

    /// The shell has to answer `-c`, it has to move a *real* descriptor for
    /// `>&2`, and it has to keep the interactive loop. `t-stdspawn` asserts the
    /// child put `captured-err` on standard error, so a shell that printed both
    /// lines to standard output would fail it while looking like it had run the
    /// script. The last of the three is what `run --arch aarch64` is: a shell
    /// that slurped standard input to end-of-file instead of reading a line at a
    /// time would still pass every spawn test and never give a prompt. Cheap
    /// checks, but they are the pieces a rewrite of the shell must keep - the
    /// real pin is the program that runs it.
    #[test]
    fn the_shell_keeps_the_c_prompt_a_real_redirection_and_a_repl() {
        assert!(TOOLS_SRC.contains("\"-c\""), "the `-c` form has no arm");
        assert!(
            TOOLS_SRC.contains("dup2(fd, 1)"),
            "the redirection does not reach the descriptor"
        );
        assert!(
            TOOLS_SRC.contains("starts_with(\">&\")"),
            "the `>&N` form has no parser"
        );
        assert!(
            TOOLS_SRC.contains("read_line(&mut line)") && TOOLS_SRC.contains("is_terminal()"),
            "the interactive loop is gone: a shell that reads to end-of-file never prompts"
        );
    }
}
