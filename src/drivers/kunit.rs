//! The block layer's boot self-test: what a device read costs the rest of the
//! machine, in the machine's own units.
//!
//! §10.5's last row is "interrupt-driven completions": while a device works, the
//! driver holds its port inside `critical_section::with` and the crate spins on
//! the used ring, so **nothing else on the machine runs and no timer fires while
//! it does**. §7.3 records that as the reason an AHCI wait had to be a *count* of
//! register reads rather than a tick deadline, and §10.6 measured the same shape
//! for the serial's transmit ("~5 ms of machine-wide stall") before giving it a
//! ring the device interrupts.
//!
//! What the row was missing was a magnitude, and measuring it took three
//! corrections - each of them a fact about the instrument rather than about the
//! machine, and each written down here because they are what makes the numbers
//! below mean what they say:
//!
//! * **A masked window defers ticks rather than losing them.** The PIT is
//!   periodic, so however many periods a mask spans, **one** interrupt is
//!   delivered when it ends. The first draft read 4 MiB pieces (0.75 ms each) and
//!   measured a perfect `3/3`: no piece ever spanned a period. So the window is
//!   one call, as wide as a single allocation can be, and the count only moves
//!   past the *second* period a mask spans - `m` periods masked cost `m - 1`
//!   ticks.
//! * **The call is not all mask.** A virtio transport proves the buffer reachable
//!   before it starts the device (`super::virtio::device_reaches`, which walks
//!   the buffer's pages) and that walk is guest CPU work *outside* the mask. This
//!   case times the same walk over the same buffer and predicts from the
//!   remainder, which is what keeps the claim about the device instead of about
//!   the walk.
//! * **The two machines' clocks do not answer the same question.** The x86_64
//!   counter is a pure *interrupt* count (`TICKS.fetch_add` in `tick`), so a mask
//!   is visible in it; the aarch64 counter **catches up** (its `tick` adds the
//!   periods the timer's own counter says have passed) so a mask cannot be seen
//!   there at all - measured: `10/10` across a 40 ms mask. That is the machine's
//!   fact rather than a defect of either, and it is why the loss is asserted where
//!   it can be observed and *reported* where it cannot.
//!
//! **Why here and not in a `t-` program.** A userland window around a block read
//! measures mostly the work *around* the device call - the devfs block window's
//! allocation and zeroing, the copy out to the caller's buffer - and those parts
//! are preemptible, so a sibling task runs during them. On a 32 MiB raw read that
//! is 30-50 % of the window, the same order as the freeze itself, and a window
//! that has to beat its own confounds by a factor is not a pin. The kernel sees
//! the two quantities separately: wall time from the TSC, ticks from the handler.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::boot::BootInfo;
use crate::case_check;
use crate::kunit::{Case, Phase, Suite};
use crate::println;

/// The suite this module contributes to the boot's self-tests.
pub const SUITE: Suite = Suite {
    name: "block",
    phase: Phase::Arch,
    cases: &[Case {
        name: "what_a_read_costs_the_clock",
        run: what_a_read_costs_the_clock,
    }],
};

/// One read's span, and the largest a single allocation can be (`mm::pframe`'s
/// `MAX_ORDER`, 2^14 pages, 64 MiB): the mask has to span several tick periods
/// before the count can lose any, so the span is as wide as the allocator allows.
/// A heap allocation this size goes out as one contiguous frame block, which is
/// what a driver may be handed whole ([`crate::blk::direct_map_run`]) - the boot
/// suite for the frame allocator already writes a block this size end to end.
const SPAN: usize = 64 * 1024 * 1024;

/// The control window's length: three ticks, so a swallowed tick is most of the
/// window rather than its edge.
const WINDOW_NS: u64 = 3 * (1_000_000_000 / crate::hal::TICK_HZ as u64);

/// What one reachability walk over [`SPAN`] may cost. Measured on the way here at
/// 701 µs on a TCG aarch64 (≈0.04 µs a page, 16 KiB of page tables walked per
/// MiB) and tens of microseconds under KVM, so this is a hundredfold margin: what
/// it refuses is a walk that has stopped being per *page*, which is the shape
/// that would make the wide path's own precondition expensive.
const WALK_BUDGET_NS: u64 = 8_000_000;

/// One measured window: the wall time it covered, and the ticks the clock
/// delivered inside it against the ticks that wall time allowed.
struct Elapsed {
    ns: u64,
    delivered: i64,
    expected: i64,
}

impl Elapsed {
    /// Measure from `ns0`/`ticks0`, both read at the window's *start*, so the
    /// window is whatever happened between the two snapshots - and nothing else.
    fn since(ns0: u64, ticks0: u64) -> Elapsed {
        let ns = crate::hal::since_boot_nanos() - ns0;
        let delivered = (crate::hal::ticks() - ticks0) as i64;
        let expected = (ns / (1_000_000_000 / crate::hal::TICK_HZ as u64)) as i64;
        Elapsed {
            ns,
            delivered,
            expected,
        }
    }

    /// How many ticks the window's own wall time allowed and the clock did not
    /// deliver. One is the edge - a tick can be pending when the window opens and
    /// delivered as it closes - and most of a window is the machine being held.
    fn lost(&self) -> i64 {
        self.expected - self.delivered
    }
}

