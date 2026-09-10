//! The shell: a user program that reads lines from fd 0 and runs them.
//!
//! The grammar lives in the kernel's library (`src/shell`) so it can be tested
//! where the tests are; what is left here is the executor - turning a parsed
//! line into spawned processes and waited-for exits. One word or many, the
//! first names a program under /bin; the words after it become its argv.
//! `a | b` runs the two through a pipe, `> path` sends a command's output to a
//! file, and `name=value` on its own remembers a variable for `$name` later.
//!
//! The quiet contract holds: the shell prints nothing unless something went
//! wrong, or unless someone asks what happened (`$?`).

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;
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
            jobs: [(0, [0; 24]); JOB_MAX],
            job_len: 0,
            status: [0; 12],
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
        // Left-aligned: the reader takes the first status_len bytes.
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
            let _ = sys::write_all(2, b"sh: too many background jobs\n");
            return;
        }
        let mut stored = [0u8; 24];
        let take = name.len().min(24);
        stored[..take].copy_from_slice(&name[..take]);
        self.jobs[self.job_len] = (pid, stored);
        self.job_len += 1;
    }

    /// Reap every background job that has ended, saying which.
    ///
    /// Asked with WNOHANG, one at a time: a job still running answers
    /// EAGAIN and stays. This runs before each prompt, which is when a
    /// shell is allowed to write to the console unasked.
    fn reap_jobs(&mut self) {
        let mut at = 0usize;
        while at < self.job_len {
            let (pid, name) = self.jobs[at];
            let code = sys::wait_flags(pid as usize, 1); // WNOHANG
            if code == ERRNO_EAGAIN {
                at += 1;
                continue;
            }
            let _ = sys::write_all(1, b"[");
            let _ = sys::write_all(1, &name[..name.iter().position(|&c| c == 0).unwrap_or(24)]);
            let _ = sys::write_all(1, b"] done\n");
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
            let _ = sys::write_all(2, b"sh: variable too long\n");
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
            let _ = sys::write_all(2, b"sh: too many variables\n");
            return;
        }
        let mut stored = ([0; NAME_MAX], [0; VALUE_MAX]);
        stored.0[..name.len()].copy_from_slice(name);
        stored.1[..value.len()].copy_from_slice(value);
        self.vars[self.var_len] = stored;
        self.var_len += 1;
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let mut sh = Shell::new();

    // `sh script` runs a file's lines instead of the console's.
    if argc > 1 {
        let args = sys::Args { argc, argv };
        run_script(&mut sh, args.get(1));
        sys::exit(0);
    }

    let mut line = [0u8; LINE_MAX];
    loop {
        sh.reap_jobs();
        if sys::write_all(1, b"$ ") < 0 {
            sys::exit(1);
        }
        // A canonical read: one line, newline included, 0 at end of input.
        let n = sys::read(0, &mut line);
        if n <= 0 {
            break;
        }
        run_line(&mut sh, &line[..n as usize]);
    }

    sys::exit(0);
}

