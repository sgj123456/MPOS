//! The shell: a user program that reads lines from fd 0 and runs them.
//!
//! The grammar lives in the kernel's library (`src/shell`) so it can be tested
//! where the tests are; what is left here is the executor - turning a parsed
//! line into spawned processes and waited-for exits. One word or many, the
//! first names a program under /bin; the words after it become its argv.
//! `a | b` runs the two through a pipe, `> path` sends a command's output to a
//! file, and `name=value` on its own remembers a variable for `$name` later.
//!
//! Linux ABI: each command is a `fork()` then `execve()` in the child, with
//! `pipe2()` and `dup2()` threading redirections between. On mpos `fork(2)`
//! answers ENOSYS today, so a pipeline without it cannot work - the shell
//! falls back to `execve()` directly, which replaces the shell with the
//! command (so a multi-command line is broken until fork lands).
//!
//! The quiet contract holds: the shell prints nothing unless something went
//! wrong, or unless someone asks what happened (`$?`).

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;
#[path = "../../src/shell/mod.rs"]
mod shell;

use core::panic::PanicInfo;

/// How long one line may be, and how much room a line's rewritten words may
/// take. Both are fixed: this program has no allocator.
const LINE_MAX: usize = 256;
const BUDGET_MAX: usize = 512;

/// How many variables a shell this size remembers. A name is short and a
/// value is shorter; sixteen of each is more than a session needs.
const VAR_MAX: usize = 16;
const NAME_MAX: usize = 16;
const VALUE_MAX: usize = 64;

/// The shell's memory: variables, jobs it started in the background, and the
/// last exit status `$?` reports.
struct Shell {
    vars: [([u8; NAME_MAX], [u8; VALUE_MAX]); VAR_MAX],
    var_len: usize,
    /// Background jobs: the pid to reap, and the name to report it by.
    jobs: [(isize, [u8; 24]); JOB_MAX],
    job_len: usize,
    /// The last command's exit status, as decimal digits. `$?` reads this.
    status: [u8; 12],
    status_len: usize,
    /// The same status as a number, for `exit` and for the signal report.
    code: isize,
    /// Where the shell is, as the caller typed it. The kernel keeps the real
    /// cwd; this is what `pwd` prints, because the kernel keeps no string.
    here: [u8; 128],
    here_len: usize,
}

/// How many background jobs one shell tracks. `sleep 60 &` three times over
/// still fits, with room to spare.
const JOB_MAX: usize = 8;

/// EAGAIN, by number: the libc pins the syscall table but not the errnos,
/// and this is the one the shell has to recognise (a job that is still
/// running, asked about without blocking).
const ERRNO_EAGAIN: isize = -11;

impl Shell {
    fn new() -> Shell {
        Shell {
            vars: [([0; NAME_MAX], [0; VALUE_MAX]); VAR_MAX],
            var_len: 0,
            jobs: [(0, [0u8; 24]); JOB_MAX],
            job_len: 0,
            status: [0u8; 12],
            status_len: 1,
            code: 0,
            here: { let mut h = [0u8; 128]; h[0] = b'/'; h },
            here_len: 1,
        }
    }

    /// Record the exit status a command left, the way `$?` will report it.
    ///
    /// A negative code is a death by signal, and the POSIX report for one is
    /// 128 plus the signal that did it: a Ctrl-C'd program shows 130, the
    /// same number it would anywhere else.
    fn set_status(&mut self, code: isize) {
        self.code = code;
        let shown = if code < 0 { 128 - code } else { code };
        let mut value = shown.max(0) as usize;
        let mut digits = [0u8; 12];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let len = digits.len() - at;
        self.status[..len].copy_from_slice(&digits[at..]);
        self.status_len = len;
    }

