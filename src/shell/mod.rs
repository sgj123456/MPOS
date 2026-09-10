//! The shell's grammar, in one place and with no dependencies.
//!
//! A user program is compiled by itself with plain `rustc`, so it has no test
//! harness and no way to be unit-tested where it lives. The parsing a shell
//! does is the part most worth testing - it is where every quoting and
//! splitting bug lives - so it lives here instead, in the kernel's library,
//! and `user/bin/sh.rs` pulls it in the same way it pulls in the libc and the
//! syscall table:
//!
//!     #[path = "../../src/shell/mod.rs"]
//!     mod shell;
//!
//! The module is `no_std` and allocates nothing: every buffer a caller passes
//! is a fixed array, and every result borrows from the line it was parsed
//! from. That is the only shape a program without a heap can use, and it is
//! also what makes the whole thing testable with a `&str`.

/// The most words one simple command may hold, and the most commands one
/// pipeline may chain. Both are small on purpose: a shell without a heap
/// gives out fixed room, and a line that wants more than this is a line
/// nobody typed by hand.
pub const MAX_WORDS: usize = 16;
pub const MAX_PIPELINE: usize = 4;

/// One command of a pipeline: its words, and where its output goes.
#[derive(Clone, Copy, Debug)]
pub struct Command<'a> {
    /// The words, `argv[0]` first.
    pub words: [&'a [u8]; MAX_WORDS],
    pub len: usize,
    /// `> file`: the path its output is written to, if the line asked.
    pub redirect_out: Option<&'a [u8]>,
}

impl Command<'_> {
    /// The words this command actually has, as a slice a caller can hand to
    /// `spawn` - which wants the same vector without the empty tail.
    pub fn argv(&self) -> &[&[u8]] {
        &self.words[..self.len]
    }

    /// The command's name, or empty for a command with no words.
    pub fn name(&self) -> &[u8] {
        if self.len == 0 { &[] } else { self.words[0] }
    }

    /// What to print when this command cannot be run.
    pub fn empty() -> Command<'static> {
        Command {
            words: [&[]; MAX_WORDS],
            len: 0,
            redirect_out: None,
        }
    }
}

/// A whole line: the commands to run, whether to wait for them, and whether
/// the line was an assignment rather than a command.
#[derive(Clone, Copy, Debug)]
pub struct Line<'a> {
    pub commands: [Command<'a>; MAX_PIPELINE],
    /// How many of `commands` are real.
    pub len: usize,
    /// `name=value`, when the line was one. The shell stores these itself;
    /// nothing here knows what a variable is.
    pub assignment: Option<(&'a [u8], &'a [u8])>,
    /// A trailing `&`: run it and come straight back to the prompt.
    pub background: bool,
}

impl Line<'_> {
    /// The commands this line actually has.
    pub fn commands(&self) -> &[Command<'_>] {
        &self.commands[..self.len]
    }

    /// An empty line: nothing to run.
    pub fn empty() -> Line<'static> {
        Line {
            commands: [Command::empty(); MAX_PIPELINE],
            len: 0,
            assignment: None,
            background: false,
        }
    }
}

/// How a line failed to parse. The shell turns each into a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// More words in one command than [`MAX_WORDS`].
    TooManyWords,
    /// More commands in the pipeline than [`MAX_PIPELINE`].
    TooManyCommands,
    /// A bar with no command on one side of it.
    MissingCommand,
    /// A quote that never closed.
    UnterminatedQuote,
    /// `>` with nothing after it.
    MissingRedirect,
}

/// Is this byte one that separates words?
///
/// The newline is one because a canonical read hands back the line it
/// terminates: without it the last word of every command would carry the
/// Enter that ended it, and `/bin/true\n` is not a program.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// A variable lookup, as the expander sees it: given a name, the value, or
/// nothing when the name is not set.
pub type Lookup<'f> = &'f dyn Fn(&[u8]) -> Option<&'f [u8]>;

