//! The local APIC's one job here: acknowledging message-signalled interrupts.
//!
//! MSI and MSI-X interrupts bypass the 8259 entirely - the device writes to
//! the APIC's address window and the APIC raises the vector on this CPU. The
//! PIC's EOI port does nothing for them; the APIC keeps the vector in its
//! In-Service register until software writes the EOI register, and a vector
//! left in service blocks every interrupt at or below its priority from being
//! delivered.
//!
//! The APIC's page is device memory at a physical address the MSR names, so
//! it needs a mapping before anything can be written to it - [`init`] makes
//! that mapping, once, and [`eoi`] is then a single volatile store.

/// The `IA32_APIC_BASE` model-specific register.
const MSR_IA32_APIC_BASE: u32 = 0x1b;

/// Where the EOI register lives inside the APIC's page.
const APIC_EOI_OFFSET: usize = 0x0b0;

/// The virtual address of the EOI register, once [`init`] has mapped the
/// page. Zero until then, which makes `eoi` a no-op rather than a fault.
static EOI: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Map the local APIC's register page and remember where the EOI register
/// landed. Called once, before the first message-signalled interrupt can
/// arrive - the virtio driver's MSI-X arming is the caller - because the
/// mapping needs the kernel's page tables and an MMIO window, neither of
/// which the earliest boot guarantees.
pub fn init() {
    let base = unsafe { x86_64::registers::model_specific::Msr::new(MSR_IA32_APIC_BASE).read() }
        as usize
        & !0xfff;
    EOI.store(
        crate::mm::map_mmio(base, 0x1000) + APIC_EOI_OFFSET,
        core::sync::atomic::Ordering::Relaxed,
    );
}

/// Signal the end of the interrupt currently in service.
///
/// A plain zero write to the register [`init`] mapped. Called from interrupt
/// handlers: nothing here locks, allocates, or can fault.
pub fn eoi() {
    let at = EOI.load(core::sync::atomic::Ordering::Relaxed);
    if at != 0 {
        unsafe { (at as *mut u32).write_volatile(0) };
    }
}