    /// What `$name` stands for. `?` is the last status, and a name nobody
    /// set expands to nothing - which is what a shell does with one.
    fn lookup(&self, name: &[u8]) -> Option<&[u8]> {
        if name == b"?" {
            return Some(&self.status[..self.status_len]);
        }
        for (known, value) in &self.vars[..self.var_len] {
            let used = known.iter().position(|&c| c == 0).unwrap_or(NAME_MAX);
            if &known[..used] == name {
                let used = value.iter().position(|&c| c == 0).unwrap_or(VALUE_MAX);
                return Some(&value[..used]);
            }
        }
        None
    }

    /// Remember a job the shell started and did not wait for.
    fn add_job(&mut self, pid: isize, name: &[u8]) {
        if self.job_len == JOB_MAX {
            let _ = lin::write_all(2, b"sh: too many background jobs\n");
            return;
        }
        let mut stored = [0u8; 24];
        let take = name.len().min(24);
        stored[..take].copy_from_slice(&name[..take]);
        self.jobs[self.job_len] = (pid, stored);
        self.job_len += 1;
    }

    /// Reap every background job that has ended, saying which.
    fn reap_jobs(&mut self) {
        let mut at = 0usize;
        while at < self.job_len {
            let (pid, name) = self.jobs[at];
            let mut status: i32 = 0;
            let rc = lin::wait4(pid as usize, &mut status as *mut i32, lin::WNOHANG);
            if rc == ERRNO_EAGAIN {
                at += 1;
                continue;
            }
            let _ = lin::write_all(1, b"[");
            let _ = lin::write_all(1, &name[..name.iter().position(|&c| c == 0).unwrap_or(24)]);
            let _ = lin::write_all(1, b"] done\n");
            self.jobs[at..self.job_len].rotate_left(1);
            self.job_len -= 1;
        }
    }

    /// Where the shell is, updated after a successful `cd`. Joined by hand:
    /// the kernel keeps the directory itself, and this is only the name of it.
    fn note_chdir(&mut self, target: &[u8]) {
        if target.starts_with(b"/") {
            let take = target.len().min(128);
            self.here = [0; 128];
            self.here[..take].copy_from_slice(&target[..take]);
            self.here_len = take;
            return;
        }
        let mut at = self.here_len;
        if at > 1 {
            self.here[at] = b'/';
            at += 1;
        }
        let room = (128 - at).min(target.len());
        self.here[at..at + room].copy_from_slice(&target[..room]);
        self.here_len = at + room;
    }

    /// The name of the place the shell is in, for `pwd`.
    fn cwd(&self) -> &[u8] {
        &self.here[..self.here_len]
    }

    /// The last command's exit status, as a number.
    fn last_status(&self) -> isize {
        self.code
    }

    fn set(&mut self, name: &[u8], value: &[u8]) {
        if name.len() > NAME_MAX || value.len() > VALUE_MAX {
            let _ = lin::write_all(2, b"sh: variable too long\n");
            return;
        }
        // An existing name is overwritten where it stands.
        for slot in &mut self.vars[..self.var_len] {
            let used = slot.0.iter().position(|&c| c == 0).unwrap_or(NAME_MAX);
            if &slot.0[..used] == name {
                slot.1 = [0; VALUE_MAX];
                slot.1[..value.len()].copy_from_slice(value);
                return;
            }
        }
        if self.var_len == VAR_MAX {
            let _ = lin::write_all(2, b"sh: too many variables\n");
            return;
        }
        let mut stored = ([0; NAME_MAX], [0; VALUE_MAX]);
        stored.0[..name.len()].copy_from_slice(name);
        stored.1[..value.len()].copy_from_slice(value);
        self.vars[self.var_len] = stored;
        self.var_len += 1;
    }
}

