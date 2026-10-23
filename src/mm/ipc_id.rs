//! The number a System V IPC object is known by, and the generation behind it.
//!
//! Linux's three SysV families - shared memory, semaphore sets, message queues
//! - share one identity scheme, and it is not a counter. An object is a *slot* in
//! a fixed table (`SHMMNI`/`SEMMNI`/`MSGMNI` entries, `IPC_MNI = 32768` slots in
//! the kernel's own idr) plus the slot's **generation**, and the number a caller
//! holds is `slot + generation * 32768`. The generation exists for one reason: a
//! number that named an object somebody removed must not read as a *different*
//! object that reused its slot. `shmget` after an `IPC_RMID` on the same slot
//! answers the next generation's number, so the stale one is `EINVAL` rather
//! than a silent window onto a stranger's segment.
//!
//! Measured on a current host (Linux 7.2.11, `.workbuddy/probe_shm_ids.c`): the
//! first segment of a fresh boot is `0` with `seq` 0; after that slot has been
//! removed and made again the number is `32768` and the `shm_perm.seq` field
//! reads 1, the next two slots following as `32769`, `32770` - so the generation
//! is **per slot** and survives the slot's reuse rather than describing the
//! namespace. That is what this module implements, and why it lives apart from
//! any one family: the semaphore and queue tables will use the same arithmetic.
//!
//! The slot table itself is not here - each family owns its own, because what a
//! slot holds differs. This is only the number's spelling, which is the part
//! they must agree on for `ipcs` and for a caller that keeps a number after the
//! object behind it is gone.

/// `IPC_MNI`: the number of slots one family's table has, and the stride between
/// one generation's numbers and the next's.
///
/// Linux's `ipc_buildid` is `slot + seq * SEQ_MULTIPLIER` with this constant; a
/// table bigger than it would make two live objects collide on one number.
pub const SEQ_MULTIPLIER: u64 = 32_768;

/// The number that names `slot` of generation `seq`.
pub fn build(slot: u64, seq: u16) -> u64 {
    slot + seq as u64 * SEQ_MULTIPLIER
}

/// The slot and generation a number names, or `None` when it names no slot at
/// all.
///
/// A negative `id` is not a number any builder produced - the syscall argument
/// is a signed `int` - and is refused here rather than wrapping into a plausible
/// slot.
pub fn decode(id: i64) -> Option<(u64, u16)> {
    if id < 0 {
        return None;
    }
    let id = id as u64;
    let slot = id % SEQ_MULTIPLIER;
    let seq = id / SEQ_MULTIPLIER;
    // The generation is *inside* the stride: a number past the last slot of the
    // last generation is not a number this table can produce, and reading it as
    // one would hand back a slot index that was never built.
    if seq >= SEQ_MULTIPLIER {
        return None;
    }
    Some((slot, seq as u16))
}

/// The generation after `seq`, wrapping to zero.
///
/// The wrap is Linux's: `seq` is a `u16` in `kern_ipc_perm`, so a slot that has
/// been made and unmade 32768 times starts its numbers over. A caller cannot
/// notice except by holding a number across that many reuses, which is the same
/// exposure Linux has.
pub fn next_generation(seq: u16) -> u16 {
    let next = seq.wrapping_add(1);
    if next as u64 >= SEQ_MULTIPLIER {
        0
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic Linux's own measurement pins: a fresh slot's first number
    /// is the slot index, and the same slot one generation on is that index plus
    /// 32768.
    #[test]
    fn a_number_is_its_slot_plus_its_generation() {
        assert_eq!(build(0, 0), 0);
        assert_eq!(build(7, 0), 7);
        assert_eq!(build(0, 1), 32_768);
        assert_eq!(build(5, 1), 32_773);
        assert_eq!(decode(32_773), Some((5, 1)));
        assert_eq!(decode(0), Some((0, 0)));
    }

    /// A number that names nothing is `None`: negative numbers are not
    /// spellings, and a value past the last generation is not either.
    #[test]
    fn a_number_outside_the_table_names_nothing() {
        assert_eq!(decode(-1), None);
        assert_eq!(decode(-32_768), None);
        assert_eq!(decode(32_768 * 32_768 + 3), None);
        assert_eq!(decode(32_768 * 32_767 + 4), Some((4, 32_767)));
        assert_eq!(build(4, 32_767), 32_768 * 32_767 + 4);
    }

    /// The generation wraps at the table's stride, and every step of the walk
    /// round-trips through [`decode`].
    #[test]
    fn generations_advance_and_wrap() {
        assert_eq!(next_generation(0), 1);
        assert_eq!(next_generation(32_767), 0);
        for seq in [0u16, 1, 32_766, 32_767] {
            let id = build(17, seq);
            assert_eq!(decode(id as i64), Some((17, seq)));
        }
    }
}
