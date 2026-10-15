#![no_std]

//! The time namespace: the offsets a task's monotonic clocks are read through.
//!
//! A notebook whose clock is wrong by a day, or a container restoring a
//! checkpoint taken an hour ago, both want the same thing: a process that
//! believes `CLOCK_MONOTONIC` started earlier (or later) than the machine's
//! did, without the machine's clock moving for anybody else. That is what Linux
//! calls a **time namespace**, and it is the fifth of `namespaces(7)`'s eight
//! this kernel has built (`docs/notes/06-process-management.md` §6.3 is the
//! ladder and the measurements).
//!
//! Two facts about the shape are counter-intuitive enough to state before
//! anything else, because every design decision here follows from them and both
//! are measured rather than recalled:
//!
//! * **`unshare(CLONE_NEWTIME)` does not move the caller.** It replaces
//!   `time_ns_for_children` alone - the slot `nsproxy` keeps for where the next
//!   child goes - so the calling process's own clocks never move, whatever the
//!   offsets are later set to, and it is the *children* it makes afterwards
//!   that see an offset. `pid_ns_for_children` works the same way, and the
//!   `/proc/<pid>/ns` directory spells both out with an extra name
//!   (`time_for_children`, `pid_for_children`).
//! * **The offsets freeze when anything enters the namespace.** Linux refuses
//!   the write with `EACCES` from that instant onwards, which is the whole
//!   reason the file is written *before* the fork that uses it: a namespace
//!   whose offset changed under a running process would have that process see
//!   time jump, and the clock would stop being monotonic.
//!
//! The offsets are read and written through `/proc/<pid>/timens_offsets`, and
//! the grammar of that file is here rather than in the syscall layer for the
//! ordinary reason a leaf crate exists: it is a parser, and a parser is argued
//! with by host tests. Every branch below is one line of a measurement table;
//! §6.3 carries the probes.
//!
//! **What is not here**: the number the namespace is known by (minted by
//! `src/vfs/ns.rs`, the one pool every kind draws from), the slot on the task
//! that holds one of these, and the clock reading that is offset by it. This is
//! the value; those are whose it is and what reads it.

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};
use critical_section::Mutex;

/// One end of an offset, in the two halves `struct timespec` is written in:
/// whole seconds and a non-negative nanosecond remainder.
///
/// A `wat` pair rather than a count of nanoseconds because the file *prints*
/// those two fields and a reader parses them back, and because the seconds
/// half can be any `i64` (a negative offset is accepted - measured). Only the
/// nanosecond half is bounded, and only from below.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Offset {
    pub sec: i64,
    pub nsec: i64,
}

impl Offset {
    /// The whole offset as nanoseconds, saturating rather than wrapping.
    ///
    /// Saturating because the seconds half is whatever a writer put in the
    /// file: a caller that wrote `monotonic 9223372036854775807 0` and one that
    /// wrote `monotonic -9223372036854775808 0` are both inside the grammar
    /// (only `nsec` is bounded), and a clock read that panicked on arithmetic
    /// overflow would be a userland write turning into a kernel fault. The
    /// saturated answer is a clock at the far end of `i64` nanoseconds, which is
    /// a nonsense reading and *is* what such a write asked for.
    pub fn total_nanos(self) -> i64 {
        self.sec
            .saturating_mul(1_000_000_000)
            .saturating_add(self.nsec)
    }
}

/// The clock names the offsets file accepts, **in the order the pair of offsets
/// is indexed by** - so `NAMES[0]` names `Offsets[0]` and a parse can go
/// straight from the name it matched to the slot it replaces.
///
/// A name outside this list is `EINVAL` - measured, along with the fact that the
/// name must be followed by whitespace (`monotonicX` is refused, not read as a
/// prefix).
pub const NAMES: [&str; 2] = ["monotonic", "boottime"];