/// Whether this machine's clock can see a masked window at all.
///
/// The two machines differ, and it is their *timers* that differ:
/// `arch/x86_64/timer.rs`'s `tick` is `TICKS.fetch_add(1)` - one count per
/// delivered interrupt, so the periods a mask spans are lost from it - while
/// `arch/aarch64/timer.rs`'s adds the periods its own counter says have passed,
/// so it **catches up** and the same mask leaves no trace. Measured: `10/10`
/// ticks across a 40 ms mask on the aarch64 arm.
///
/// The claim below is therefore one the first machine carries and the second
/// reports: an assertion that can never fail on a machine is not a pin, and
/// pretending the two are the same would hide which machine answered.
#[cfg(target_arch = "x86_64")]
fn clock_sees_the_mask() -> bool {
    true
}

/// See the x86_64 body: this machine's `tick` catches up, so a mask is invisible
/// here and the case reports its figures instead of asserting on them.
#[cfg(not(target_arch = "x86_64"))]
fn clock_sees_the_mask() -> bool {
    false
}

/// What a device read costs the machine that is waiting for it.
fn what_a_read_costs_the_clock(_info: &BootInfo) -> Result<(), String> {
    let disks = crate::blk::disks();
    if disks.is_empty() {
        return Err(String::from("no block device is registered"));
    }

    // ---- the control: the machine running freely for one window ----
    //
    // Nothing is masked here, so every tick the wall time allows is delivered
    // *when* it is due. This is the instrument's own soundness rather than the
    // machine's answer, and it is what makes a shortfall below the read's doing.
    let c0 = crate::hal::since_boot_nanos();
    let ct0 = crate::hal::ticks();
    while crate::hal::since_boot_nanos() - c0 < WINDOW_NS {
        core::hint::spin_loop();
    }
    let control = Elapsed::since(c0, ct0);
    println!(
        "block: {} ms of spinning delivered {}/{} ticks",
        control.ns / 1_000_000,
        control.delivered,
        control.expected
    );
    case_check!(
        control.lost() <= 1,
        "a {} ms spin lost {} of its {} ticks: the clock cannot be this case's instrument",
        control.ns / 1_000_000,
        control.lost(),
        control.expected
    );

    // A buffer once for every disk measured: the allocation and its zeroing are
    // work outside every window by construction.
    let mut buf = vec![0u8; SPAN];

    // The reachability walk a virtio transport does before it hands the buffer
    // over, timed on the same buffer so the read's window can be corrected for
    // it: the walk is work *before* the mask, so its ticks arrive on time.
    let w0 = crate::hal::since_boot_nanos();
    let reachable = super::virtio::device_reaches(&buf);
    let walk_ns = crate::hal::since_boot_nanos() - w0;
    let tick_ns = 1_000_000_000 / crate::hal::TICK_HZ as u64;
    println!(
        "block: the reachability walk over a {} MiB buffer takes {} us (reachable {reachable})",
        SPAN / (1024 * 1024),
        walk_ns / 1_000
    );
    case_check!(
        walk_ns <= WALK_BUDGET_NS,
        "the reachability walk over {SPAN} bytes took {} us, over the {WALK_BUDGET_NS} ns \
         budget: the wide path's precondition has stopped being a per-page check",
        walk_ns / 1_000
    );

    // ---- every disk the machine has, one call each ----
    //
    // Not just the boot disk: the two transports this kernel drives share the
    // trait and the wait, so a machine with both measures both, and the answers
    // are collected rather than returned on the first - one disk's figure is not
    // a reason to leave the other unmeasured.
    let mut failures: Vec<String> = Vec::new();
    for disk in &disks {
        let name = &disk.name;
        let cap = disk.dev.capacity_sectors() * 512;
        if cap < SPAN as u64 {
            println!("block: {name} holds {cap} bytes, under the {SPAN}-byte span: not measured");
            continue;
        }
        let t0 = crate::hal::since_boot_nanos();
        let k0 = crate::hal::ticks();
        disk.dev.read_at(0, &mut buf);
        let read = Elapsed::since(t0, k0);

        // The mask's own wall time, and what the clock should lose for it. Zero
        // for a device that answers inside a period - which is every call this
        // machine's filesystem makes (a run is 64 KiB), and why this row has no
        // number behind it on this machine.
        let mask_ns = read.ns.saturating_sub(walk_ns);
        let predicted = (mask_ns / tick_ns).saturating_sub(1) as i64;
        println!(
            "block: {name}: one {} MiB read over {} ms ({} ms of it masked) delivered {}/{} \
             ticks; a mask that long costs {predicted}",
            SPAN / (1024 * 1024),
            read.ns / 1_000_000,
            mask_ns / 1_000_000,
            read.delivered,
            read.expected
        );

        // The row's pin, flipped with the completion interrupt (see
        // [`clock_sees_the_mask`]): a read no longer holds the machine for the
        // device's own latency — the transfer submits non-blocking, waits
        // unmasked for the completion line, and retires — so the loss is at
        // most the submit/retire window's edge, the same bound the control
        // above takes. The counterfactual is exact and lives one flag away:
        // with the completion line unarmed, the transfer spins masked for the
        // whole device call and `lost` climbs to `predicted`, firing this
        // claim. The history this replaced — a case asserting the mask *costs*
        // what the device latency says — is §10.5's own table, and the
        // negative-loss guard's story is in the git log of this file.
        if clock_sees_the_mask() && read.lost() > 1 {
            failures.push(format!(
                "{name}: a {} ms device read ({} ms of it masked) held the machine \
                 for {} ticks - completions are interrupt-driven, and the mask \
                 should be the submit/retire edge alone",
                read.ns / 1_000_000,
                mask_ns / 1_000_000,
                read.lost(),
            ));
        }
    }

    match failures.first() {
        Some(why) => Err(why.clone()),
        None => Ok(()),
    }
}