/// Read a script and run every line of it, then come back.
fn run_script(sh: &mut Shell, path: &[u8]) {
    let fd = sys::open(path);
    if fd < 0 {
        let _ = sys::write_all(2, b"sh: cannot open ");
        let _ = sys::write_all(2, path);
        let _ = sys::write_all(2, b"\n");
        return;
    }
    let mut contents = [0u8; 4096];
    let mut used = 0usize;
    while used < contents.len() {
        let n = sys::read(fd as i32, &mut contents[used..]);
        if n <= 0 {
            break;
        }
        used += n as usize;
    }
    sys::close(fd as i32);

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
///
/// Copied out of the shell first, so the parse holds no borrow on it: the
/// status and the variables are read-only for the length of a parse, and the
/// bytes the lookup hands back are gone the moment the expansion is written
/// into the budget anyway.
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

    // A bare `name=value` remembers, and runs nothing.
    if let Some((name, value)) = parsed.assignment {
        sh.set(name, value);
        return;
    }

    let commands = parsed.commands();
    if commands.is_empty() {
        return;
    }

    // Builtins run in the shell itself, and only ever alone: `cd | cat` has
    // no meaning here, because a builtin has no separate process to pipe
    // from. Everything the builtin touches is the shell's own state.
    if commands.len() == 1 && commands[0].redirect_out.is_none() {
        if let Some(code) = builtin(sh, commands[0].argv()) {
            sh.set_status(code);
            return;
        }
    }

    // A trailing `&`: start the pipeline, remember it, and give the prompt
    // straight back. The job is reaped before the next prompt.
    let background = parsed.background;
    let first_name = commands[0].name();

    // The pipes between the stages: one fewer than the commands. The shell
    // holds both ends only long enough to hand them over, because a pipeline
    // ends only when the programs are the last ones holding it.
    let stages = commands.len();
    let mut pipes = [[0i32; 2]; shell::MAX_PIPELINE - 1];
    for pair in pipes.iter_mut().take(stages - 1) {
        if sys::pipe(pair) < 0 {
            let _ = sys::write_all(2, b"sh: pipe failed\n");
            return;
        }
    }

    // One file descriptor the shell opens for a `>`, if this line has one. It
    // is closed again the moment the child has its copy, success or not.
    let mut shell_fd = -1isize;
    let mut pids = [0isize; shell::MAX_PIPELINE];
    let mut spawned = 0usize;
    for (stage, command) in commands.iter().enumerate() {
        let mut redirects: [(i32, i32); 3] = [(0, 0); 3];
        let mut count = 0usize;
        if stage > 0 {
            redirects[count] = (pipes[stage - 1][0], 0);
            count += 1;
        }
        if stage + 1 < stages {
            redirects[count] = (pipes[stage][1], 1);
            count += 1;
        }
        if let Some(path) = command.redirect_out {
            match redirect_to(path) {
                Ok(fd) => {
                    redirects[count] = (fd as i32, 1);
                    count += 1;
                    shell_fd = fd;
                }
                Err(e) => {
                    let _ = sys::write_all(2, b"sh: cannot write ");
                    let _ = sys::write_all(2, path);
                    let _ = sys::write_all(2, b"\n");
                    close_pipes(&mut pipes, stages);
                    sh.set_status(-e);
                    return;
                }
            }
        }

        // Where the program lives: every one is in /bin, which is the whole
        // of the PATH for now. Only the filled part of the buffer is the
        // path - the tail is zeros, and a name with NULs in it is a name
        // nothing answers to.
        let mut path = [0u8; 80];
        let name = command.name();
        path[..5].copy_from_slice(b"/bin/");
        path[5..5 + name.len()].copy_from_slice(name);
        let path = &path[..5 + name.len()];
        let pid = sys::spawn_with(path, command.argv(), &redirects[..count]);
        if shell_fd >= 0 {
            // The child has its copy of the file now, or never will; either
            // way the shell's own end has done its job.
            sys::close(shell_fd as i32);
            shell_fd = -1;
        }
        // The shell's pipe ends go too: it is neither a writer nor a reader.
        if stage > 0 {
            sys::close(pipes[stage - 1][0]);
        }
        if stage + 1 < stages {
            sys::close(pipes[stage][1]);
        }
        if pid < 0 {
            let _ = sys::write_all(2, b"sh: ");
            let _ = sys::write_all(2, command.name());
            let _ = sys::write_all(2, b": cannot run\n");
            close_pipes(&mut pipes, stages);
            sh.set_status(-pid);
            return;
        }
        pids[stage] = pid;
        spawned = stage + 1;
    }

    // The last stage's status is the pipeline's, the way POSIX settles it.
    // A background line skips this entirely: the job belongs to the table
    // now, and collecting it is the reaper's business, later.
    if !background {
        let mut code = 0isize;
        for stage in 0..spawned {
            code = sys::wait(pids[stage]);
            if code == -10 {
                // ECHILD is the one answer that means the wait itself went
                // wrong; a negative code would be a death by signal, which
                // is a fact about the child and not a failure of the wait.
                let _ = sys::write_all(2, b"sh: wait failed\n");
                code = 1;
            }
            if code < 0 {
                // A death by signal is worth naming: 137 out of nowhere means
                // nothing, "killed by signal 9" means Ctrl-C or a kill worked.
                let _ = sys::write_all(2, b"sh: ");
                let _ = sys::write_all(2, commands[stage.min(commands.len() - 1)].name());
                let _ = sys::write_all(2, b": killed by signal ");
                let mut digits = [0u8; 8];
                let mut at = digits.len();
                let mut v = (-code) as usize;
                loop {
                    at -= 1;
                    digits[at] = b'0' + (v % 10) as u8;
                    v /= 10;
                    if v == 0 {
                        break;
                    }
                }
                let _ = sys::write_all(2, &digits[at..]);
                let _ = sys::write_all(2, b"\n");
            }
        }
        sh.set_status(code);
    }

    // Started, not waited for: it belongs to the job table until it is done.
    if background {
        sh.add_job(pids[spawned - 1], first_name);
    }
}

