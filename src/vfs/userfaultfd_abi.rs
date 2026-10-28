//! `userfaultfd`'s published surface: the struct layouts, the request words and
//! the rules a shape has to satisfy before a page is ever touched.
//!
//! Host-compiled on purpose. Everything here is arithmetic over bytes - what a
//! caller's `struct uffdio_register` says, whether a mode word names a mode this
//! kernel serves, what a fault record looks like - and none of it needs a
//! machine to be true. The half that needs frames, address spaces and parked
//! tasks lives in [`crate::vfs::userfaultfd`], which is bare-metal only; the
//! split is the same one `io_uring_abi`/`io_uring` take, and for the same
//! reason: the shape is the part a host test can hold to Linux's measured
//! answers, so a mistake in it is caught without booting anything.
//!
//! **Every number below was measured on Linux 7.2.11 as real root**
//! (`build/scratch/uffd/probe.c`, `probe3.c` - the call is `EPERM` for an
//! unprivileged user, `vm.unprivileged_userfaultfd` being 0, so the shape has
//! no other source). The measured record is `docs/notes/06-process-management.md`
//! §6.33.

/// `UFFD_API` — the only API number this kernel answers, as measured.
pub const UFFD_API: u64 = 0xAA;

/// `sizeof(struct uffd_msg)`: what one `read(2)` delivers, and the smallest
/// buffer that is not `EINVAL`.
pub const MSG_BYTES: usize = 32;
/// `sizeof(struct uffdio_api)`.
pub const API_BYTES: usize = 24;
/// `sizeof(struct uffdio_register)`.
pub const REGISTER_BYTES: usize = 32;
/// `sizeof(struct uffdio_copy)`.
pub const COPY_BYTES: usize = 40;
/// `sizeof(struct uffdio_zeropage)` — range, mode, then the answer word.
pub const ZEROPAGE_BYTES: usize = 32;
/// `sizeof(struct uffdio_range)`.
pub const RANGE_BYTES: usize = 16;

/// The four ioctl request words this kernel answers, exactly as `_IOWR`/`_IOR`
/// compute them on Linux (measured, not derived: the sizes are in the word).
pub const UFFDIO_API_REQUEST: u32 = 0xc018_aa3f;
pub const UFFDIO_REGISTER_REQUEST: u32 = 0xc020_aa00;
pub const UFFDIO_UNREGISTER_REQUEST: u32 = 0x8010_aa01;
pub const UFFDIO_WAKE_REQUEST: u32 = 0x8010_aa02;
pub const UFFDIO_COPY_REQUEST: u32 = 0xc028_aa03;
pub const UFFDIO_ZEROPAGE_REQUEST: u32 = 0xc020_aa04;

/// The requests a real Linux also has, which this kernel refuses by name. The
/// words are Linux's, so a caller's `ioctl` reaches the right arm and hears an
/// honest refusal rather than `ENOTTY` about a request the ABI has.
pub const UFFDIO_MOVE_REQUEST: u32 = 0xc028_aa05;
pub const UFFDIO_WRITEPROTECT_REQUEST: u32 = 0xc018_aa06;
pub const UFFDIO_CONTINUE_REQUEST: u32 = 0xc020_aa07;
pub const UFFDIO_POISON_REQUEST: u32 = 0xc020_aa08;

/// `UFFD_EVENT_PAGEFAULT` — the only event kind this kernel delivers. It starts
/// at 0x12 rather than 0 in Linux, "to be more strict against bugs".
pub const EVENT_PAGEFAULT: u8 = 0x12;

/// The pagefault record's flag bits.
pub const PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;
pub const PAGEFAULT_FLAG_WP: u64 = 1 << 1;
pub const PAGEFAULT_FLAG_MINOR: u64 = 1 << 2;
pub const PAGEFAULT_FLAG_RWP: u64 = 1 << 3;

/// `UFFDIO_REGISTER_MODE_*`.
pub const MODE_MISSING: u64 = 1 << 0;
pub const MODE_WP: u64 = 1 << 1;
pub const MODE_MINOR: u64 = 1 << 2;
pub const MODE_RWP: u64 = 1 << 3;

