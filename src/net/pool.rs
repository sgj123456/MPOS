//! The frame counters and buffer pool behind the smoltcp device seam.
//!
//! §10.4's last item is a pool of frame buffers that hands buffers back rather
//! than freeing them. Its own text also names the prerequisite: *"an instrument
//! first - a datagram-per-second count with and without the pool - so that it
//! is landed with a number rather than with a diff."* The counters came first
//! and are this file's other half: every increment happens at exactly one point
//! (a frame entering `NicDevice::receive`, an outgoing frame being built by
//! `FrameTx::consume`), and the pool shares the domain - counting reuse and
//! falling back to fresh allocations is the same few lines as counting frames,
//! and one home keeps the two from disagreeing. The door both read through is
//! Linux's own: `/proc/net/dev`, whose whole purpose is per-interface rx/tx
//! packet and byte counts, rendered by `vfs::procfs` from [`net_dev_stats`]
//! here.
//!
//! The counters live *beside the seam* rather than inside `mod.rs` because
//! they are the seam's own bookkeeping: every increment happens at exactly one
//! point (a frame entering `NicDevice::receive`, an outgoing frame being built
//! by `FrameTx::consume`), and the pool shares the domain - counting reuse and falling back to fresh allocations is the same few lines
//! as counting frames, and one home keeps the two from disagreeing.
//!
//! # What is counted, and what is not
//!
//! A **packet** here is one frame crossing the eth0 seam - which is why the
//! UDP claim `t-netstats` pins can be exact: one datagram within the MTU is
//! one frame, smoltcp builds it, the driver sends it. Two things deliberately
//! stay out of these numbers:
//!
//! * **loopback.** `lo`'s traffic runs on smoltcp's own internal device
//!   (`mod.rs`'s loopback note) and never reaches this seam, so its row prints
//!   zeros. That is honest, not lazy: the seam cannot count bytes it never
//!   sees, and a kernel that claimed lo numbers would be inventing them.
//! * **the driver's inner bounce buffer.** virtio-drivers allocates its own
//!   `TxBuffer` per send below [`NetDevice::transmit`]; a frame counted here is
//!   one *seam* allocation, whatever the layer under it also does. When the
//!   pool lands, the before/after figure is attributed to the seam's two Vecs
//!   and says so.
//!
//! Relaxed atomics throughout: the counts are read by a measurement, never used
//! to decide anything, and interrupt-disable already serialises every writer on
//! this uniprocessor machine.
//!
//! # The pool's shape, and its bound
//!
//! Sixteen slots per direction, each `POOL_BYTES = 2048` wide - the same size
//! the driver's own receive ring pre-allocates (`drivers/virtio/net.rs`'s
//! `BUFFER_BYTES`), so a slot is exactly one of the buffers the machine already
//! proves it wants. Worst case **64 KiB resident** (32 × 2 KiB), allocated at
//! `init`, which is §10's rule-4 bound stated before the allocations exist: no
//! user steers the count or the width, and a frame wider than the slot - an
//! MTU raised past 2034 by `ip link set ... mtu` - falls back to a fresh
//! allocation counted as a miss rather than growing the pool under anyone's
//! feet.
//!
//! The lock is the same `critical_section::Mutex<RefCell<..>>` the seam itself
//! runs under, not a second discipline: every take and put happens inside the
//! NET critical section anyway, and a pooled Vec must never be reachable while
//! another holder can still touch it.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// One interface's counters, as `/proc/net/dev` renders them.
///
/// Lives here, in the crate that owns the fact, because `vfs::procfs` is
/// host-compiled and must name this type even where no stack is; the sibling
/// pattern is `procfs::NetRow`, which travels the other way for the same
/// reason (one shape, both doors).
pub struct NetDevStats {
    /// The interface's name, as `/proc/net/dev`'s first column prints it.
    pub name: &'static str,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    /// The link's current MTU, which is the file's `mtu` column.
    pub mtu: usize,
    /// The pool's `(hits, misses)` counters — this row's trailing `pool:`
    /// field, whose story is at [`pool_counters`].
    pub pool_hits: u64,
    pub pool_misses: u64,
}