/// Build the argv layout `execve(2)` takes: argv[0] is the program name,
/// then the words, then a NULL. Each slot is a `*const u8` to a NUL-
/// terminated string, kept in a stack buffer the shell owns.
fn make_argv(name: &[u8], words: &[&[u8]], argv: &mut [*const u8; 16], strs: &mut [[u8; 32]; 16]) -> usize {
    let mut count = 0usize;
    let mut copy_in = |slot: &mut [u8; 32], src: &[u8]| {
        let n = src.len().min(31);
        slot[..n].copy_from_slice(&src[..n]);
        slot[n] = 0;
    };
    copy_in(&mut strs[count], name);
    argv[count] = strs[count].as_ptr();
    count += 1;
    for w in words {
        if count >= argv.len() - 1 {
            break;
        }
        copy_in(&mut strs[count], w);
        argv[count] = strs[count].as_ptr();
        count += 1;
    }
    argv[count] = core::ptr::null();
    count
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let mut sh = Shell::new();

    // `sh script` runs a file's lines instead of the console's.
    if argc > 1 {
        let args = lin::Args { argc, argv };
        run_script(&mut sh, args.get(1));
        lin::exit_group(0);
    }

    let mut line = [0u8; LINE_MAX];
    loop {
        sh.reap_jobs();
        if lin::write_all(1, b"$ ") < 0 {
            lin::exit_group(1);
        }
        let n = lin::read(0, &mut line);
        if n <= 0 {
            break;
        }
        run_line(&mut sh, &line[..n as usize]);
    }

    lin::exit_group(0);
}

/// Read a script and run every line of it, then come back.
fn run_script(sh: &mut Shell, path: &[u8]) {
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY, 0);
    if fd < 0 {
        let _ = lin::write_all(2, b"sh: cannot open ");
        let _ = lin::write_all(2, path);
        let _ = lin::write_all(2, b"\n");
        return;
    }
    let mut contents = [0u8; 4096];
    let mut used = 0usize;
    while used < contents.len() {
        let n = lin::read(fd as i32, &mut contents[used..]);
        if n <= 0 {
            break;
        }
        used += n as usize;
    }
    lin::close(fd as i32);

    let mut start = 0usize;
    for at in 0..=used {
        let ends = at == used || contents[at] == b'\n';
        if !ends {
            continue;
        }
        if at > start {
            run_line(sh, &contents[start..at]);
        }
        start = at + 1;
    }
}

/// The value `$name` has while this line is being parsed.
struct View {
    status: [u8; 12],
    status_len: usize,
    vars: [([u8; NAME_MAX], [u8; VALUE_MAX]); VAR_MAX],
    var_len: usize,
}

impl View {
    fn of(sh: &Shell) -> View {
        let mut view = View {
            status: [0; 12],
            status_len: sh.status_len,
            vars: sh.vars,
            var_len: sh.var_len,
        };
        view.status[..sh.status_len].copy_from_slice(&sh.status[..sh.status_len]);
        view
    }

    fn lookup(&self, name: &[u8]) -> Option<&[u8]> {
        if name == b"?" {
            return Some(&self.status[..self.status_len]);
        }
        for (known, value) in &self.vars[..self.var_len] {
            let used = known.iter().position(|&c| c == 0).unwrap_or(NAME_MAX);
            if &known[..used] == name {
                let used = value.iter().position(|&c| c == 0).unwrap_or(VALUE_MAX);
                return Some(&value[..used]);
            }
        }
        None
    }
}

