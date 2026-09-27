#![no_main]

//! The inotify record's wire layout, and the mask a watch fires on.
//!
//! `read(2)` on an inotify fd hands a program nothing but bytes: a fixed
//! sixteen-byte header, then a name padded so the next record starts on the
//! alignment `inotify(7)` promises. A record whose declared length and actual
//! bytes disagree makes busybox's `inotifywait` read the middle of the next one
//! as a filename. `event_matches` is the other half - the rule that decides
//! whether a watch fires at all, and the one place `IN_ISDIR` means "filter"
//! rather than "event".

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::vfs::inotify::{self, EVENT_ALIGN, EVENT_HEADER_SIZE, InotifyEvent};

fuzz_target!(|data: &[u8]| {
    let name: Vec<u8> = data.iter().copied().take(80).collect();
    let padded = inotify::pad_name(&name);

    assert_eq!(
        inotify::record_len(&name),
        EVENT_HEADER_SIZE + padded.len(),
        "the length a record claims and the bytes it is made of differ"
    );
    assert!(
        padded.len() % EVENT_ALIGN == 0,
        "a padded name of {} bytes leaves the next record unaligned",
        padded.len()
    );
    assert!(
        padded.len() >= name.len() + if name.is_empty() { 0 } else { 1 },
        "the padded field is shorter than the name it holds"
    );
    if !name.is_empty() {
        assert_eq!(
            &padded[..name.len()],
            &name[..],
            "the name came out changed"
        );
        assert!(
            padded[name.len()..].iter().all(|&b| b == 0),
            "padding that is not zero is a filename userland will print"
        );
    } else {
        assert!(
            padded.is_empty(),
            "a nameless record carries a padding field"
        );
    }

    // Serialize into a buffer of exactly the length advertised, and read the
    // header back. This is the only check a reader can do: `len` and `name`
    // have to agree, or the next record starts in the wrong place.
    let event = InotifyEvent {
        wd: support::u32_at(data, 0) as i32,
        mask: support::u32_at(data, 4),
        cookie: support::u32_at(data, 8),
        name: padded.clone(),
    };
    let total = event.len();
    assert_eq!(total, EVENT_HEADER_SIZE + padded.len());
    let mut buf = vec![0xAAu8; total];
    event.write_to(&mut buf);
    assert_eq!(&buf[..4], &event.wd.to_ne_bytes()[..]);
    assert_eq!(&buf[4..8], &event.mask.to_ne_bytes()[..]);
    assert_eq!(&buf[8..12], &event.cookie.to_ne_bytes()[..]);
    assert_eq!(
        u32::from_ne_bytes(buf[12..16].try_into().unwrap()) as usize,
        padded.len(),
        "the header's `len` field is not the name the record carries"
    );
    assert_eq!(&buf[EVENT_HEADER_SIZE..], &padded[..]);

    // The mask rule, over every combination of the bits that matter to it.
    for watch_mask in [
        0u32,
        1,
        2,
        inotify::IN_ISDIR,
        inotify::IN_ISDIR | 4,
        u32::MAX,
    ] {
        for ev_mask in [0u32, 1, 2, 4, inotify::IN_ISDIR, u32::MAX] {
            for ev_is_dir in [false, true] {
                let fired = inotify::event_matches(watch_mask, ev_mask, ev_is_dir);
                // A watch that names none of the event's bits, and is not the
                // directory filter on a directory event, must stay silent: a
                // spurious wake is what makes `inotifywait` loop.
                assert_eq!(
                    fired,
                    (watch_mask & !inotify::IN_ISDIR & ev_mask) != 0
                        || (ev_is_dir && watch_mask & inotify::IN_ISDIR != 0),
                    "watch {watch_mask:#x} on event {ev_mask:#x} (dir {ev_is_dir})"
                );
            }
        }
    }
    assert!(event.is_empty() == event.name.is_empty());
});