/// The eth0 counters: four totals plus the pool's two, all relaxed.
static SEAM: SeamCounters = SeamCounters::new();

struct SeamCounters {
    rx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    /// Frames the pool served from a slot that already existed (the number
    /// the pool's *worth* is read off).
    pool_hits: AtomicU64,
    /// Frames the pool could not serve - oversize after a `set_mtu`, or an
    /// empty pool - and that were built from a fresh allocation instead. An
    /// honest denominator beside `pool_hits`.
    pool_misses: AtomicU64,
}

impl SeamCounters {
    const fn new() -> SeamCounters {
        SeamCounters {
            rx_bytes: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            pool_hits: AtomicU64::new(0),
            pool_misses: AtomicU64::new(0),
        }
    }
}

/// Record one received frame crossing the seam.
pub(crate) fn record_rx(len: usize) {
    SEAM.rx_packets.fetch_add(1, Ordering::Relaxed);
    SEAM.rx_bytes.fetch_add(len as u64, Ordering::Relaxed);
}

/// Record one outgoing frame built at the seam.
pub(crate) fn record_tx(len: usize) {
    SEAM.tx_packets.fetch_add(1, Ordering::Relaxed);
    SEAM.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
}

/// Record that the pool served a frame from an existing slot.
pub(crate) fn record_pool_hit() {
    SEAM.pool_hits.fetch_add(1, Ordering::Relaxed);
}

/// Record that a frame fell back to a fresh allocation.
pub(crate) fn record_pool_miss() {
    SEAM.pool_misses.fetch_add(1, Ordering::Relaxed);
}

/// The seam's counters, in the shape `/proc/net/dev` renders.
///
/// `mtu` arrives from the caller - the live value lives in the stack's
/// `NicDevice`, and this module deliberately holds no pointer into it: the
/// counters are statics, and the one dynamic field stays with the thing that
/// owns it.
pub(crate) fn net_dev_stats(mtu: usize) -> Vec<NetDevStats> {
    use alloc::vec;
    let (hits, misses) = pool_counters();
    vec![
        NetDevStats {
            name: "eth0",
            rx_bytes: SEAM.rx_bytes.load(Ordering::Relaxed),
            rx_packets: SEAM.rx_packets.load(Ordering::Relaxed),
            tx_bytes: SEAM.tx_bytes.load(Ordering::Relaxed),
            tx_packets: SEAM.tx_packets.load(Ordering::Relaxed),
            mtu,
            pool_hits: hits,
            pool_misses: misses,
        },
        // Loopback's row exists and reads zeros, for the reason in this file's
        // header: its frames never cross this seam. A missing `lo` row would
        // make a reader that walks the file by name fail; a zeroed one tells it
        // the truth about what this kernel counts.
        NetDevStats {
            name: "lo",
            rx_bytes: 0,
            rx_packets: 0,
            tx_bytes: 0,
            tx_packets: 0,
            mtu: 65536,
            pool_hits: 0,
            pool_misses: 0,
        },
    ]
}

/// The pool's two counters. Nothing in the shipping kernel reads them - no
/// policy branches on them - and that is said rather than hidden: they are
/// §10.4's instrument state, filled by the takes below and read through
/// `/proc/net/dev`'s trailing `pool=<hits>:<misses>` word on the eth0 row - a
/// named divergence from Linux's column count, appended past the last standard
/// column so a reader that parses by position sees every Linux field where
/// Linux puts it, and only the one extra word is this kernel's own
/// (`t-netstats` reads it; the procfs renderer's test pins the shape).
pub fn pool_counters() -> (u64, u64) {
    (
        SEAM.pool_hits.load(Ordering::Relaxed),
        SEAM.pool_misses.load(Ordering::Relaxed),
    )
}

