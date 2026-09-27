#![no_main]

//! The device event the kernel pushes at udev, and the numbering it names
//! devices by.
//!
//! `uevent::Event::encode` produces the envp block `nlplug-findfs` and `udevd`
//! parse: NUL-separated `KEY=value` lines, where a missing `SEQNUM` is the
//! difference between a compositor starting and every event queueing behind a
//! phantom. The block is built from strings the kernel did not write - a device
//! name, a devpath - so the target's job is to prove that nothing in them can
//! forge a line, and that the numbering lookups answer for the table they were
//! built from.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::uevent::{Event, Numbering};

const ACTIONS: [&str; 3] = ["add", "remove", "change"];
const SUBSYSTEMS: [&str; 4] = ["block", "virtio", "input", "drm"];
/// The keys `encode_seq` writes, in order - `DEVNAME` only when there is one.
const KEYS: [&str; 7] = [
    "ACTION=",
    "DEVPATH=",
    "SUBSYSTEM=",
    "DEVNAME=",
    "MAJOR=",
    "MINOR=",
    "SEQNUM=",
];

fuzz_target!(|data: &[u8]| {
    let devname = support::text(data, 32);
    let devpath = support::text(data, 96);

    let event = Event {
        action: ACTIONS[data.len() % ACTIONS.len()],
        devpath,
        subsystem: SUBSYSTEMS[(data.len() >> 2) % SUBSYSTEMS.len()],
        devname,
        major: support::u32_at(data, 0),
        minor: support::u32_at(data, 4),
    };

    let (block, seq) = event.encode_seq();
    let lines: Vec<&[u8]> = block.split(|b| *b == 0).filter(|l| !l.is_empty()).collect();
    assert!(
        block.last().copied() == Some(0),
        "an envp block that does not end in a NUL"
    );
    // Linux's uevent message is `ACTION=<action>\0`-led by the netlink header's
    // own `action@devpath` string, and udev parses that pair before it looks at
    // any key. Both halves have to be there, in that order.
    assert!(
        !lines.is_empty() && lines[0].starts_with(event.action.as_bytes()),
        "a uevent that does not open with its action: {lines:?}"
    );
    assert!(
        lines[0].contains(&b'@'),
        "the first line is `action@devpath`, and this one has no `@`: {:?}",
        String::from_utf8_lossy(lines[0])
    );
    for key in [
        b"ACTION=".as_slice(),
        b"DEVPATH=",
        b"SUBSYSTEM=",
        b"SEQNUM=",
    ] {
        assert!(
            lines.iter().any(|l| l.starts_with(key)),
            "a uevent with no {} line",
            String::from_utf8_lossy(&key[..key.len() - 1])
        );
    }
    // What a name must not be able to do is change how many records the block
    // holds. Splitting on the NUL that ends each line therefore has to yield a
    // fixed count - seven keys, plus `DEVNAME` when the event carries one - and
    // every record after the message line has to open with a key this kernel
    // knows. A value that merely *contains* an `=` is a different matter, and
    // allowed: the reader splits a line on its first `=`, so an `=` inside
    // `DEVPATH` changes nothing about where the records start, while a second
    // `SEQNUM=` line would be a whole event udevd never sent.
    let expected = 7 + usize::from(!event.devname.is_empty());
    assert_eq!(
        lines.len(),
        expected,
        "a device name changed how many records one uevent holds"
    );
    for line in lines.iter().skip(1) {
        assert!(
            KEYS.iter().any(|key| line.starts_with(key.as_bytes())),
            "a uevent record that opens with no known key: {:?}",
            String::from_utf8_lossy(line)
        );
    }
    assert_eq!(
        lines
            .iter()
            .find(|l| l.starts_with(b"SEQNUM="))
            .map(|l| &l[7..])
            .and_then(|v| core::str::from_utf8(v).ok())
            .and_then(|v| v.parse::<u64>().ok()),
        Some(seq),
        "the SEQNUM line and the number the caller got are not the same"
    );

    // The second event on the same counter has to be a later one, or udevd
    // waits behind an event that already ran.
    let (_, seq2) = event.encode_seq();
    assert!(seq2 > seq, "seqnum did not advance: {seq} then {seq2}");

    // The numbering table: what `/sys/class/block/*/dev` and `/dev` agree on.
    let devices: Vec<(String, u32, u32, bool)> = (0..8u32)
        .map(|i| {
            (
                format!("vd{}", i % 4),
                254,
                i,
                support::u32_at(data, 8) & (1 << (i % 8)) != 0,
            )
        })
        .collect();
    let numbering = Numbering {
        devices: devices.clone(),
    };
    for name in ["vd0", "vd1", "vda", "", "virtio0"] {
        if let Some((major, minor, whole)) = numbering.find(name) {
            let (want_major, want_minor, want_whole) = devices
                .iter()
                .find(|(n, ..)| n == name)
                .map(|&(_, major, minor, whole)| (major, minor, whole))
                .expect("find answered for a name the table does not hold");
            assert_eq!((major, minor, whole), (want_major, want_minor, want_whole));
        }
    }
});
