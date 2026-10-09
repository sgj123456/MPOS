#![no_main]

//! The 9P `statu` record: what an `Rstat` carries and what a readdir stream is
//! made of.
//!
//! `vfs::p9::parse_stat` walks a record whose own u16 length word decides where
//! it ends, over bytes a *server* chose. The listing reader steps from one record
//! to the next by the count it answers, so a `used` past the buffer is a readdir
//! that walks off the end of its chunk - and a slice that panics instead of
//! answering `None` takes the machine down when a share answers something odd.
//! Neither is reachable through the message framing in the well-behaved case,
//! which is exactly why the parser is fuzzed directly (`NOTES.md` §13.29).

use libfuzzer_sys::fuzz_target;
use mpos::vfs::p9::parse_stat;

fuzz_target!(|data: &[u8]| {
    // A record the parser places must lie inside the buffer it was given, and be
    // at least as long as the length word that placed it. It never needs to be
    // more: `None` is the honest answer for every shape that is not a record.
    if let Some((_stat, used)) = parse_stat(data) {
        assert!(
            used >= 2,
            "a placed record ({used} bytes) is shorter than its length word"
        );
        assert!(
            used <= data.len(),
            "a record of {used} was placed in a {}-byte buffer",
            data.len()
        );
    }
});