/// One time namespace: the pair of offsets its members read their monotonic
/// clocks through, the number it is known by, and whether it has been entered.
///
/// The offsets are behind the ordinary `critical_section::Mutex<RefCell<_>>`
/// because a namespace is *shared*: every child of the process that made it
/// points at the same object, so what the offsets file writes has to be visible
/// to all of them and not merely to the writer. In practice they are written
/// once, before any of those children exist, and the freeze below is what keeps
/// it that way - the lock is there because "in practice" is not a property a
/// shared object gets to rely on.
pub struct TimeNs {
    /// The number this namespace is known by: `time:[<id>]` in
    /// `/proc/<pid>/ns/time`, and what `setns` compares two of them by.
    id: u64,
    offsets: Mutex<RefCell<[Offset; 2]>>,
    /// Whether a task has been created in this namespace yet.
    ///
    /// Linux's `timens_offsets_write` refuses the write the moment one has, and
    /// this is the same predicate: [`TimeNs::freeze`] is called by the
    /// scheduler at exactly the point a child is put into the namespace. It is
    /// a separate flag rather than "are there members" because this kernel does
    /// not count members - nothing here frees a namespace, so the count would
    /// have no reader (the gap is named in §6.3).
    frozen: AtomicBool,
}

impl TimeNs {
    /// A namespace nobody has entered yet: zero offsets, and the file writable.
    ///
    /// Zero rather than a copy of the parent's offsets, because that is what
    /// `unshare(CLONE_NEWTIME)` produces - Linux's `create_time_ns` makes an
    /// empty one - and it is why the offsets have to be written *after* the
    /// unshare and *before* the fork. A copy would make the fresh namespace
    /// inherit whatever the caller had set up for its previous children, which
    /// is not what a second `unshare` is asking for.
    pub fn fresh(id: u64) -> Arc<TimeNs> {
        Arc::new(TimeNs {
            id,
            offsets: Mutex::new(RefCell::new([Offset::default(); 2])),
            frozen: AtomicBool::new(false),
        })
    }

    /// The machine's own namespace: the one a boot begins in, which is frozen
    /// from the first task rather than after one.
    ///
    /// It is frozen from the start because `init` is in it - there is no
    /// instant at which the machine's namespace had no member - and the
    /// measurement agrees from the other direction: a write to
    /// `/proc/self/timens_offsets` on a machine that has never unshared answers
    /// `EACCES`, and the file reads two zero lines.
    pub fn machine(id: u64) -> Arc<TimeNs> {
        let ns = TimeNs::fresh(id);
        ns.freeze();
        ns
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Record that a task has been created in this namespace, after which the
    /// offsets are fixed.
    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::Relaxed);
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> [Offset; 2] {
        critical_section::with(|cs| *self.offsets.borrow(cs).borrow())
    }

    /// The pair as nanoseconds, `(monotonic, boottime)`. What the clock reader
    /// adds to an uptime; zero on both for every namespace nobody has written
    /// to, which is every ordinary task.
    pub fn offset_nanos(&self) -> (i64, i64) {
        let [monotonic, boottime] = self.snapshot();
        (monotonic.total_nanos(), boottime.total_nanos())
    }

    /// The file's contents: two lines, `monotonic <sec> <nsec>` and
    /// `boottime <sec> <nsec>`.
    ///
    /// The padding is Linux's and is reproduced rather than approximated: the
    /// measured line is 31 bytes, `"monotonic"` then a twelve-wide seconds field
    /// then a ten-wide nanoseconds field. Whether anything parses the spaces is
    /// not the point - a file whose text differs from a real kernel's is a file
    /// some `diff` will fail on, and there is no cost to being the same.
    pub fn offsets_text(&self) -> String {
        let [monotonic, boottime] = self.snapshot();
        let mut out = String::new();
        for (name, offset) in NAMES.iter().zip([monotonic, boottime]) {
            // The write cannot fail on a `String`, and a line that could not be
            // written would be a silently short file - so the result is dropped
            // rather than a signature that pretends it might.
            let _ = writeln!(out, "{name:<9} {:>11} {:>9}", offset.sec, offset.nsec);
        }
        out
    }

