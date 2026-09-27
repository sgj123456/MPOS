#![no_main]

//! evdev's record window and who may read it.
//!
//! `src/evdev.rs` holds the arithmetic behind `/dev/input/eventN`: one window
//! of records and, per open file description, a position in it. The driver
//! around it needs a virtio ring and a clock, but this policy is plain state,
//! and it is where a mistake is a reader that is handed somebody else's
//! events, or one that is never woken, rather than a crash. The questions the
//! host tests ask one at a time are asked here over an arbitrary sequence of
//! opens, reads, grabs and releases: does `take` ever answer with a partial
//! record, does `readable` agree with what `take` then does, is a non-owner
//! told nothing while a grab is held, and is a waiter only ever due when the
//! record that landed is for it.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::evdev::{RECORD, Records};
use mpos::sched::policy::TaskId;

#[derive(Arbitrary, Debug)]
enum Op {
    /// A description opens the node and takes its position.
    Open {
        ofd: u16,
    },
    /// Its last descriptor closes.
    Forget {
        ofd: u16,
    },
    /// The device reports one record.
    Push {
        tag: u8,
    },
    /// `readable` and then the read it promises, with nothing in between.
    Probe {
        ofd: u16,
    },
    /// A read smaller than one event - `EINVAL` (no answer at all).
    TakeSmall {
        ofd: u16,
    },
    Claim {
        ofd: u16,
    },
    Release {
        ofd: u16,
    },
    Subscribe {
        ofd: u16,
        id: u16,
    },
    Unsubscribe {
        id: u16,
    },
    /// The device is taken away.
    Close,
    /// The set of waits a record landing now would satisfy.
    Due,
}

/// A description's identity: 0 is reserved for "nobody holds the grab", so
/// every client is mapped to an odd number.
fn ofd_of(raw: u16) -> u64 {
    (raw as u64) | 1
}

fuzz_target!(|ops: Vec<Op>| {
    let mut records = Records::new();
    // The target's own picture of who subscribed, so `due_waiters` - which
    // answers ids only - can be checked against the client each id belongs to.
    let mut subscribed: Vec<(TaskId, u64)> = Vec::new();
    let mut buf = [0u8; RECORD * 4];
    let mut tag = 0u8;

    for op in ops.iter().take(512) {
        match *op {
            Op::Open { ofd } => records.opened(ofd_of(ofd)),
            // `forget` drops a description's *position*, not its waits: a
            // client can be reading again without re-opening, so the target's
            // picture of who subscribed must not be pruned here.
            Op::Forget { ofd } => records.forget(ofd_of(ofd)),
            Op::Push { tag: op_tag } => {
                tag = tag.wrapping_add(op_tag);
                let mut record = [0u8; RECORD];
                record[0] = tag;
                records.push(record);
            }
            Op::Probe { ofd } => {
                let ofd = ofd_of(ofd);
                let readable = records.readable(ofd);
                let n = records
                    .take(ofd, &mut buf)
                    .expect("a buffer of four records is never too small");
                assert_eq!(
                    readable,
                    n > 0,
                    "readable said {readable} and the read then took {n} bytes",
                );
                assert_eq!(n % RECORD, 0, "a read took a partial record");
                assert!(n <= buf.len(), "a read took more than the buffer");
            }
            Op::TakeSmall { ofd } => {
                let mut tiny = [0u8; RECORD - 1];
                assert_eq!(
                    records.take(ofd_of(ofd), &mut tiny),
                    None,
                    "a buffer smaller than one record was accepted",
                );
            }
            Op::Claim { ofd } => {
                let ofd = ofd_of(ofd);
                let before = records.grabbed_by();
                let ok = records.set_grab(true, ofd);
                let after = records.grabbed_by();
                if ok {
                    assert_eq!(after, ofd, "a claim that was accepted did not take hold");
                } else {
                    assert_ne!(before, 0, "a claim was refused with nobody holding it");
                    assert_ne!(before, ofd, "the holder was refused its own claim");
                    assert_eq!(after, before, "a refused claim changed the holder");
                }
            }
            Op::Release { ofd } => {
                let ofd = ofd_of(ofd);
                let before = records.grabbed_by();
                assert!(
                    records.set_grab(false, ofd),
                    "a release was refused, which Linux never does",
                );
                let after = records.grabbed_by();
                if before == ofd {
                    assert_eq!(after, 0, "the holder's release did not drop the grab");
                } else {
                    assert_eq!(after, before, "a stranger's release moved the grab");
                }
            }
            Op::Subscribe { ofd, id } => {
                let ofd = ofd_of(ofd);
                let id = TaskId(id as usize);
                records.subscribe(ofd, id);
                if !subscribed.contains(&(id, ofd)) {
                    subscribed.push((id, ofd));
                }
            }
            Op::Unsubscribe { id } => {
                let id = TaskId(id as usize);
                records.unsubscribe(id);
                subscribed.retain(|&(waiter, _)| waiter != id);
            }
            Op::Close => {
                records.close();
                assert!(records.closed());
            }
            Op::Due => {
                let due = records.due_waiters();
                let all = records.all_waiters();
                let grab = records.grabbed_by();
                for id in &due {
                    assert!(all.contains(id), "a due waiter was not a registered one");
                    // `subscribe` is keyed by (client, id), so one id can be
                    // parked under more than one description; it is due when
                    // *any* of them may read.
                    let mut holders = subscribed
                        .iter()
                        .filter(|&&(waiter, _)| waiter == *id)
                        .map(|&(_, holder)| holder)
                        .peekable();
                    assert!(
                        holders.peek().is_some(),
                        "a due waiter was never subscribed",
                    );
                    assert!(
                        !records.closed() && holders.any(|h| grab == 0 || grab == h),
                        "a waiter was made due while {grab} held the grab and it did not",
                    );
                }
            }
        }

        // The invariant that ties the two answers together, whatever happened:
        // a read by a client that may not read takes nothing.
        let grab = records.grabbed_by();
        if grab != 0 {
            let stranger = if grab == 1 { 3 } else { 1 };
            assert!(
                !records.readable(stranger),
                "a non-owner was told a grabbed device was readable",
            );
        }
    }
});
