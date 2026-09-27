#![no_main]

//! How loud the kernel agrees to be, read off the command line.
//!
//! `level_from_cmdline` is the boot loader's text turned into a rank the log
//! subscriber compares against. The command line is firmware-controlled and,
//! under QEMU, attacker-controllable from the same `-append` that decides which
//! kernel runs - so the one property that matters is that it answers a rank in
//! the range the subscriber knows how to compare, whatever the bytes say.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::logging;

fuzz_target!(|data: &[u8]| {
    let cmdline = support::text(data, data.len().min(256));
    let level = logging::level_from_cmdline(&cmdline);
    // `rank` gives ERROR 1 through TRACE 5; the quietest and loudest answers
    // the subscriber can be handed are its ends, and nothing outside them has
    // a level to compare against.
    assert!(
        (1..=5).contains(&level),
        "{cmdline:?} selected log level {level}, which no `Level` ranks to"
    );

    // The parsing is whitespace-split, so the same words anywhere in the line
    // select the same level - and a line that mentions `loglevel=` twice takes
    // the first, which is what `strscpy`-based argument handling does too.
    // The decision is made per whitespace-separated word and nothing else, so
    // a command line re-spelled with different runs of whitespace between the
    // very same words has to answer identically. Anything else would mean the
    // level depends on how the bootloader laid the line out.
    let respelled = cmdline.split_whitespace().collect::<Vec<_>>().join("  ");
    assert_eq!(
        logging::level_from_cmdline(&respelled),
        level,
        "{cmdline:?} and its own words re-spaced answer differently"
    );
});