    /// Replace the offsets from the text a writer handed the file.
    ///
    /// Three answers, and each is one line of the measurement table:
    ///
    /// * **`EACCES`** once a task has been created in this namespace. The
    ///   offsets have to be set before anything is in the namespace, and the
    ///   refusal is total: a failed write changes nothing.
    /// * **`EINVAL`** for text the grammar does not accept - a name that is not
    ///   one of the two, a missing or unparsable field, a nanosecond field
    ///   outside `0..1_000_000_000`, or nothing at all. Again nothing changes:
    ///   the grammar is checked against a copy and committed only when the whole
    ///   text parsed, which is the difference between a refusal and a half-write.
    /// * **`Ok`**, in which case the two clocks named are replaced and the ones
    ///   not named keep what they had. That last half is measured: writing only
    ///   `monotonic` after setting both leaves `boottime` exactly where it was,
    ///   so a partial write is a partial update and not a reset.
    pub fn write_offsets(&self, text: &str) -> Result<(), isize> {
        if self.is_frozen() {
            return Err(mpos_err::EACCES);
        }
        let mut next = self.snapshot();
        if parse_offsets(text, &mut next).is_err() {
            return Err(mpos_err::EINVAL);
        }
        critical_section::with(|cs| *self.offsets.borrow(cs).borrow_mut() = next);
        Ok(())
    }
}