/// The `ioctls` word `UFFDIO_API` answers with: which requests this kernel
/// serves. Linux answers `0x8000_0000_0000_0003` (the API bit plus
/// register/unregister); this kernel's set is the same two plus the API bit,
/// because that is exactly what it answers.
pub const API_IOCTLS: u64 = (1 << 0) | (1 << 1) | (1 << 63);

/// The `ioctls` word a successful `UFFDIO_REGISTER` answers with: which requests
/// work on that range. Linux answers `0x13c` for a missing-mode anonymous range
/// (bits 2, 3, 4, 5, 8 = WAKE, COPY, ZEROPAGE, WRITEPROTECT, POISON). This
/// kernel serves WAKE, COPY and ZEROPAGE, so its word is those three bits.
pub const RANGE_IOCTLS: u64 = (1 << 2) | (1 << 3) | (1 << 4);

/// The features this kernel's `UFFDIO_API` reports. Linux reports `0x1ffff`.
///
/// The one bit that changes behaviour is `PAGEFAULT_FLAG_WP` (bit 0): a caller
/// that asks for it is asking for write-protect mode, which this kernel refuses
/// by name, so it is not offered. Everything else in Linux's word is about
/// hugetlbfs, shmem, minor faults, exact addresses, poisoning and RWP - none of
/// which is served, and reporting a feature bit is a promise.
pub const FEATURES: u64 = 0;

/// What `UFFDIO_API` accepts: `api` must be `UFFD_API`, and the requested
/// feature word must ask for nothing this kernel does not report.
///
/// Measured: `api=0` and `api=0xAB` are `EINVAL`, and `features=~0` is `EINVAL`
/// (Linux refuses an unknown feature bit rather than ignoring it).
pub fn api_accepts(api: u64, features: u64) -> bool {
    api == UFFD_API && features & !FEATURES == 0
}

/// The validation `UFFDIO_REGISTER` performs before it touches the range.
///
/// Measured, in order: a mode word with no known mode bit is `EINVAL`
/// (`mode=0`, and `mode=0x8` without the RWP feature); a mode this kernel does
/// not serve is refused by name; a start that is not page-aligned is `EINVAL`;
/// and `len` that is zero or not a whole number of pages is `EINVAL`.
///
/// The page size is a parameter rather than a constant so the caller's own
/// page size is what is checked against - both machines here use 4 KiB, but the
/// rule is the shape's, not this machine's.
pub enum RegisterVerdict {
    /// A mode this kernel serves.
    Serve,
    /// A real mode this kernel does not implement: refuse with `EOPNOTSUPP`.
    Unsupported,
    /// Malformed input: `EINVAL`.
    Invalid,
}

pub fn register_verdict(mode: u64, start: u64, len: u64, page: u64) -> RegisterVerdict {
    // A mode word with no mode bit at all is malformed - not a request for
    // something this kernel lacks. Linux answers EINVAL for `mode=0`.
    if mode & (MODE_MISSING | MODE_WP | MODE_MINOR | MODE_RWP) == 0 {
        return RegisterVerdict::Invalid;
    }
    // Missing-mode is served. The other three are real modes with real
    // meanings this kernel has no machinery for: write-protect faults,
    // minor faults on a backing object, and read-write protection tracking.
    if mode != MODE_MISSING {
        return RegisterVerdict::Unsupported;
    }
    if !start.is_multiple_of(page) || len == 0 || !len.is_multiple_of(page) {
        return RegisterVerdict::Invalid;
    }
    RegisterVerdict::Serve
}

/// The validation `UFFDIO_COPY` performs. Measured: `len=0` and `len=1` are
/// `EINVAL` (a partial page is not a copy), an unknown mode bit is `EINVAL`,
/// and an unaligned `src` is **accepted** - the source is read with an ordinary
/// copy, which does not care where in a page it starts.
pub fn copy_len_ok(len: u64, page: u64) -> bool {
    len != 0 && len.is_multiple_of(page)
}

/// The mode bits `UFFDIO_COPY` understands: `MODE_DONTWAKE` (bit 0). Linux also
/// has `MODE_WP` (bit 1), which is the write-protect half this kernel refuses.
pub const COPY_MODE_DONTWAKE: u64 = 1 << 0;
pub const COPY_MODE_WP: u64 = 1 << 1;

