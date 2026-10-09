#![no_main]

//! ACPI tables: the root pointer, a table header, and MCFG's ECAM windows.
//!
//! `crates/acpi` reads three things a firmware supplies - the root pointer, each
//! table's header, and the memory windows MCFG names - and every one of them is
//! a length or an address that later arithmetic strides by. The failure worth
//! fuzzing is not a wrong window but a *read past the buffer that arrived*: a
//! table that claims more bytes than it was handed, an entry loop that does not
//! stop on a partial trailing record, an offset computed for a bus the window
//! does not cover.

use libfuzzer_sys::fuzz_target;
use mpos_acpi as acpi;

fuzz_target!(|data: &[u8]| {
    // The root pointer lives in a fixed 36-byte window; anything shorter is a
    // truncated table and must be refused rather than read past.
    let _ = acpi::parse_rsdp(data);

    if let Some(header) = acpi::parse_header(data) {
        let length = header.length as usize;
        assert!(length <= data.len(), "a table claims {length} of {} bytes", data.len());
        assert!(length >= acpi::SDT_HEADER_LEN, "a table shorter than its header");
    }

    // A body of `n` bytes holds at most `n / 16` windows, and every window must
    // describe an offset that is genuinely inside itself.
    let ranges = acpi::parse_mcfg(data);
    assert!(
        ranges.len() <= data.len() / 16 + 1,
        "{} windows from {} bytes",
        ranges.len(),
        data.len()
    );
    for range in &ranges {
        if range.is_empty() {
            continue;
        }
        assert!(
            range.len() <= 256 * acpi::ECAM_BUS_BYTES,
            "a window of {} bytes is wider than 256 buses",
            range.len()
        );
        for (bus, device, function) in [
            (range.start_bus, 0u8, 0u8),
            (range.end_bus, 31, 7),
            (range.start_bus, 32, 0),
            (range.end_bus, 0, 8),
        ] {
            if let Some(offset) = range.offset(bus, device, function) {
                assert!(
                    offset + 4096 <= range.len(),
                    "function {bus}:{device}.{function} sits at {offset}, outside {} bytes",
                    range.len()
                );
            }
        }
    }

    // Both root widths, because the width decides the stride and a partial
    // trailing entry must not be read.
    let _ = acpi::table_entries(data, true);
    let _ = acpi::table_entries(data, false);
});