/// Split one line into the commands it names.
///
/// The grammar is deliberately the smallest one that is still a shell: words
/// separated by whitespace, `|` between commands, `>` before a path, `&` at
/// the very end, and single quotes to take the special meaning off everything
/// between them. There is no escaping, no double quotes, no `;` and no `&&` -
/// each of those is a design of its own and none is needed to run a program.
///
/// Everything returned borrows from `line`, quotes stripped; expansion has
/// already happened by the time this returns, because the caller has the
/// variables and this module does not. A word may therefore need to be
/// rebuilt to substitute - `budget` is where that room comes from.
/// Split one line into the commands it names.
///
/// The grammar is the smallest one that is still a shell: words separated by
/// whitespace, `|` between commands, `>` before a path, `&` at the very end,
/// and single quotes to take the special meaning off everything between them.
/// There is no escaping, no double quotes, no `;` and no `&&` - each is a
/// design of its own and none is needed to run a program.
///
/// Everything returned borrows from `line` or from `budget`; a word that
/// needed a substitution to build is written into the latter, because this
/// module cannot allocate. Both must outlive the `Line`.
/// Split one line into the commands it names.
///
/// The grammar is the smallest one that is still a shell: words separated by
/// whitespace, `|` between commands, `>` before a path, `&` at the very end,
/// and single quotes to take the special meaning off everything between them.
/// There is no escaping, no double quotes, no `;` and no `&&` - each is a
/// design of its own and none is needed to run a program.
///
/// Everything returned borrows from `line` or from `budget`. A word that
/// needed a substitution is built in `budget` during a first pass, and the
/// slices are taken in a second one, after the writing is over: a shell
/// without a heap has to be explicit about how long a built word lives, and
/// this is what makes it expressible - the result borrows both inputs for as
/// long as it exists, and neither may be touched while it does.
pub fn parse<'a>(
    line: &'a [u8],
    budget: &'a mut [u8],
    lookup: Lookup<'_>,
) -> Result<Line<'a>, Error> {
    // Pass one records where every word is and builds the ones that need it:
    // a lifetime is not available until the writing stops, so a span is what
    // is remembered rather than a slice.
    let plan = plan(line, budget, lookup)?;
    // Pass two turns the spans into slices, by which point `budget` is as
    // final as it is ever going to be.
    Ok(plan.resolve(line, budget))
}

/// One word's whereabouts: either a stretch of the line as typed, or a
/// stretch of the budget it was built into.
#[derive(Clone, Copy)]
enum Span {
    Plain(usize, usize),
    Built(usize, usize),
}

impl Span {
    fn slice<'a>(&self, line: &'a [u8], budget: &'a [u8]) -> &'a [u8] {
        match *self {
            Span::Plain(start, end) => &line[start..end],
            Span::Built(start, end) => &budget[start..end],
        }
    }
}

/// A parsed but unsliced line, and how much of the budget it used.
struct Plan {
    words: [[Span; MAX_WORDS]; MAX_PIPELINE],
    lens: [usize; MAX_PIPELINE],
    redirects: [Option<Span>; MAX_PIPELINE],
    len: usize,
    assignment: Option<(Span, Span)>,
    background: bool,
}

impl Plan {
    fn resolve<'a>(&self, line: &'a [u8], budget: &'a [u8]) -> Line<'a> {
        let mut out = Line::empty();
        out.len = self.len;
        out.background = self.background;
        if let Some((name, value)) = self.assignment {
            out.assignment = Some((name.slice(line, budget), value.slice(line, budget)));
        }
        for at in 0..self.len {
            let mut command = Command::empty();
            command.len = self.lens[at];
            for word in 0..command.len {
                command.words[word] = self.words[at][word].slice(line, budget);
            }
            command.redirect_out = self.redirects[at].map(|span| span.slice(line, budget));
            out.commands[at] = command;
        }
        out
    }
}

