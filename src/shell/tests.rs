//! The shell grammar, exercised on the host.
//!
//! This is where the shell's real regression coverage lives: a user program
//! has no test harness, so everything worth being sure about is over here.

use alloc::vec::Vec;

use super::{Error, MAX_PIPELINE, MAX_WORDS, parse};

/// Parse with no variables set.
fn parse_with<'a>(line: &'a str, budget: &'a mut [u8]) -> super::Line<'a> {
    parse(line.as_bytes(), budget, &|_| None).expect("the line parses")
}

/// A command's words as owned bytes, so an assertion can name what it wants
/// without the two sides having to be the same shape.
fn words(command: &super::Command<'_>) -> Vec<Vec<u8>> {
    command.argv().iter().map(|w| w.to_vec()).collect()
}

fn owned(words: &[&[u8]]) -> Vec<Vec<u8>> {
    words.iter().map(|w| w.to_vec()).collect()
}

#[test]
fn a_line_splits_into_words_on_any_run_of_space() {
    let mut budget = [0u8; 256];
    let line = parse_with("  echo \t hello  world \n", &mut budget);
    assert_eq!(line.len, 1);
    assert_eq!(
        words(&line.commands[0]),
        owned(&[b"echo", b"hello", b"world"])
    );
    assert_eq!(line.commands[0].name(), b"echo");
    assert!(!line.background);
}

#[test]
fn an_empty_line_has_nothing_to_run() {
    let mut budget = [0u8; 64];
    assert_eq!(parse_with("", &mut budget).len, 0);
    assert_eq!(parse_with("   \n", &mut budget).len, 0);
    assert_eq!(parse_with("\t", &mut budget).len, 0);
}

#[test]
fn a_pipe_splits_the_line_into_a_pipeline() {
    let mut budget = [0u8; 256];
    let line = parse_with("echo hi | cat | cat\n", &mut budget);
    assert_eq!(line.len, 3);
    assert_eq!(words(&line.commands[0]), owned(&[b"echo", b"hi"]));
    assert_eq!(words(&line.commands[1]), owned(&[b"cat"]));
    assert_eq!(words(&line.commands[2]), owned(&[b"cat"]));

    // Spacing around the bar is the typist's business, not the grammar's.
    let line = parse_with("a|b", &mut budget);
    assert_eq!(line.len, 2);
    assert_eq!(line.commands[0].name(), b"a");
    assert_eq!(line.commands[1].name(), b"b");
}

#[test]
fn redirection_takes_the_path_out_of_the_words() {
    let mut budget = [0u8; 256];
    let line = parse_with("echo hi > /tmp/out\n", &mut budget);
    assert_eq!(line.len, 1);
    assert_eq!(words(&line.commands[0]), owned(&[b"echo", b"hi"]));
    assert_eq!(line.commands[0].redirect_out, Some(&b"/tmp/out"[..]));

    // Where it lands in the line does not matter, and it can lead.
    let line = parse_with("> /f cat", &mut budget);
    assert_eq!(words(&line.commands[0]), owned(&[b"cat"]));
    assert_eq!(line.commands[0].redirect_out, Some(&b"/f"[..]));

    // A `>` with nowhere to write is an error rather than a silent swallow.
    assert_eq!(
        parse(b"echo hi >", &mut budget, &|_| None).unwrap_err(),
        Error::MissingRedirect
    );
    assert_eq!(
        parse(b"echo hi > | cat", &mut budget, &|_| None).unwrap_err(),
        Error::MissingRedirect
    );
}

#[test]
fn only_a_trailing_ampersand_asks_for_the_background() {
    let mut budget = [0u8; 256];
    let line = parse_with("sleep 5 &\n", &mut budget);
    assert!(line.background);
    assert_eq!(words(&line.commands[0]), owned(&[b"sleep", b"5"]));

    // In the middle it is just a character in a word.
    let line = parse_with("echo a&b\n", &mut budget);
    assert!(!line.background);
    assert_eq!(words(&line.commands[0]), owned(&[b"echo", b"a&b"]));

    // With nothing in front of it there is no job to run.
    let line = parse_with("&\n", &mut budget);
    assert!(!line.background);
    assert_eq!(line.len, 0);
}

#[test]
fn single_quotes_take_the_special_meaning_off_everything_between_them() {
    let mut budget = [0u8; 256];
    let line = parse_with("echo 'a | b > c' tail\n", &mut budget);
    assert_eq!(line.len, 1, "the bar inside quotes is not a pipe");
    assert_eq!(
        words(&line.commands[0]),
        owned(&[b"echo", b"a | b > c", b"tail"])
    );

    // The quotes come off, and an empty one is an empty word.
    let line = parse_with("echo ''", &mut budget);
    assert_eq!(words(&line.commands[0]), owned(&[b"echo", b""]));

    // One that never closes is an error, not a word that runs to the end.
    assert_eq!(
        parse(b"echo 'half", &mut budget, &|_| None).unwrap_err(),
        Error::UnterminatedQuote
    );
}