/// The shell's own commands. `Some` is an exit status and the line is over;
/// `None` means the name is not a builtin and must be spawned.
fn builtin(sh: &mut Shell, argv: &[&[u8]]) -> Option<isize> {
    match argv[0] {
        // `cd` moves the process's directory and the shell's own record of
        // where that is: `pwd` can only print what the shell remembers.
        b"cd" => {
            let target = argv.get(1).copied().unwrap_or(b"/");
            if sys::chdir(target) < 0 {
                let _ = sys::write_all(2, b"sh: cd: cannot enter ");
                let _ = sys::write_all(2, target);
                let _ = sys::write_all(2, b"\n");
                Some(1)
            } else {
                sh.note_chdir(target);
                Some(0)
            }
        }
        b"pwd" => {
            let _ = sys::write_all(1, sh.cwd());
            let _ = sys::write_all(1, b"\n");
            Some(0)
        }
        // The way a session ends. The status is what the shell exits with,
        // and what init sees before it starts the next shell.
        b"exit" => {
            let code = argv.get(1).and_then(|v| decimal(v)).unwrap_or(sh.last_status());
            sys::exit(code as i32);
        }
        _ => None,
    }
}

/// A decimal number in `bytes`, if the whole thing is one.
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
/// not, and a fresh descriptor either way. The unlink-then-create is what
/// truncation looks like without an open that takes flags.
fn redirect_to(path: &[u8]) -> Result<isize, isize> {
    let _ = sys::unlink(path);
    let fd = sys::create(path);
    if fd < 0 { Err(fd) } else { Ok(fd) }
}

/// Close every end of the pipeline's pipes, back to front.
///
/// On the way out after a failure: the stages that did start are blocked
/// reading or writing, and only these closes can tell them it is over.
fn close_pipes(pipes: &mut [[i32; 2]; shell::MAX_PIPELINE - 1], stages: usize) {
    for pair in pipes.iter_mut().take(stages.saturating_sub(1)) {
        sys::close(pair[0]);
        sys::close(pair[1]);
        pair[0] = -1;
        pair[1] = -1;
    }
}

fn report_parse_error(e: &shell::Error) {
    let text: &[u8] = match e {
        shell::Error::TooManyWords => b"sh: line too long\n",
        shell::Error::TooManyCommands => b"sh: too many commands in one pipeline\n",
        shell::Error::UnterminatedQuote => b"sh: missing closing quote\n",
        shell::Error::MissingRedirect => b"sh: > needs somewhere to write\n",
        shell::Error::MissingCommand => b"sh: | needs a command on both sides\n",
    };
    let _ = sys::write_all(2, text);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
