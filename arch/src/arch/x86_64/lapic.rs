//! The local APIC: acknowledging message-signalled interrupts, and the
//! one-shot timer a sub-tick sleep is woken by.
//!
//! MSI and MSI-X interrupts bypass the 8259 entirely - the device writes to
//! the APIC's address window and the APIC raises the vector on this CPU. The
//! PIC's EOI port does nothing for them; the APIC keeps the vector in its
//! In-Service register until software writes the EOI register, and a vector
//! left in service blocks every interrupt at or below its priority from being
//! delivered.
//!
//! The APIC's page is device memory at a physical address the MSR names, so it
//! needs a mapping before anything can be written to it - [`init`] makes that
//! mapping, once, and [`eoi`] is then a single volatile store.
//!
//! ## Why the timer lives here
//!
//! The scheduler's clock is the PIT's 250 Hz rate, and the PIT is the only
//! wake-up source a machine had: a `nanosleep(1 ms)` rounded up to the next
//! 4 ms tick. The LAPIC has a count-down timer of its own, on a vector of its
//! own, and [`arm_one_shot`] programs it to raise that vector at one named
//! instant - so a wait that lands between two ticks ends when it is due. The
//! PIT stays armed as the periodic floor: if a one-shot is ever lost, the tick
//! still advances the clock and the sleeper wakes a tick late rather than never
//! (`docs/notes/` §10.7).
//!
//! Its rate is not a constant this kernel may write down. QEMU and real
//! hardware differ, TCG and KVM differ, and the counts below the divide
//! register are in an internal unit no specification ties to nanoseconds - so
//! [`calibrate`] measures it against the TSC, which the clock already measures
//! against the PIT. A machine whose LAPIC timer does not tick is answered
//! `None`, and the scheduler then keeps sleeping in ticks rather than trusting a
//! timer that never fires.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The `IA32_APIC_BASE` model-specific register.
const MSR_IA32_APIC_BASE: u32 = 0x1b;

/// Where the EOI register lives inside the APIC's page.
const APIC_EOI_OFFSET: usize = 0x0b0;

/// The LVT timer entry: vector in the low byte, mode in bits 17-18, mask in 16.
const APIC_LVT_TIMER: usize = 0x320;

/// The timer's count-down register and its current value.
const APIC_TIMER_INITIAL: usize = 0x380;
const APIC_TIMER_CURRENT: usize = 0x390;

/// The divide configuration: `0b1011` is divide-by-1, the finest setting.
const APIC_TIMER_DIVIDE: usize = 0x3e0;
const DIVIDE_BY_ONE: u32 = 0b1011;

/// The vector the one-shot raises: clear of the PIC's 32..47, of the
/// virtio-net MSI-X vector (0x41) and of every architecturally reserved slot.
pub const TIMER_VECTOR: u8 = 0x40;

/// The virtual address of the EOI register, once [`init`] has mapped the page.
/// Zero until then, which makes `eoi` a no-op rather than a fault.
static EOI: AtomicUsize = AtomicUsize::new(0);

/// The mapped page's own base, for the registers other than EOI.
static BASE: AtomicUsize = AtomicUsize::new(0);

/// The spurious-interrupt vector: the standard `0xFF`, and the one the LAPIC is
/// told to raise for a delivery it cannot make. It needs a handler that does
/// nothing, or enabling the APIC would turn a harmless race into a fault.
pub const SPURIOUS_VECTOR: u8 = 0xff;

/// The APIC's software-enable register: bit 8 enables delivery, the low byte is
/// the spurious vector.
const APIC_SPURIOUS: usize = 0x0f0;

/// The task-priority register. Zero accepts every priority; a non-zero value
/// would silently drop our own vector.
const APIC_TPR: usize = 0x080;

/// Timer counts per millisecond, once [`calibrate`] has measured it, and
/// [`NOT_TICKING`] when three measurements say this LAPIC's timer does not count.
///
/// Counts per *millisecond* rather than per nanosecond because the raw timer is
/// around a hundred million counts a second: counts-per-nanosecond truncates to
/// zero, and a rate kept as an integer fraction of that is a timer armed for the
/// wrong instant.
static COUNTS_PER_MS: AtomicU64 = AtomicU64::new(0);
const NOT_TICKING: u64 = u64::MAX;

/// How many measurements have failed. One failure is not evidence - a boot
/// under load can miss the window - but three are, and after the third the
/// timer is left alone rather than re-spinning every turn.
static CALIBRATION_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Map the local APIC's register page and remember where its registers landed.
///
/// Idempotent: the first caller maps and the rest see the stored base. The
/// mapping needs the kernel's page tables and an MMIO window, neither of which
/// the earliest boot guarantees - which is why the timer path calls this lazily
/// at its first arming rather than from the clock's own `init`.
pub fn init() {
    if BASE.load(Ordering::Relaxed) != 0 {
        return;
    }
    let phys = unsafe { x86_64::registers::model_specific::Msr::new(MSR_IA32_APIC_BASE).read() }
        as usize
        & !0xfff;
    let base = crate::hal::map_mmio_window(phys, 0x1000).expect("out of room for the local APIC");
    BASE.store(base, Ordering::Relaxed);
    EOI.store(base + APIC_EOI_OFFSET, Ordering::Relaxed);

    // Turn delivery on before anything is programmed into the timer. Bit 11 of
    // the base MSR is the APIC's global enable and bit 8 of the spurious-vector
    // register is its software enable; a timer armed without them counts down
    // and never raises its vector, which reads exactly like a timer that does not
    // tick - and it did, intermittently, before this was here (one boot's fifty
    // 1 ms sleeps all landed on clock ticks with every counter in this file
    // looking right, `docs/notes/` §10.7).
    let mut msr = x86_64::registers::model_specific::Msr::new(MSR_IA32_APIC_BASE);
    unsafe { msr.write(msr.read() | (1 << 11)) };
    write(APIC_TPR, 0);
    write(APIC_SPURIOUS, (1 << 8) | SPURIOUS_VECTOR as u32);
}