/// Parse the offsets file into `into`, which starts as the values it already
/// has, so the clocks a text does not name keep theirs.
///
/// The grammar, all of it measured (§6.3, `.workbuddy/probe-timens{7,9,10}.c`):
/// one line per clock, in **either order**; leading whitespace and any run of
/// whitespace between the fields; a trailing newline that may be missing; text
/// after the two numbers ignored; whole seconds that may be negative; and a
/// nanosecond field that may not be. Anything else is the caller's mistake -
/// including an empty text, which is `EINVAL` rather than a no-op.
fn parse_offsets(text: &str, into: &mut [Offset; 2]) -> Result<(), ()> {
    /// The nanosecond field's range, from the measurement that refused both
    /// `-1` and `1000000000`.
    const NANOS: core::ops::Range<i64> = 0..1_000_000_000;

    let mut named = 0usize;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else {
            // A blank line is whitespace, not a mistake: the text a writer
            // hands over is theirs, and a trailing or doubled newline names no
            // clock rather than an unknown one.
            continue;
        };
        let which = NAMES.iter().position(|n| *n == name).ok_or(())?;
        let sec = words.next().ok_or(())?.parse::<i64>().map_err(|_| ())?;
        let nsec = words.next().ok_or(())?.parse::<i64>().map_err(|_| ())?;
        if !NANOS.contains(&nsec) {
            return Err(());
        }
        into[which] = Offset { sec, nsec };
        named += 1;
    }
    // Nothing named is a refusal rather than a write that changed nothing: an
    // empty write is a caller that believes it set something.
    if named == 0 {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes a reader gets, pinned against a real kernel's rather than
    /// against this module's own formatter: measured on this developer's Linux
    /// (7.2), `/proc/self/timens_offsets` of a machine that has never unshared
    /// is two 31-byte lines.
    #[test]
    fn the_offsets_text_is_what_a_real_kernel_prints() {
        let ns = TimeNs::fresh(1);
        assert_eq!(
            ns.offsets_text(),
            "monotonic           0         0\nboottime            0         0\n"
        );
        assert_eq!(ns.offsets_text().lines().next().unwrap().len(), 31);
    }

    /// A reading's text is what the file then hands back, so the two doors
    /// cannot disagree - and a negative seconds half prints as one.
    #[test]
    fn what_was_written_is_what_is_read() {
        let ns = TimeNs::fresh(1);
        ns.write_offsets("monotonic -5 123456789\nboottime 200000 0\n")
            .unwrap();
        assert_eq!(
            ns.offsets_text(),
            "monotonic          -5 123456789\nboottime       200000         0\n"
        );
        assert_eq!(
            ns.offset_nanos(),
            (-5 * 1_000_000_000 + 123_456_789, 200_000 * 1_000_000_000)
        );
    }

    /// The grammar's acceptances, each one a measured line. A test that only
    /// fed the canonical text would pass for a parser that refused everything a
    /// real writer emits.
    #[test]
    fn the_grammar_accepts_what_a_real_kernel_accepts() {
        // Either order.
        let ns = TimeNs::fresh(1);
        assert_eq!(ns.write_offsets("boottime 4 0\nmonotonic 3 0\n"), Ok(()));
        assert_eq!(ns.offset_nanos(), (3 * 1_000_000_000, 4 * 1_000_000_000));

        // Leading whitespace, tabs, no trailing newline, text after the fields.
        let ns = TimeNs::fresh(2);
        assert_eq!(
            ns.write_offsets("  monotonic\t3\t0   trailing\nboottime 4 0"),
            Ok(())
        );
        assert_eq!(ns.offset_nanos(), (3 * 1_000_000_000, 4 * 1_000_000_000));

        // A blank line names no clock.
        let ns = TimeNs::fresh(3);
        assert_eq!(ns.write_offsets("\nmonotonic 1 0\n\n"), Ok(()));
        assert_eq!(ns.offset_nanos(), (1_000_000_000, 0));

        // Negative seconds.
        let ns = TimeNs::fresh(4);
        assert_eq!(ns.write_offsets("monotonic -1 0\n"), Ok(()));
        assert_eq!(ns.offset_nanos(), (-1_000_000_000, 0));
    }

    /// And its refusals: `EINVAL` for each, and **nothing changed** for each -
    /// which is the half a test that only checked the errno would miss.
    #[test]
    fn the_grammar_refuses_what_a_real_kernel_refuses() {
        for text in [
            "",                         // nothing at all
            "wallclock 1 0\n",          // an unknown clock name
            "monotonicX 1 0\n",         // a name with a suffix, not a prefix match
            "monotonic 1\n",            // a missing field
            "monotonic 1 1000000000\n", // the nanosecond field's ceiling
            "monotonic 1 -1\n",         // ...and its floor
            "monotonic x 0\n",          // an unparsable field
        ] {
            let ns = TimeNs::fresh(1);
            assert_eq!(ns.write_offsets(text), Err(mpos_err::EINVAL), "{text:?}");
            assert_eq!(ns.offset_nanos(), (0, 0), "{text:?} changed something");
        }
    }

    /// A write naming one clock leaves the other where it was - measured, and
    /// the reason the parse starts from the current pair rather than from zero.
    #[test]
    fn a_partial_write_leaves_the_other_clock_alone() {
        let ns = TimeNs::fresh(1);
        ns.write_offsets("monotonic 3 0\nboottime 4 0\n").unwrap();
        ns.write_offsets("monotonic 9 0\n").unwrap();
        assert_eq!(ns.offset_nanos(), (9_000_000_000, 4_000_000_000));
        ns.write_offsets("boottime 7 0\n").unwrap();
        assert_eq!(ns.offset_nanos(), (9_000_000_000, 7_000_000_000));
    }

    /// Once something is in the namespace the offsets are fixed, and the
    /// machine's namespace is fixed from the start because `init` is in it.
    #[test]
    fn a_frozen_namespace_refuses_every_write() {
        let ns = TimeNs::fresh(1);
        ns.freeze();
        assert_eq!(
            ns.write_offsets("monotonic 1 0\nboottime 1 0\n"),
            Err(mpos_err::EACCES)
        );
        assert_eq!(ns.offset_nanos(), (0, 0));

        let machine = TimeNs::machine(1);
        assert!(machine.is_frozen());
        assert_eq!(
            machine.write_offsets("monotonic 1 0\n"),
            Err(mpos_err::EACCES)
        );
    }

    /// The absurd ends of the grammar do not panic the clock reader.
    #[test]
    fn an_unrepresentable_offset_saturates_rather_than_wrapping() {
        let ns = TimeNs::fresh(1);
        ns.write_offsets("monotonic 9223372036854775807 0\n")
            .unwrap();
        assert_eq!(ns.offset_nanos().0, i64::MAX);
        let ns = TimeNs::fresh(2);
        ns.write_offsets("monotonic -9223372036854775808 0\n")
            .unwrap();
        assert_eq!(ns.offset_nanos().0, i64::MIN);
    }
}
