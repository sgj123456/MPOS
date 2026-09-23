use super::*;

/// Direct-mapped cache: one slot per possible block modulo this count.
///
/// A direct-mapped cache indexed by `block % CACHE_SLOTS` thrashes hard when a
/// working set is longer than the slot count: reading a file or walking inode
/// tables cycles every slot and evicts the inode-table block the very next
/// lookup needs, so each metadata access becomes a device read. 32 slots (128
/// KiB) was small enough that an ordinary `ls` re-read the table repeatedly;
/// 512 (2 MiB) keeps the hot metadata resident. Purely a size/latency trade -
/// correctness does not depend on it, so an undersized cache is slow, not wrong.
pub(crate) const CACHE_SLOTS: usize = 512;
pub(crate) const NO_BLOCK: u64 = u64::MAX;

pub(crate) struct Slot {
    block: u64,
    bytes: Vec<u8>,
}

/// Direct-mapped, write-through block cache: `block % CACHE_SLOTS` slots, a
/// miss loads from the device, every modification is pushed straight back.
pub(crate) struct Cache {
    slots: Vec<Slot>,
}

impl Cache {
    pub(crate) fn new(block_size: usize) -> Cache {
        Cache {
            slots: (0..CACHE_SLOTS)
                .map(|_| Slot {
                    block: NO_BLOCK,
                    bytes: vec![0; block_size],
                })
                .collect(),
        }
    }

    /// The page holding `block`, loaded from `dev` on a miss. The borrow keeps
    /// the slot resident, so callers must finish with the slice before the
    /// next cache access.
    pub(crate) fn slot(&mut self, dev: &dyn BlockDev, block: u64, block_size: usize) -> &mut [u8] {
        let slot = &mut self.slots[(block % CACHE_SLOTS as u64) as usize];
        if slot.block != block {
            dev.read_at(block * block_size as u64, &mut slot.bytes);
            slot.block = block;
        }
        &mut slot.bytes
    }
}