pub enum CopyModeVerdict {
    Serve,
    Unsupported,
    Invalid,
}

pub fn copy_mode_verdict(mode: u64) -> CopyModeVerdict {
    if mode & !(COPY_MODE_DONTWAKE | COPY_MODE_WP) != 0 {
        // Measured: an unknown mode bit (0x100) is EINVAL.
        return CopyModeVerdict::Invalid;
    }
    if mode & COPY_MODE_WP != 0 {
        return CopyModeVerdict::Unsupported;
    }
    CopyModeVerdict::Serve
}

/// One 32-byte pagefault record, laid out as Linux's `struct uffd_msg` is:
/// `event` byte, three reserved bytes, then the union (here the pagefault arm:
/// `flags`, `address`, `ptid`).
pub fn pagefault_record(flags: u64, address: u64, ptid: u32) -> [u8; MSG_BYTES] {
    let mut out = [0u8; MSG_BYTES];
    out[0] = EVENT_PAGEFAULT;
    // bytes 1..4 stay zero: Linux's reserved fields.
    out[8..16].copy_from_slice(&flags.to_le_bytes());
    out[16..24].copy_from_slice(&address.to_le_bytes());
    out[24..28].copy_from_slice(&ptid.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured sizes and words, in one place: a change to any of them is a
    /// change to what a caller's struct means, and this is the test that says so.
    #[test]
    fn the_published_shape_is_the_measured_linux_one() {
        assert_eq!(UFFD_API, 0xAA);
        assert_eq!(MSG_BYTES, 32);
        assert_eq!(API_BYTES, 24);
        assert_eq!(REGISTER_BYTES, 32);
        assert_eq!(COPY_BYTES, 40);
        assert_eq!(UFFDIO_API_REQUEST, 0xc018_aa3f);
        assert_eq!(UFFDIO_REGISTER_REQUEST, 0xc020_aa00);
        assert_eq!(UFFDIO_UNREGISTER_REQUEST, 0x8010_aa01);
        assert_eq!(UFFDIO_WAKE_REQUEST, 0x8010_aa02);
        assert_eq!(UFFDIO_COPY_REQUEST, 0xc028_aa03);
        assert_eq!(UFFDIO_ZEROPAGE_REQUEST, 0xc020_aa04);
        assert_eq!(EVENT_PAGEFAULT, 0x12);
    }

    /// `_IOC`'s arithmetic, re-derived from the measured words: a request word
    /// carries its size in bits 16..30 and its direction in bits 30..32, so a
    /// struct that grows silently changes the word a caller sends.
    #[test]
    fn the_request_words_are_the_size_of_the_struct_they_name() {
        let size_of = |word: u32| ((word >> 16) & 0x3fff) as usize;
        assert_eq!(size_of(UFFDIO_API_REQUEST), API_BYTES);
        assert_eq!(size_of(UFFDIO_REGISTER_REQUEST), REGISTER_BYTES);
        assert_eq!(size_of(UFFDIO_COPY_REQUEST), COPY_BYTES);
        assert_eq!(size_of(UFFDIO_UNREGISTER_REQUEST), RANGE_BYTES);
        assert_eq!(size_of(UFFDIO_WAKE_REQUEST), RANGE_BYTES);
        assert_eq!(size_of(UFFDIO_ZEROPAGE_REQUEST), ZEROPAGE_BYTES);
        // And every one of them carries the `0xAA` type magic the family is.
        for word in [
            UFFDIO_API_REQUEST,
            UFFDIO_REGISTER_REQUEST,
            UFFDIO_UNREGISTER_REQUEST,
            UFFDIO_WAKE_REQUEST,
            UFFDIO_COPY_REQUEST,
            UFFDIO_ZEROPAGE_REQUEST,
        ] {
            assert_eq!((word >> 8) & 0xff, 0xAA);
        }
    }

    #[test]
    fn the_api_answers_the_api_it_serves_and_refuses_the_rest() {
        assert!(api_accepts(UFFD_API, 0));
        // Measured: api=0 and api=0xAB are EINVAL.
        assert!(!api_accepts(0, 0));
        assert!(!api_accepts(0xAB, 0));
        // Measured: an unknown feature bit is EINVAL, not ignored. This kernel
        // reports no features at all, so any bit asked for is unknown.
        assert!(!api_accepts(UFFD_API, u64::MAX));
        assert!(!api_accepts(UFFD_API, PAGEFAULT_FLAG_WRITE));
    }

    #[test]
    fn a_register_mode_is_served_refused_or_malformed() {
        let page = 4096;
        // The served mode, measured 0 with ioctls=0x13c.
        assert!(matches!(
            register_verdict(MODE_MISSING, 0x1000, 0x2000, page),
            RegisterVerdict::Serve
        ));
        // Measured: mode=0 is EINVAL (no mode bit at all).
        assert!(matches!(
            register_verdict(0, 0x1000, 0x1000, page),
            RegisterVerdict::Invalid
        ));
        // Measured: mode=0x8 (RWP) is EINVAL on a kernel without the feature,
        // and a real mode this kernel lacks is refused by name.
        assert!(matches!(
            register_verdict(MODE_WP, 0x1000, 0x1000, page),
            RegisterVerdict::Unsupported
        ));
        assert!(matches!(
            register_verdict(MODE_MINOR, 0x1000, 0x1000, page),
            RegisterVerdict::Unsupported
        ));
        assert!(matches!(
            register_verdict(MODE_RWP, 0x1000, 0x1000, page),
            RegisterVerdict::Unsupported
        ));
        // Measured: an unaligned start, len=0 and len=1 are all EINVAL.
        assert!(matches!(
            register_verdict(MODE_MISSING, 0x1001, 0x1000, page),
            RegisterVerdict::Invalid
        ));
        assert!(matches!(
            register_verdict(MODE_MISSING, 0x1000, 0, page),
            RegisterVerdict::Invalid
        ));
        assert!(matches!(
            register_verdict(MODE_MISSING, 0x1000, 1, page),
            RegisterVerdict::Invalid
        ));
    }

    #[test]
    fn a_copy_mode_is_the_one_bit_that_is_served() {
        assert!(matches!(copy_mode_verdict(0), CopyModeVerdict::Serve));
        assert!(matches!(
            copy_mode_verdict(COPY_MODE_DONTWAKE),
            CopyModeVerdict::Serve
        ));
        // Measured: an unknown mode bit (0x100) is EINVAL.
        assert!(matches!(copy_mode_verdict(0x100), CopyModeVerdict::Invalid));
        // MODE_WP is a real mode whose meaning is write-protect, which this
        // kernel refuses by name rather than pretending to honour.
        assert!(matches!(
            copy_mode_verdict(COPY_MODE_WP),
            CopyModeVerdict::Unsupported
        ));
        assert!(copy_len_ok(4096, 4096));
        assert!(!copy_len_ok(0, 4096));
        assert!(!copy_len_ok(1, 4096));
        assert!(!copy_len_ok(4097, 4096));
    }

    /// The record a `read(2)` hands back, byte for byte: measured as
    /// `event=0x12`, `flags=0`, `address` page-aligned, `ptid=0` when the
    /// THREAD_ID feature was not asked for (this kernel never reports it).
    #[test]
    fn a_pagefault_record_is_linux_s_32_bytes() {
        let rec = pagefault_record(0, 0x7f48_0c48_1000, 0);
        assert_eq!(rec.len(), MSG_BYTES);
        assert_eq!(rec[0], EVENT_PAGEFAULT);
        assert_eq!(&rec[1..8], &[0u8; 7], "reserved bytes stay zero");
        assert_eq!(u64::from_le_bytes(rec[8..16].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(rec[16..24].try_into().unwrap()),
            0x7f48_0c48_1000
        );
        assert_eq!(u32::from_le_bytes(rec[24..28].try_into().unwrap()), 0);
        // A write fault's flags word, measured as 0x3 (WRITE|WP) under
        // write-protect mode.
        let rec = pagefault_record(PAGEFAULT_FLAG_WRITE | PAGEFAULT_FLAG_WP, 0x1000, 7);
        assert_eq!(u64::from_le_bytes(rec[8..16].try_into().unwrap()), 0x3);
        assert_eq!(u32::from_le_bytes(rec[24..28].try_into().unwrap()), 7);
    }
}
