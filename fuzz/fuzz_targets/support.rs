//! Shared harness for the fuzz targets.
//!
//! Two things live here. An in-memory [`BlockDev`] so the disk-reading parsers
//! - the partition table and ext2/ext4 - can be driven from a byte string, and
//! the clamps that keep a fuzzed *length* from being reported as a kernel bug
//! when it is only the fuzzing host running out of RAM. The kernel's own
//! allocation limits are a different question from "can a `Vec` on a laptop
//! grow to 4 GiB", and a fuzzer that conflates them spends its time finding
//! the second one.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use mpos::blk::BlockDev;

/// Bytes a logical sector, matching the unit `BlockDev` speaks.
pub const SECTOR: u64 = 512;

/// The largest disk a target will build. A fuzzed image's *declared* size is
/// whatever the bytes say and is the kernel's problem to validate; this caps
/// only how much real memory backs the disk it is served from.
pub const MAX_DISK: usize = 1 << 20;

/// A `Vec<u8>` behind a lock, seen through the kernel's block-device contract.
///
/// The contract is that offsets and lengths are whole sectors and the buffer
/// is at most a page, so reads are exact slices; past the end of the device a
/// real disk answers nothing, and this answers zeros, which is what makes a
/// truncated image behave like a short one rather than like an out-of-bounds
/// indexing panic in the harness.
pub struct MemDev {
    bytes: Mutex<Vec<u8>>,
}

impl MemDev {
    /// The device a fuzzed byte string should be served from, rounded down to
    /// whole sectors and capped at [`MAX_DISK`].
    pub fn from_bytes(data: &[u8]) -> Arc<MemDev> {
        let len = data.len().min(MAX_DISK) / SECTOR as usize * SECTOR as usize;
        Arc::new(MemDev {
            bytes: Mutex::new(data[..len].to_vec()),
        })
    }

    /// `blocks` 1024-byte blocks of zeros, for a target that wants to write a
    /// header of its own into an otherwise empty disk.
    pub fn zeroed(blocks: usize) -> Arc<MemDev> {
        Arc::new(MemDev {
            bytes: Mutex::new(vec![0u8; blocks * 1024]),
        })
    }

    /// `disk` wrapped as a `dyn BlockDev`, which is the shape every mount takes.
    pub fn as_dev(disk: Arc<MemDev>) -> Arc<dyn BlockDev> {
        disk
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
        f(&mut self.bytes.lock().unwrap())
    }
}

impl BlockDev for MemDev {
    fn capacity_sectors(&self) -> u64 {
        self.with(|b| b.len() as u64 / SECTOR)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        self.with(|b| {
            // A read that starts past the end of the device answers zeros, the
            // way a short read on a real disk leaves the buffer untouched.
            let at = (offset as usize).min(b.len());
            let end = at + buf.len();
            if end <= b.len() {
                buf.copy_from_slice(&b[at..end]);
            } else {
                let have = b.len() - at;
                buf[..have].copy_from_slice(&b[at..at + have]);
                buf[have..].fill(0);
            }
        });
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        self.with(|b| {
            let at = offset as usize;
            if at + buf.len() <= b.len() {
                b[at..at + buf.len()].copy_from_slice(buf);
            }
        });
    }
}

/// Read a little-endian `u32` out of a fuzzed buffer, zero past the end.
pub fn u32_at(data: &[u8], at: usize) -> u32 {
    let mut out = [0u8; 4];
    let n = out.len().min(data.len().saturating_sub(at));
    if n > 0 {
        out[..n].copy_from_slice(&data[at..at + n]);
    }
    u32::from_le_bytes(out)
}

/// Read a little-endian `u16` out of a fuzzed buffer, zero past the end.
pub fn u16_at(data: &[u8], at: usize) -> u16 {
    let mut out = [0u8; 2];
    let n = out.len().min(data.len().saturating_sub(at));
    if n > 0 {
        out[..n].copy_from_slice(&data[at..at + n]);
    }
    u16::from_le_bytes(out)
}

/// The first `max` bytes of `data`, treated as UTF-8 with the invalid runs
/// replaced - a name the kernel would have to cope with either way.
pub fn text(data: &[u8], max: usize) -> String {
    String::from_utf8_lossy(&data[..data.len().min(max)]).into_owned()
}
