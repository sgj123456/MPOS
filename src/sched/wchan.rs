//! `/proc/<pid>/wchan`: the wait a task is in **now**, as a name.
//!
//! Two files in this tree answer a question about where a task is waiting, and
//! they are not the same question. `/proc/<pid>/stat`'s field 35 is a *number*
//! and it is the **last** park the task took - instrumentation for a stuck
//! machine, where the interesting task is the one that stopped moving
//! (`docs/notes/` §7 R.9). This file is Linux's `wchan`, and Linux answers it
//! from the task's *current* state: measured on a current host
//! (`build/scratch/wchan/{w,stale,f35}.c`, Linux 7.2.14), a task asleep in a
//! pipe read prints `anon_pipe_read`, and the same task one moment after it was
//! woken prints `0` - while its `stat` field 35 still reads `1`. So the two
//! doors disagree on purpose, and this module is the half that has to be live.
//!
//! The other measured facts, each of which the file's shape follows:
//!
//! * **`0` is "not waiting"**, and it is the answer for a task that is running,
//!   for a zombie, and for a task the reader may not inspect - Linux does not
//!   refuse that last case, it reports nothing (`build/scratch/wchan/perm3.c`:
//!   a child dropped to uid 65534 reads `0` from its parent's `/proc`, and
//!   `pid 1` reads `0` too). A refusal here would be a different syscall's
//!   answer.
//! * **A stop is a wait with its own name**: `do_signal_stop` for a task held by
//!   a stop signal and `ptrace_stop` for one held by its tracer
//!   (`build/scratch/wchan/{w,ptrace}.c`).
//! * **No trailing newline.** The file's whole content is the word; a reader
//!   that `read`s it gets the name and nothing else.
//! * **`0444`**, and `ENOENT` once the task has been collected
//!   (`build/scratch/wchan/perm.c`, `w.c`).
//!
//! What this module holds is only the *word*: the state that produces it is the
//! scheduler's, and the file that serves it is `vfs::procfs`. Host-compiled
//! because a name table and a match are integers and strings - the same split
//! `cpustat` and `loadavg` take.

/// Which wait a task is in, as the one fact `/proc/<pid>/wchan` needs.
///
/// A closed vocabulary rather than a `&str`, for `PARK_SITES`' reason: the
/// names must not drift from the states that produce them, and the compiler
/// should refuse a state nobody named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// Not waiting: running, runnable, a zombie, or a task the reader may not
    /// inspect. Linux prints `0` for all four.
    Nothing,
    /// Parked in one of the scheduler's primitives, by [`PARK_SITES`] id.
    Parked(u8),
    /// Held by a stop signal with nobody tracing it - Linux's `do_signal_stop`.
    GroupStop,
    /// Held by its tracer - Linux's `ptrace_stop`.
    TraceStop,
}

/// The names behind [`Wait::Parked`]'s ids.
///
/// `/proc/<pid>/stat`'s field 35 carries the **number** (Linux's is a kernel
/// address, so a number keeps the field's shape); these names are what the
/// number means, and they live here so the mapping cannot drift from the
/// primitives that write it.
pub const PARK_SITES: [&str; 12] = [
    "0",
    "block_until_on",
    "block_until_nanos",
    "block_until_interruptible_rolling_back",
    "block_until_uninterruptible",
    "block_until_code",
    "block_until_deadline",
    "ext4_slot_wait",
    "ext4_scratch_wait",
    "ext4_set_wait",
    "ext4_reentry_wait",
    "blk_claim_wait",
];

/// The word `/proc/<pid>/wchan` serves for `wait`.
///
/// `0` for [`Wait::Nothing`] and for an id with no name - an id the vocabulary
/// does not carry is "nothing to say" rather than a made-up word, which is the
/// same answer Linux gives a task it cannot name.
pub fn wchan_word(wait: Wait) -> &'static str {
    match wait {
        Wait::Nothing => "0",
        Wait::Parked(site) => PARK_SITES.get(site as usize).copied().unwrap_or("0"),
        Wait::GroupStop => "do_signal_stop",
        Wait::TraceStop => "ptrace_stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four answers the host measured, one per branch.
    #[test]
    fn the_word_is_the_live_wait_and_zero_when_there_is_none() {
        assert_eq!(wchan_word(Wait::Nothing), "0");
        assert_eq!(wchan_word(Wait::GroupStop), "do_signal_stop");
        assert_eq!(wchan_word(Wait::TraceStop), "ptrace_stop");
    }

    /// Every park id the scheduler can store has a name, and id `0` is `0` -
    /// the same word [`Wait::Nothing`] answers, because a task that never
    /// parked is not waiting.
    #[test]
    fn every_park_id_has_a_name_and_the_empty_one_is_zero() {
        assert_eq!(wchan_word(Wait::Parked(0)), "0");
        for site in 1..PARK_SITES.len() as u8 {
            let word = wchan_word(Wait::Parked(site));
            assert_ne!(word, "0", "park site {site} has no name");
            assert!(!word.is_empty());
        }
    }

    /// An id past the vocabulary is nothing to say, not a panic and not an
    /// invented word: the file still has to answer a task that is mid-park.
    #[test]
    fn an_id_with_no_name_reads_as_nothing() {
        assert_eq!(wchan_word(Wait::Parked(200)), "0");
    }

    /// The file's whole content is the word - no newline, which is the one
    /// shape difference between this file and `/proc/<pid>/comm`.
    #[test]
    fn no_name_carries_a_newline() {
        for wait in [
            Wait::Nothing,
            Wait::GroupStop,
            Wait::TraceStop,
            Wait::Parked(2),
            Wait::Parked(11),
        ] {
            assert!(!wchan_word(wait).contains('\n'));
        }
    }
}