#[test]
fn a_dollar_expands_from_the_lookup_and_an_unknown_one_vanishes() {
    let lookup = |name: &[u8]| match name {
        b"WHO" => Some(&b"world"[..]),
        b"?" => Some(&b"0"[..]),
        _ => None,
    };
    // Each parse borrows its budget until the `Line` it returned is done, so
    // one assertion gets one buffer.
    fn check(line: &[u8], want: &[&[u8]], lookup: super::Lookup<'_>) {
        let mut budget = [0u8; 256];
        let parsed = parse(line, &mut budget, lookup).unwrap();
        assert_eq!(words(&parsed.commands[0]), owned(want));
    }

    check(b"echo hello $WHO", &[b"echo", b"hello", b"world"], &lookup);
    // A word that is nothing but a variable, and one with text before it.
    check(b"echo $WHO!", &[b"echo", b"world!"], &lookup);
    // Undefined names expand to nothing at all, which is what a shell does.
    check(b"echo [$NOPE]", &[b"echo", b"[]"], &lookup);
    // `$?` is answered by the caller like any other name.
    check(b"echo $?", &[b"echo", b"0"], &lookup);
    // A dollar with no name after it is a dollar sign: eating what follows
    // would lose a character the typist meant.
    check(b"echo $ b", &[b"echo", b"$", b"b"], &lookup);
}

#[test]
fn expansion_and_quoting_work_together() {
    let lookup = |name: &[u8]| (name == b"HOME").then_some(&b"/root"[..]);
    fn check(line: &[u8], want: &[&[u8]], lookup: super::Lookup<'_>) {
        let mut budget = [0u8; 256];
        let parsed = parse(line, &mut budget, lookup).unwrap();
        assert_eq!(words(&parsed.commands[0]), owned(want));
    }

    // An expansion before a quoted run, which is the case that forces the
    // word to be rebuilt rather than sliced out of the line.
    check(b"echo $HOME'/x y'", &[b"echo", b"/root/x y"], &lookup);
    // The reverse order too.
    check(b"echo '/a'$HOME", &[b"echo", b"/a/root"], &lookup);
}

#[test]
fn a_bare_assignment_is_recognised_and_runs_nothing() {
    let mut budget = [0u8; 256];
    let line = parse_with("NAME=value\n", &mut budget);
    assert_eq!(line.len, 0, "an assignment is not a command");
    assert_eq!(line.assignment, Some((&b"NAME"[..], &b"value"[..])));

    // An empty value is a value, and `=` on its own is not a name.
    let line = parse_with("EMPTY=\n", &mut budget);
    assert_eq!(line.assignment, Some((&b"EMPTY"[..], &b""[..])));
    assert_eq!(parse_with("=x\n", &mut budget).assignment, None);

    // With anything else on the line it is just a word - this shell has no
    // `VAR=x cmd` form yet, and guessing at one would run the wrong thing.
    let line = parse_with("NAME=value other\n", &mut budget);
    assert_eq!(line.len, 1);
    assert_eq!(line.assignment, None);
    assert_eq!(words(&line.commands[0]), owned(&[b"NAME=value", b"other"]));
}

#[test]
fn a_line_too_long_for_the_fixed_room_is_an_error_not_a_truncation() {
    // A variable worth more than the buffer it has to be built in. The line is
    // small; what does not fit is what the name expands to.
    let value = [b'a'; 200];
    let lookup = |name: &[u8]| (name == b"BIG").then_some(&value[..]);
    let mut budget = [0u8; 32];
    assert_eq!(
        parse(b"echo $BIG", &mut budget, &lookup).unwrap_err(),
        Error::TooManyWords
    );

    // The same word fits once the room is there, so it is the room that was
    // short and not the word that was refused.
    let mut roomy = [0u8; 256];
    let line = parse(b"echo $BIG", &mut roomy, &lookup).expect("it fits a bigger buffer");
    assert_eq!(line.commands[0].argv().len(), 2);
    assert_eq!(line.commands[0].argv()[1].len(), 200);
}

#[test]
fn too_many_words_or_commands_is_refused() {
    let mut budget = [0u8; 1024];
    let many_words = "a ".repeat(MAX_WORDS + 1);
    assert_eq!(
        parse(many_words.as_bytes(), &mut budget, &|_| None).unwrap_err(),
        Error::TooManyWords
    );

    let many_pipes = "a | ".repeat(MAX_PIPELINE) + "a";
    assert_eq!(
        parse(many_pipes.as_bytes(), &mut budget, &|_| None).unwrap_err(),
        Error::TooManyCommands
    );

    // A bar with nothing on one side names no command to run.
    assert_eq!(
        parse(b"| cat", &mut budget, &|_| None).unwrap_err(),
        Error::MissingCommand
    );
    assert_eq!(
        parse(b"echo a |", &mut budget, &|_| None).unwrap_err(),
        Error::MissingCommand
    );
}