/// Pass one. The borrow of `budget` here is short: a span is only offsets, so
/// nothing this returns points into the buffer - which is what lets `parse`
/// read it back out afterwards.
fn plan(line: &[u8], budget: &mut [u8], lookup: Lookup<'_>) -> Result<Plan, Error> {
    let mut out = Plan {
        words: [[Span::Plain(0, 0); MAX_WORDS]; MAX_PIPELINE],
        lens: [0; MAX_PIPELINE],
        redirects: [None; MAX_PIPELINE],
        len: 0,
        assignment: None,
        background: false,
    };
    let mut words_in_command = 0usize;
    // Which command the words being read belong to. `out.len` only counts the
    // commands already finished, so it is not this: a command's words are
    // gathered before it joins that count.
    let mut building = 0usize;
    let mut used = 0usize;
    let mut at = 0usize;
    let mut background = false;

    while at < line.len() {
        let b = line[at];
        // `&` means the background only when it ends the line: in the middle
        // it is just a character, and `a&b` is one word.
        if b == b'&' && line[at + 1..].iter().all(|&c| is_space(c)) {
            background = true;
            break;
        }
        if is_space(b) {
            at += 1;
            continue;
        }
        if b == b'|' {
            if words_in_command == 0 {
                // `a || b`, or a bar with nothing before it: there is no
                // command to end, and an empty one would run nothing.
                return Err(Error::MissingCommand);
            }
            finish_command(&mut out, words_in_command)?;
            if out.len == MAX_PIPELINE {
                return Err(Error::TooManyCommands);
            }
            building = out.len;
            words_in_command = 0;
            at += 1;
            continue;
        }
        let redirect = b == b'>';
        if redirect {
            at += 1;
            while at < line.len() && is_space(line[at]) {
                at += 1;
            }
        }

        let (span, next) = word(line, at, budget, &mut used, lookup)?;
        at = next;
        if redirect {
            if span.slice(line, budget).is_empty() {
                return Err(Error::MissingRedirect);
            }
            out.redirects[building] = Some(span);
            continue;
        }
        if words_in_command == MAX_WORDS {
            return Err(Error::TooManyWords);
        }
        out.words[building][words_in_command] = span;
        words_in_command += 1;
    }
    if words_in_command > 0 {
        finish_command(&mut out, words_in_command)?;
    } else if out.len > 0 && building == out.len {
        // A trailing bar: `a |`. The command before it is real and the one
        // after it is not.
        return Err(Error::MissingCommand);
    }

    // `name=value` is an assignment only when the whole line is one word:
    // `a=b c` runs `c` with `a` set, and `ls a=b` runs `ls`. Neither is this
    // shell's business yet, so only the bare form is taken.
    if out.len == 1 && out.lens[0] == 1 && out.redirects[0].is_none() {
        let only = out.words[0][0].slice(line, budget);
        if let Some(eq) = only.iter().position(|&c| c == b'=') {
            let name = &only[..eq];
            if !name.is_empty() && name.iter().all(|&c| is_name_char(c)) {
                let full = out.words[0][0];
                out.assignment = Some((sub(full, 0, eq), sub(full, eq + 1, only.len())));
                out.len = 0;
            }
        }
    }
    // A background line with no command is nothing to do.
    out.background = background && out.len > 0;
    Ok(out)
}

/// A piece of a word: the same place it was, narrowed to `from..to`.
///
/// Only ever used on a plain span, where the offsets are the line's own; a
/// built word is already a fresh buffer and a shell this small has no use for
/// taking one apart.
fn sub(span: Span, from: usize, to: usize) -> Span {
    match span {
        Span::Plain(start, _) => Span::Plain(start + from, start + to),
        Span::Built(start, _) => Span::Built(start + from, start + to),
    }
}

fn finish_command(out: &mut Plan, words: usize) -> Result<(), Error> {
    assert!(
        out.len < MAX_PIPELINE,
        "the pipeline bound is checked first"
    );
    out.lens[out.len] = words;
    out.len += 1;
    Ok(())
}