/// Signal the end of the interrupt currently in service.
///
/// A plain zero write to the register [`init`] mapped. Called from interrupt
/// handlers: nothing here locks, allocates, or can fault.
pub fn eoi() {
    let at = EOI.load(Ordering::Relaxed);
    if at != 0 {
        unsafe { (at as *mut u32).write_volatile(0) };
    }
}

fn write(offset: usize, value: u32) {
    let base = BASE.load(Ordering::Relaxed);
    if base != 0 {
        unsafe { ((base + offset) as *mut u32).write_volatile(value) };
    }
}

fn read(offset: usize) -> u32 {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        0
    } else {
        unsafe { ((base + offset) as *const u32).read_volatile() }
    }
}

/// Measure this LAPIC timer against the clock the deadlines are written in.
///
/// The timer is started from its maximum with its interrupt masked, and both its
/// count and `since_boot_nanos` are sampled twice. The two deltas give counts per
/// nanosecond directly - in the very unit a deadline is named in, so no third
/// rate (the TSC's, the PIT's) can disagree with the timer at the moment it
/// matters. The earlier shape measured counts per TSC cycle and scaled by the
/// clock's own TSC rate; two calibrations that each drift is one more way to arm
/// the wrong instant than this needs.
///
/// The loops are bounded and both limits say "not counting": 20 ms of clock, or
/// 200 000 counts. A boot where the APIC is not delivering has already been seen
/// (§10.7) and is answered `None` rather than trusted.
fn calibrate() -> Option<u64> {
    // One-shot, masked: the calibration must not raise the vector.
    write(APIC_LVT_TIMER, TIMER_VECTOR as u32 | (1 << 16));
    write(APIC_TIMER_DIVIDE, DIVIDE_BY_ONE);
    write(APIC_TIMER_INITIAL, u32::MAX);

    let first_count = read(APIC_TIMER_CURRENT) as u64;
    let first_ns = crate::hal::since_boot_nanos();
    let (elapsed_counts, elapsed_ns) = loop {
        let now_count = read(APIC_TIMER_CURRENT) as u64;
        let now_ns = crate::hal::since_boot_nanos();
        let counts = first_count.saturating_sub(now_count);
        let ns = now_ns.wrapping_sub(first_ns);
        if counts >= 200_000 || ns >= 20_000_000 {
            break (counts, ns);
        }
    };
    // Leave the timer stopped whatever the answer; an arming re-programs it.
    write(APIC_TIMER_INITIAL, 0);
    if elapsed_counts < 4_000 || elapsed_ns == 0 {
        return None;
    }
    let per_ms = elapsed_counts.saturating_mul(1_000_000) / elapsed_ns;
    // A timer outside 1 MHz..4 GHz is not one this measurement can trust, and
    // arming from a garbage rate is worse than falling back to the tick.
    (1_000..=4_000_000).contains(&per_ms).then_some(per_ms)
}

/// Arm the one-shot at `deadline_ns`, or stop it when `None`.
///
/// Called from the scheduler's turn with interrupts off. The first call is also
/// where the page is mapped and the rate measured. A measurement that fails is
/// retried on later turns - a single miss is not proof - and after three the
/// timer is remembered as [`NOT_TICKING`], so a machine without one pays for a
/// bounded number of spins and then sleeps in ticks.
pub fn arm_one_shot(deadline_ns: Option<u64>) {
    init();
    let Some(deadline_ns) = deadline_ns else {
        write(APIC_TIMER_INITIAL, 0);
        return;
    };
    let mut per_ms = COUNTS_PER_MS.load(Ordering::Relaxed);
    if per_ms == 0 {
        per_ms = match calibrate() {
            Some(per_ms) => {
                COUNTS_PER_MS.store(per_ms, Ordering::Relaxed);
                per_ms
            }
            None => {
                let failures = CALIBRATION_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= 3 {
                    COUNTS_PER_MS.store(NOT_TICKING, Ordering::Relaxed);
                }
                return;
            }
        };
    }
    if per_ms == NOT_TICKING {
        return;
    }

    let now = crate::hal::since_boot_nanos();
    // A deadline already reached gets the shortest interval the timer accepts:
    // the next scheduling turn would wake the task anyway, and a zero initial
    // count would stop the timer instead of raising it.
    let wait_ns = deadline_ns.saturating_sub(now).max(1);
    let cycles = wait_ns
        .saturating_mul(per_ms)
        .wrapping_div(1_000_000)
        .min(u32::MAX as u64) as u32;
    write(APIC_TIMER_DIVIDE, DIVIDE_BY_ONE);
    // One-shot (bit 17 clear), unmasked: the vector is the scheduler's.
    write(APIC_LVT_TIMER, TIMER_VECTOR as u32);
    write(APIC_TIMER_INITIAL, cycles.max(1));
}