/// Run one line: parse it, then run what it names.
fn run_line(sh: &mut Shell, line: &[u8]) {
    let mut budget = [0u8; BUDGET_MAX];
    let view = View::of(sh);
    let parsed = match shell::parse(line, &mut budget, &|name| view.lookup(name)) {
        Ok(parsed) => parsed,
        Err(e) => {
            report_parse_error(&e);
            return;
        }
    };

    if let Some((name, value)) = parsed.assignment {
        sh.set(name, value);
        return;
    }

    let commands = parsed.commands();
    if commands.is_empty() {
        return;
    }

    // Builtins run in the shell itself.
    if commands.len() == 1 && commands[0].redirect_out.is_none() {
        if let Some(code) = builtin(sh, commands[0].argv()) {
            sh.set_status(code);
            return;
        }
    }

    // No fork on mpos yet. Run the first command in-place via execve and
    // report the error; the shell can at least answer the prompt again.
    let first = &commands[0];
    let mut path = [0u8; 80];
    let name = first.name();
    path[..5].copy_from_slice(b"/bin/");
    path[5..5 + name.len()].copy_from_slice(name);

    // If a redirection is wanted, build it on the fd table first.
    if let Some(redir_path) = first.redirect_out {
        let fd = redirect_to(redir_path);
        if fd < 0 {
            let _ = lin::write_all(2, b"sh: cannot write ");
            let _ = lin::write_all(2, redir_path);
            let _ = lin::write_all(2, b"\n");
            sh.set_status(1);
            return;
        }
        let _ = lin::dup2(fd as i32, 1);
        let _ = lin::close(fd as i32);
    }

    let argv_words = first.argv();
    let mut argv_buf: [*const u8; 16] = [core::ptr::null(); 16];
    let mut strs: [[u8; 32]; 16] = [[0; 32]; 16];
    let _ = make_argv(name, &argv_words[1..], &mut argv_buf, &mut strs);
    let envp: [*const u8; 1] = [core::ptr::null()];

    // On mpos `execve` cannot swap this task's image, so it starts the
    // program as a child and answers its pid. Waiting for that child is
    // what makes the shell survive the command and read the next line -
    // and what makes `$?` the command's real exit status.
    let pid = lin::execve(path.as_ptr(), argv_buf.as_ptr(), envp.as_ptr());
    if pid < 0 {
        let _ = lin::write_all(2, b"sh: ");
        let _ = lin::write_all(2, name);
        let _ = lin::write_all(2, b": cannot run\n");
        sh.set_status(1);
        return;
    }
    let code = lin::wait4(pid as usize, core::ptr::null_mut(), 0);
    sh.set_status(if code < 0 { 1 } else { code });
}

/// The shell's own commands.
fn builtin(sh: &mut Shell, argv: &[&[u8]]) -> Option<isize> {
    match argv[0] {
        b"cd" => {
            let target = argv.get(1).copied().unwrap_or(b"/");
            let mut buf = [0u8; 128];
            let n = target.len().min(buf.len() - 1);
            buf[..n].copy_from_slice(&target[..n]);
            buf[n] = 0;
            if lin::chdir(&buf) < 0 {
                let _ = lin::write_all(2, b"sh: cd: cannot enter ");
                let _ = lin::write_all(2, target);
                let _ = lin::write_all(2, b"\n");
                Some(1)
            } else {
                sh.note_chdir(target);
                Some(0)
            }
        }
        b"pwd" => {
            let _ = lin::write_all(1, sh.cwd());
            let _ = lin::write_all(1, b"\n");
            Some(0)
        }
        b"exit" => {
            let code = argv.get(1).and_then(|v| decimal(v)).unwrap_or(sh.last_status());
            lin::exit_group(code as i32);
        }
        _ => None,
    }
}

fn decimal(bytes: &[u8]) -> Option<isize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0isize;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (b - b'0') as isize;
    }
    Some(value)
}

/// Open `path` for a `>` redirection: gone if it was there, made if it was
/// not, and a fresh descriptor either way.
fn redirect_to(path: &[u8]) -> isize {
    let mut buf = [0u8; 128];
    let n = path.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&path[..n]);
    buf[n] = 0;
    let _ = lin::unlinkat(lin::AT_FDCWD, &buf, 0);
    lin::openat(
        lin::AT_FDCWD,
        &buf,
        lin::O_WRONLY | lin::O_CREAT | lin::O_EXCL,
        0o666,
    )
}

fn report_parse_error(e: &shell::Error) {
    let text: &[u8] = match e {
        shell::Error::TooManyWords => b"sh: line too long\n",
        shell::Error::TooManyCommands => b"sh: too many commands in one pipeline\n",
        shell::Error::UnterminatedQuote => b"sh: missing closing quote\n",
        shell::Error::MissingRedirect => b"sh: > needs somewhere to write\n",
        shell::Error::MissingCommand => b"sh: | needs a command on both sides\n",
    };
    let _ = lin::write_all(2, text);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"sh: panicked\n")
}