/// Whether a frame of `len` bytes would be served from a pool slot.
///
/// The policy is pinned on the machine by `t-netstats`'s warm-up claim: a
/// full-MTU frame must be a hit, so an off-by-one (`<` instead of `<=`, or a
/// slot computed without the Ethernet header) shows up as `pool=0:<N>` - every
/// frame a silent miss. This file is reached only through the machine-gated
/// `crate::net` (the trap at `net/socket.rs`'s tail note - a `cfg(test)` module
/// here would never run), so the pin lives in the program that boots rather
/// than a host test.
///
/// `slot_bytes` is the widest buffer any slot holds; anything larger - an MTU
/// raised past the boot's by `ip link set ... mtu` - falls back to a fresh
/// allocation, counted as a miss. That is the honest shape: the pool serves
/// what its slots can hold and says plainly when it cannot.
pub(crate) const fn fits_pool(len: usize, slot_bytes: usize) -> bool {
    len <= slot_bytes
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// One slot's width: the driver ring's own buffer size, so the seam's pooled
/// buffers are byte-for-byte the ones this machine already proves it wants
/// (`drivers/virtio/net.rs`'s `BUFFER_BYTES`).
const POOL_BYTES: usize = 2048;

/// Slots per direction. Sixteen covers one full poll round with headroom for
/// the frames still held between polls; the bound this implies -
/// 2 × 16 × [`POOL_BYTES`] = **64 KiB resident** - is stated in this file's
/// header before the allocations exist (§10's rule-4 worst-case discipline):
/// no user steers the count or the width.
const POOL_SLOTS: usize = 16;

static POOL: critical_section::Mutex<core::cell::RefCell<FramePool>> =
    critical_section::Mutex::new(core::cell::RefCell::new(FramePool::EMPTY));

/// Two fixed-size free lists of owned frame buffers, TX and RX apart.
///
/// The directions are separate because their lifetimes differ: a TX buffer is
/// returned the moment `transmit` copies it into the driver's own buffer, while
/// an RX buffer can sit in `NicDevice::pending` until a later poll consumes or
/// drops it. One shared list would let whichever direction drains first starve
/// the other for no benefit - the buffers are identical width.
struct FramePool {
    tx_free: Vec<Vec<u8>>,
    rx_free: Vec<Vec<u8>>,
}

impl FramePool {
    /// The static initialiser: empty lists, filled by [`init`] once the
    /// allocator is up. A `const` body cannot allocate, and the pool must not
    /// build slots lazily under the lock at frame time either - the whole point
    /// is that the steady path takes and puts, never constructs.
    const EMPTY: FramePool = FramePool {
        tx_free: Vec::new(),
        rx_free: Vec::new(),
    };

    /// Serve one frame of `len` bytes from the given direction's free list.
    ///
    /// `None` means *fall back honestly*: an oversize frame (past
    /// [`POOL_BYTES`]) or an exhausted list, each counted as a miss beside the
    /// hits so `/proc/net/dev`'s trailing word tells which. A taken slot comes
    /// back zero-length but capacity kept - `resize` then writes exactly `len`
    /// bytes, so the caller sees a buffer sized for its frame without paying
    /// for a new allocation.
    fn take(list: &mut Vec<Vec<u8>>, len: usize) -> Option<Vec<u8>> {
        if !fits_pool(len, POOL_BYTES) || list.is_empty() {
            record_pool_miss();
            return None;
        }
        record_pool_hit();
        let mut buf = list.pop()?;
        buf.resize(len, 0);
        Some(buf)
    }

    /// Hand one buffer back. Refuses a foreign or oversized buffer rather than
    /// letting the put side grow the pool past its declared bound: a buffer
    /// whose capacity cannot serve a slot came from a fallback allocation and
    /// belongs to the heap's normal life, and the length check keeps the list
    /// at [`POOL_SLOTS`] whatever path a buffer travelled.
    fn put(list: &mut Vec<Vec<u8>>, mut buf: Vec<u8>) {
        if buf.capacity() < POOL_BYTES || buf.len() > POOL_BYTES || list.len() >= POOL_SLOTS {
            return;
        }
        // Keep the capacity, drop the contents: the next taker resizes anyway,
        // and leaving a previous frame's bytes reachable inside a static would
        // be a leak of a different kind.
        buf.clear();
        list.push(buf);
    }
}

/// Fill one direction's free list with `POOL_SLOTS` pre-built buffers.
///
/// A loop, not `vec![Vec::with_capacity(..); n]`: the macro *clones* its
/// element, and an empty Vec's clone drops the allocation - so the repeat form
/// would leave thirty-two zero-capacity slots that every take then refuses.
/// The capacity is the point of this function.
fn fill_slots() -> Vec<Vec<u8>> {
    let mut slots = Vec::with_capacity(POOL_SLOTS);
    for _ in 0..POOL_SLOTS {
        slots.push(Vec::with_capacity(POOL_BYTES));
    }
    slots
}

/// Fill the pool. Called once from `net::init`, before the stack exists, so no
/// frame can race the fill and every later take finds slots.
pub(crate) fn init() {
    critical_section::with(|cs| {
        let mut pool = POOL.borrow_ref_mut(cs);
        pool.tx_free = fill_slots();
        pool.rx_free = fill_slots();
    });
}

/// Take an outgoing frame buffer: pooled when a slot can serve it, a fresh
/// zeroed `Vec` otherwise. The caller always gets a buffer of exactly `len`;
/// only its provenance differs, and the counters say which.
pub(crate) fn take_tx(len: usize) -> Vec<u8> {
    critical_section::with(|cs| FramePool::take(&mut POOL.borrow_ref_mut(cs).tx_free, len))
        .unwrap_or_else(|| vec![0u8; len])
}

/// Take an incoming frame buffer to copy the driver's frame into. Same
/// contract as [`take_tx`], separate list for the reason on [`FramePool`].
pub(crate) fn take_rx(len: usize) -> Vec<u8> {
    critical_section::with(|cs| FramePool::take(&mut POOL.borrow_ref_mut(cs).rx_free, len))
        .unwrap_or_else(|| vec![0u8; len])
}

/// Return an outgoing buffer.
pub(crate) fn put_tx(buf: Vec<u8>) {
    critical_section::with(|cs| FramePool::put(&mut POOL.borrow_ref_mut(cs).tx_free, buf));
}

/// Return an incoming buffer.
pub(crate) fn put_rx(buf: Vec<u8>) {
    critical_section::with(|cs| FramePool::put(&mut POOL.borrow_ref_mut(cs).rx_free, buf));
}

/// One arrived frame, carrying its pool slot.
///
/// The only place a pooled buffer needs a guard, and the reason it is enough:
/// the guard is the seam's pending slot *and* the RX token's field, so whichever
/// way a frame's life ends — the stack read the bytes in `FrameRx::consume`, or
/// smoltcp took the token pair and dropped it unread — exactly one `Drop`
/// returns the slot. Every other recycle point is an explicit put at the site
/// that owns the buffer's last use, and those sites carry no guard, because two
/// owners returning one buffer is how a pool double-feeds itself.
pub(crate) struct PendingFrame {
    buf: Vec<u8>,
}

impl PendingFrame {
    pub(crate) fn new(buf: Vec<u8>) -> PendingFrame {
        PendingFrame { buf }
    }

    /// The frame's bytes, for counting and delivery.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for PendingFrame {
    fn drop(&mut self) {
        // The RX direction's one recycle point. Taking the buffer out first
        // makes the drop idempotent by construction: a second drop hands over an
        // empty Vec, and `put_rx` refuses one whose capacity cannot serve a slot.
        put_rx(core::mem::take(&mut self.buf));
    }
}
