#![no_main]

//! Signal numbers, pending sets and dispositions.
//!
//! Every one of these is indexed by a byte that came from a syscall argument:
//! `rt_sigaction`, `rt_sigprocmask`, `kill`. The module's own contract is that
//! an out-of-range number is ignored rather than fatal - `Actions::set` reaches
//! for `get_mut` precisely so a bogus signal cannot index past the table - and
//! that a pending set is a `u32` of bits where `bit(sig)` wraps with `% 32`.
//! Both are asserted here, along with the ordering `take` promises: the lowest
//! number first, each signal once, and the set empty when it says it is.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::sched::signal::{self, Action, Actions, SIGKILL, SIGNALS, SIGSTOP};

#[derive(Arbitrary, Debug)]
struct Input {
    /// Signal numbers, in the order they arrive.
    signals: Vec<u8>,
    /// The disposition a `rt_sigaction` would install.
    handler: usize,
    flags: usize,
    mask: u64,
    /// The blocked set a handler would run under.
    blocked: u32,
}

fuzz_target!(|input: Input| {
    let mut actions = Actions::default();
    let action = Action {
        handler: input.handler,
        flags: input.flags,
        restorer: 0,
        mask: input.mask,
    };

    for &sig in &input.signals {
        actions.set(sig, action);
        let got = actions.get(sig);
        if (sig as usize) < SIGNALS && sig >= 1 {
            assert_eq!(
                got, action,
                "signal {sig} lost the disposition it was given"
            );
        }
        let _ = signal::default_action(sig);
        let _ = signal::stops_by_default(sig);
        let _ = action.handler_mask(sig, input.blocked);
    }

    // Only the signals a process may catch have a disposition that means
    // anything, and the two the kernel reserves are not among them.
    assert!(!signal::catchable(0), "signal 0 is not a signal");
    assert!(
        !signal::catchable(SIGKILL as usize),
        "SIGKILL cannot be caught"
    );
    assert!(
        !signal::catchable(SIGSTOP as usize),
        "SIGSTOP cannot be caught"
    );
    assert!(
        !signal::catchable(SIGNALS),
        "nothing at the table's end is catchable"
    );

    // The pending set: `add` then `take` must hand back the lowest bit first,
    // never a signal that was not added, and drain the set completely.
    let mut pending: signal::Pending = 0;
    let bits: std::collections::BTreeSet<u8> = input
        .signals
        .iter()
        .take(256)
        .map(|&sig| {
            signal::add(&mut pending, sig);
            assert_ne!(signal::bit(sig), 0, "bit({sig}) is the empty set");
            sig % 32
        })
        .collect();

    let mut drained: Vec<u8> = Vec::new();
    while let Some(sig) = signal::take(&mut pending) {
        assert!(
            bits.contains(&sig),
            "take produced {sig}, which no `add` set"
        );
        assert!(
            drained.last().copied().is_none_or(|last| last < sig),
            "signals were taken out of order: {drained:?} then {sig}"
        );
        drained.push(sig);
    }
    assert_eq!(pending, 0, "a drained set still has bits left");
    assert_eq!(
        drained.len(),
        bits.len(),
        "`bit` wraps at 32, so every distinct bit added has to come out again"
    );
});