/// Where a word's words end and the next one begins, quotes included.
///
/// A quoted run is part of the word whatever it holds, so `'a b'` is one
/// word and the space inside it does not separate. Everything else stops at
/// whitespace, at a bar, or at a `>` that starts a redirection.
fn word_end(line: &[u8], at: usize) -> usize {
    let mut end = at;
    while end < line.len() {
        let b = line[end];
        if b == b'\'' {
            // Run to the closing quote; an unclosed one runs to the end and
            // is caught when the word is built, which is where the error is
            // reported.
            match line[end + 1..].iter().position(|&c| c == b'\'') {
                Some(inner) => {
                    end += inner + 2;
                    continue;
                }
                None => return line.len(),
            }
        }
        if is_space(b) || b == b'|' || b == b'>' {
            break;
        }
        // `&` separates only when it ends the line: inside a word it is a
        // character, so `a&b` is one word.
        if b == b'&' && line[end + 1..].iter().all(|&c| is_space(c)) {
            break;
        }
        end += 1;
    }
    end
}

/// Whether `c` may appear in a variable name.
fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// One word starting at `at`, quotes honoured and `$` expanded, plus where
/// the word after it begins.
///
/// A word with nothing to substitute is remembered as a stretch of `line` and
/// nothing is copied - which is most words, and the reason the span exists.
/// One that has work done to it is built into `budget` from `budget_at`, and
/// the returned span points there. Nothing is sliced here: a slice would need
/// a lifetime that outlives the writing, and there is still writing to come.
fn word(
    line: &[u8],
    at: usize,
    budget: &mut [u8],
    budget_at: &mut usize,
    lookup: Lookup<'_>,
) -> Result<(Span, usize), Error> {
    let plain_end = word_end(line, at);
    let plain = &line[at..plain_end];
    if !plain.iter().any(|&c| c == b'\'' || c == b'$') {
        return Ok((Span::Plain(at, plain_end), plain_end));
    }

    let base = *budget_at;
    let mut out = base;
    let mut cursor = at;
    while cursor < plain_end {
        let b = line[cursor];
        if b == b'\'' {
            // Everything between the quotes is literal, and they come off.
            let Some(end) = line[cursor + 1..].iter().position(|&c| c == b'\'') else {
                return Err(Error::UnterminatedQuote);
            };
            copy(&line[cursor + 1..cursor + 1 + end], budget, &mut out)?;
            cursor += end + 2;
            continue;
        }
        if b == b'$' {
            let (value, next) = expanded(line, cursor, lookup);
            copy(value, budget, &mut out)?;
            cursor = next;
            continue;
        }
        // A run with neither quote nor dollar in it, taken whole.
        let stop = line[cursor..plain_end]
            .iter()
            .position(|&c| c == b'\'' || c == b'$')
            .map(|i| cursor + i)
            .unwrap_or(plain_end);
        copy(&line[cursor..stop], budget, &mut out)?;
        cursor = stop;
    }
    *budget_at = out;
    Ok((Span::Built(base, out), plain_end))
}

/// The value of the `$` at `at`, and where the text after it resumes.
///
/// `$?` is the last command's status, which the caller answers through the
/// lookup like any other name - it is the shell that decides `?` is a
/// variable, and this module that does not care.
fn expanded<'v>(line: &[u8], at: usize, lookup: Lookup<'v>) -> (&'v [u8], usize) {
    match line.get(at + 1) {
        Some(b'$') => (b"$", at + 2),
        Some(b'?') => (lookup(b"?").unwrap_or(b""), at + 2),
        Some(&c) if is_name_char(c) => {
            let mut end = at + 1;
            while end < line.len() && is_name_char(line[end]) {
                end += 1;
            }
            (lookup(&line[at + 1..end]).unwrap_or(b""), end)
        }
        // A `$` before anything else is a dollar sign: there is no name to
        // look up, and eating the next character would be worse.
        _ => (b"$", at + 1),
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests;

/// Append `bytes` to the word being rebuilt in `budget`.
fn copy(bytes: &[u8], budget: &mut [u8], at: &mut usize) -> Result<(), Error> {
    if *at + bytes.len() > budget.len() {
        // Out of room is the same failure as too many words: the line asked
        // for more than a shell this size hands out.
        return Err(Error::TooManyWords);
    }
    budget[*at..*at + bytes.len()].copy_from_slice(bytes);
    *at += bytes.len();
    Ok(())
}
