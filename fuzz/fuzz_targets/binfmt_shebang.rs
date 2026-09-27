#![no_main]

//! `binfmt`'s reading of a `#!` line - the path and argument that decide what
//! runs a file.
//!
//! The interesting part is not whether it parses but what it hands to `exec`:
//! an interpreter path with no NUL, and an argument list that came out of
//! whatever the first line of a file happened to contain.

use libfuzzer_sys::fuzz_target;
use mpos::binfmt;

fuzz_target!(|data: &[u8]| {
    let Some((program, argument)) = binfmt::shebang(data) else {
        return;
    };
    assert!(
        !program.is_empty(),
        "an empty interpreter path was accepted"
    );
    assert!(
        !program.contains('\0'),
        "interpreter path {program:?} carries a NUL"
    );
    if let Some(argument) = &argument {
        assert!(
            !argument.contains('\0'),
            "interpreter argument {argument:?} carries a NUL"
        );
    }
});
