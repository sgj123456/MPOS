//! The `io_uring` ring's *layout*: the constants and the two pure functions the
//! bare-metal ring object and its syscall handler both need.
//!
//! This half is host-compiled on purpose. The values here are Linux's ABI, not
//! this machine's hardware, so a host test can pin them - and it must, because
//! they are the part a wrong byte would break silently: `build/scratch/iouring/
//! probe4.c` measured every one of them on Linux 7.2.11 (an `entries=8` ring:
//! `sq_entries=8`, `cq_entries=16`, `features=0x3ffff`, `sq_off` at bytes
//! `0,4,16,24,36,32,array=320`, `cq_off` at bytes `8,12,20,28,44,64,40`), and a
//! caller computes its pointers from these words. `params_words` is the single
//! place this kernel spells them; the tests below check it word for word
//! against those measurements, and check that the same offsets the ring's
//! `write_u32` calls use agree with the words it publishes.

/// `mmap`'s magic offsets: the SQ ring, the CQ ring, and the SQE array.
pub const IORING_OFF_SQ_RING: u64 = 0;
pub const IORING_OFF_CQ_RING: u64 = 0x800_0000;
pub const IORING_OFF_SQES: u64 = 0x1000_0000;

/// The features this kernel can honestly publish: one mapping covers both rings
/// (`SINGLE_MMAP`), and a completion is never dropped (`NODROP`, trivially true
/// of a synchronous ring). Every other bit in Linux's mask is a promise about
/// deferred work or about part of the ABI this kernel does not serve.
pub const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
pub const IORING_FEAT_NODROP: u32 = 1 << 1;
pub const FEATURES: u32 = IORING_FEAT_SINGLE_MMAP | IORING_FEAT_NODROP;

/// The largest SQ ring served, and the number of live rings: memory bounds, not
/// ABI facts (Linux's own ceiling is 32768 SQEs; this kernel serves 1024 so the
/// ring's page cost stays inside §8.0's allocation bound).
pub const MAX_SQ_ENTRIES: u32 = 1024;
pub const MAX_RINGS: usize = 64;

/// `IORING_OP_LAST` from the uapi header this work measured against: an opcode
/// at or beyond it is not an opcode Linux has, and Linux answers `EINVAL` for
/// it (measured: opcode 250). An opcode below it that this kernel does not
/// serve is refused per item with `EOPNOTSUPP` instead - the request is honest
/// and the ring's other items keep running. A Linux newer than this header
/// would serve some opcodes below 65 that this table calls unserved, which the
/// notes name as the divergence rather than pretending the sets match.
pub const IORING_OP_LAST: u8 = 65;

/// The offsets of everything in the ring page range: Linux's `io_sqring_offsets`
/// and `io_cqring_offsets` values for this layout, which is Linux's own
/// (`single mmap`: both rings in one page range, CQEs at 64, the SQ array of
/// SQE indices after them).
pub const SQ_HEAD: usize = 0;
pub const SQ_TAIL: usize = 4;
pub const CQ_HEAD: usize = 8;
pub const CQ_TAIL: usize = 12;
pub const SQ_MASK: usize = 16;
pub const CQ_MASK: usize = 20;
pub const SQ_ENTRIES: usize = 24;
pub const CQ_ENTRIES: usize = 28;
pub const SQ_DROPPED: usize = 32;
pub const SQ_FLAGS: usize = 36;
pub const CQ_FLAGS: usize = 40;
pub const CQ_OVERFLOW: usize = 44;
/// Where the 16-byte `io_uring_cqe`s start, and how big each is.
pub const CQES: usize = 64;
pub const CQE_SIZE: usize = 16;
/// The submission entry's size, and the `io_uring_params` record's.
pub const SQE_SIZE: usize = 64;
pub const PARAMS_BYTES: usize = 120;

/// The SQ array's offset for a ring of `cq_entries` completions - the first
/// word of the layout that depends on the ring's size.
pub fn array_offset(cq_entries: u32) -> usize {
    CQES + cq_entries as usize * CQE_SIZE
}

/// Round the requested SQ size the way Linux does: up to a power of two,
/// refusing zero and anything past [`MAX_SQ_ENTRIES`] (measured: `entries=3`
/// gives `sq=4`, `entries=9` gives `sq=16`, `entries=32769` is `EINVAL`).
pub fn sq_entries_for(entries: u32) -> Option<u32> {
    if entries == 0 || entries > MAX_SQ_ENTRIES {
        return None;
    }
    Some(entries.next_power_of_two())
}

/// The `io_uring_params` words for a ring of these sizes, exactly as
/// `io_uring_setup` returns them: 30 words, 120 bytes, with `sq_off` at word 10
/// and `cq_off` at word 20 (bytes 40 and 80).
pub fn params_words(sq_entries: u32, cq_entries: u32) -> [u32; 30] {
    let mut w = [0u32; 30];
    w[0] = sq_entries;
    w[1] = cq_entries;
    // w[2] = flags (echoed setup flags; a ring with flags is refused, so 0).
    // w[3], w[4] = sq_thread_cpu, sq_thread_idle: no poll thread.
    w[5] = FEATURES;
    // w[6] = wq_fd, w[7..=9] = resv.
    w[10] = SQ_HEAD as u32;
    w[11] = SQ_TAIL as u32;
    w[12] = SQ_MASK as u32;
    w[13] = SQ_ENTRIES as u32;
    w[14] = SQ_FLAGS as u32;
    w[15] = SQ_DROPPED as u32;
    w[16] = array_offset(cq_entries) as u32;
    // w[17] = resv1, w[18..=19] = sq_off.user_addr (0: not a registered ring).
    w[20] = CQ_HEAD as u32;
    w[21] = CQ_TAIL as u32;
    w[22] = CQ_MASK as u32;
    w[23] = CQ_ENTRIES as u32;
    w[24] = CQ_OVERFLOW as u32;
    w[25] = CQES as u32;
    w[26] = CQ_FLAGS as u32;
    // w[27] = resv1, w[28..=29] = cq_off.user_addr.
    w
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::vec;

    /// The measured `entries=8` ring's params, word for word: sizes, the
    /// feature words this kernel can keep, and every offset Linux returned
    /// (`sq_off` 0/4/16/24/36/32/320, `cq_off` 8/12/20/28/44/64/40).
    #[test]
    fn the_published_params_are_the_measured_linux_layout() {
        let w = params_words(8, 16);
        assert_eq!(w[0], 8, "sq_entries");
        assert_eq!(w[1], 16, "cq_entries");
        assert_eq!(
            w[2], 0,
            "flags are echoed back, and a flagged ring is refused"
        );
        assert_eq!(
            w[5],
            IORING_FEAT_SINGLE_MMAP | IORING_FEAT_NODROP,
            "features"
        );
        // sq_off: head, tail, ring_mask, ring_entries, flags, dropped, array.
        assert_eq!(
            (&w[10..=16]).to_vec(),
            vec![0, 4, 16, 24, 36, 32, 320],
            "sq_off must be Linux's measured words"
        );
        // cq_off: head, tail, ring_mask, ring_entries, overflow, cqes, flags.
        assert_eq!(
            (&w[20..=26]).to_vec(),
            vec![8, 12, 20, 28, 44, 64, 40],
            "cq_off must be Linux's measured words"
        );
        assert_eq!(PARAMS_BYTES, 30 * 4, "the params record is 30 words");
        // Nothing between the two offset blocks is set: the words the ABI
        // reserves are zeros, not whatever the caller's buffer held.
        assert!(w[3..=4].iter().all(|v| *v == 0));
        assert!(w[6..=9].iter().all(|v| *v == 0));
        assert_eq!(w[17], 0);
        assert!(w[18..=19].iter().all(|v| *v == 0));
        assert_eq!(w[27], 0);
        assert!(w[28..=29].iter().all(|v| *v == 0));
    }

    /// The ring's CQ half of the params must describe the CQEs the kernel
    /// posts: `cqes` plus `cq_entries` records has to end exactly where the SQ
    /// array starts, or a caller's array pointer would land inside a CQE slot.
    #[test]
    fn the_cq_area_is_exactly_as_long_as_its_records() {
        for (sq, cq) in [(1, 2), (8, 16), (1024, 2048)] {
            let w = params_words(sq, cq);
            let cqes = w[25] as usize;
            let array = w[16] as usize;
            assert_eq!(
                array,
                cqes + cq as usize * CQE_SIZE,
                "the SQ array follows the CQEs for sq={sq}"
            );
            // Every SQE index the array can hold names one 64-byte SQE.
            assert_eq!(array % 4, 0, "the array is a word array");
        }
    }

    /// Linux's rounding, measured on 7.2.11: 1→1, 2→2, 3→4, 5→8, 9→16, 1024→1024,
    /// and 0 / anything past the served ceiling is `EINVAL` (`None` here).
    #[test]
    fn the_ring_size_rounds_up_to_a_power_of_two() {
        assert_eq!(sq_entries_for(0), None);
        assert_eq!(sq_entries_for(1), Some(1));
        assert_eq!(sq_entries_for(2), Some(2));
        assert_eq!(sq_entries_for(3), Some(4));
        assert_eq!(sq_entries_for(5), Some(8));
        assert_eq!(sq_entries_for(9), Some(16));
        assert_eq!(sq_entries_for(1024), Some(1024));
        assert_eq!(sq_entries_for(1025), None);
    }

    /// The three magic regions are the ABI's: `IORING_OFF_SQ_RING` 0,
    /// `IORING_OFF_CQ_RING` 0x8000000 and `IORING_OFF_SQES` 0x10000000, each a
    /// whole number of pages, in the order the ABI names them.
    #[test]
    fn the_mapping_offsets_are_linux_s() {
        assert_eq!(IORING_OFF_SQ_RING, 0);
        assert_eq!(IORING_OFF_CQ_RING, 0x0800_0000);
        assert_eq!(IORING_OFF_SQES, 0x1000_0000);
        for offset in [IORING_OFF_SQ_RING, IORING_OFF_CQ_RING, IORING_OFF_SQES] {
            assert_eq!(offset % 4096, 0, "each magic offset is page-aligned");
        }
    }
}
