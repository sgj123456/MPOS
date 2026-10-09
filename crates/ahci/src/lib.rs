#![no_std]

//! What an AHCI port's status words mean, kept apart from the bus that reads them.
//!
//! Why this is its own crate: `drivers::ahci` is compiled only for the
//! machine (`src/lib.rs` gates the whole `drivers` module on
//! `target_os = "none"`), so a decision written inside it is a decision no host
//! test can reach - which is how a port that reports a failure and never
//! retires its command came to cost 28.3 s of the boot, a thing nobody had
//! measured because nobody could assert on it. The three registers a command
//! wait consults are three `u32`s, and the question "is this command still
//! going, has it failed, or has the device said it is an ATAPI one" is
//! arithmetic. `mpos-pci` beside `drivers/pci` is the same split, and being a
//! crate makes this half's downward-only-ness a compile error rather than a
//! convention: nothing here can name the kernel it serves.
//!
//! Every expected value below is a measurement, not a reading of the spec: the
//! numbers in [`tests`] are what a `q35` machine's own SATA controller at
//! `00:1f.2` returned on a port that claims a device and has none, taken with
//! the register printout described in `NOTES.md` §7.3.

extern crate alloc;

/// `PxIS.TFES`: the port latched a task-file error. The bit the driver already
/// reads *after* it stops waiting, which is the ordering this module exists to
/// fix - see [`PortStatus::waiting`].
pub const IS_TFES: u32 = 1 << 30;
/// `PxTFD.ERR` and `PxTFD.DF`: the device's own "no" - an ATA error and a
/// device fault.
pub const TFD_ERR: u32 = 1 << 0;
pub const TFD_DF: u32 = 1 << 5;

/// `PxSIG` for an ATAPI device, which speaks a different command set than the
/// driver does.
///
/// The *only* signature refused, and deliberately not a test for the ATA one:
/// the controller this was measured against reports `0xffffffff` on the port
/// that has the disk on it until that port has been brought up, so a driver that
/// waited for `0x00000101` would never bring up the port that has the disk.
/// Whether the device is an ATA disk is what `IDENTIFY` answers, and
/// `PxSSTS.DET` is the presence question, which is a different one.
///
/// The other half of the measurement, which is the one that cost a boot: a
/// phantom port - `DET == 3`, nothing behind it - reads `0xffffffff` before it
/// is started and exactly this ATAPI word afterwards. So the signature answers
/// a question about the device only once the port is running, and it is not a
/// bound on how long a command may take waiting for an answer, which is what
/// [`PortStatus`] is for.
pub const SIG_ATAPI: u32 = 0xeb14_0101;

/// The three status words of one port, read together.
///
/// Reading them in one place is not tidy-ness: the retirement bit and the error
/// bits are separate registers, and a driver that polls only the first can wait
/// for an event that the second has already announced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    /// `PxCI`: the command slots still in progress.
    pub ci: u32,
    /// `PxIS`: the port's latched interrupt status, polled here rather than
    /// taken as an interrupt - this driver enables no line.
    pub is: u32,
    /// `PxTFD`: the device's own task file, error and drive-select bits included.
    pub tfd: u32,
    /// The slot this driver ever uses: slot 0.
    pub slot: u32,
}

impl PortStatus {
    /// Has the device reported a failure this command should stop for?
    ///
    /// Either half is enough: `PxIS.TFES` is the latch the port sets when the
    /// device answers with an error, and `PxTFD.ERR`/`.DF` is the same fact in
    /// the task file, which is what the driver checks once a wait ends. A
    /// phantom device sets both and never retires the command bit, so the latch
    /// is what turns a 28.3 s poll into one round of reads.
    pub fn failed(&self) -> bool {
        self.is & IS_TFES != 0 || self.tfd & (TFD_ERR | TFD_DF) != 0
    }

    /// Has the command left the slot? The bit is the device's receipt, cleared
    /// when the command is finished with - successfully or not, which is why
    /// [`Self::failed`] is asked as well and not instead.
    pub fn retired(&self) -> bool {
        self.ci & self.slot == 0
    }

    /// Should the wait for this command go on?
    ///
    /// `false` on either answer: the slot is empty, or the port has said the
    /// command is over and bad. The caller then reads the task file again for
    /// the error it must report, so this function ends the waiting and never
    /// decides the outcome.
    pub fn waiting(&self) -> bool {
        !(self.retired() || self.failed())
    }
}

/// The question [`SIG_ATAPI`] exists to be asked: is this the one device class
/// the driver's scope excludes? Equality, not "not an ATA disk", for the reason
/// written on the constant.
pub fn signature_is_atapi(sig: u32) -> bool {
    sig == SIG_ATAPI
}

/// The byte limit one PRDT entry's `DBC` field can say: it is a 22-bit
/// "bytes minus one", so the longest run it describes is 4 MiB.
pub const PRDT_MAX_BYTES: usize = 1 << 22;
/// `DBC`, the byte count minus one, in the low 22 bits of an entry's flags.
const PRDT_DBC_MASK: u32 = 0x003f_ffff;
/// `IADB`: the completion signal, and the reason a table has a *last* entry.
const PRDT_IADB: u32 = 1 << 31;

/// One entry's flags word: how many bytes the device moves into this run, and
/// whether this is the run that ends the command.
///
/// `None` says a run is uncodable - empty, or longer than [`PRDT_MAX_BYTES`] -
/// which is a question worth asking out loud, because writing the low 22 bits of
/// a 5 MiB count describes a transfer that silently stops short. `DBC` is
/// `bytes - 1`, and `IADB` belongs on the final entry alone: the port reports
/// the command finished when *that* descriptor completes, so a driver that set it
/// on every entry would be told "done" at the end of the first page of a
/// multi-page read and hand back a buffer with the rest of it unwritten.
pub fn prdt_flags(bytes: usize, final_entry: bool) -> Option<u32> {
    if bytes == 0 || bytes > PRDT_MAX_BYTES {
        return None;
    }
    Some((bytes - 1) as u32 & PRDT_DBC_MASK | if final_entry { PRDT_IADB } else { 0 })
}

/// The byte count of run `index` of a table that moves `bytes` in page-sized
/// runs: a whole page for every run the transfer fills, and what is left for the
/// one that stops partway.
///
/// Zero past the end, so a caller that walks `0..entries` reads only the runs it
/// declared and the arithmetic for the last one is in one place.
pub const fn run_bytes(bytes: usize, index: usize, page: usize) -> usize {
    let start = index * page;
    if start >= bytes {
        return 0;
    }
    let left = bytes - start;
    if left < page { left } else { page }
}

/// How many descriptors a table of `table_bytes` can hold when its runs start at
/// `prdt_offset` and each entry is 16 bytes.
///
/// The driver's compile-time check that its bounce holds no more runs than this,
/// so a request cannot describe a table the device would read past.
pub const fn prdt_capacity(table_bytes: usize, prdt_offset: usize) -> usize {
    (table_bytes - prdt_offset) / 16
}

/// The pieces one command can move, for a request of `bytes` and a command that
/// carries at most `command_bytes`.
///
/// Each piece is `command_bytes` except the last, which is what is left. That
/// shape is the whole of the driver's loop over a long request, and it is worth
/// saying separately from the bus: `command_bytes` is a page multiple because the
/// runs are pages, and a command's sector count has to be exact, so a piece that
/// was not a whole number of sectors would be a transfer of the wrong length
/// rather than a shorter piece.
pub fn chunks(bytes: usize, command_bytes: usize) -> ChunkPlan {
    ChunkPlan {
        left: bytes,
        size: command_bytes,
    }
}

/// The iterator [`chunks`] hands back: the length of each command's transfer.
#[derive(Clone, Copy, Debug)]
pub struct ChunkPlan {
    left: usize,
    size: usize,
}

impl Iterator for ChunkPlan {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.left == 0 {
            return None;
        }
        let take = self.size.min(self.left);
        self.left -= take;
        Some(take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// The phantom port, measured on the `q35` chipset's own controller at
    /// `00:1f.2`, port 2, while it still holds `IDENTIFY`: `PxCI` 0x1, `PxIS`
    /// 0x40000001, `PxTFD` 0x441. It claims `PxSSTS.DET == 3` and has no device
    /// behind it. Before this module the driver waited on `PxCI` alone, so these
    /// three words meant "keep polling" for 5,000,000 reads = 28.3 s.
    const PHANTOM: PortStatus = PortStatus {
        ci: 0x1,
        is: 0x4000_0001,
        tfd: 0x0441,
        slot: 1,
    };

    /// A healthy port mid-command: slot 0 still busy, no error latched, the
    /// device not yet offering the sector buffer.
    const BUSY: PortStatus = PortStatus {
        ci: 0x1,
        is: 0x0,
        tfd: 0x0080,
        slot: 1,
    };

    /// The same command once the device has retired it and left DRQ up.
    const DONE: PortStatus = PortStatus {
        ci: 0x0,
        is: 0x0,
        tfd: 0x0050,
        slot: 1,
    };

    #[test]
    fn a_command_that_never_retires_but_reported_an_error_stops_the_wait() {
        assert!(PHANTOM.failed(), "the phantom port latched TFES");
        assert!(!PHANTOM.retired(), "and it never cleared PxCI");
        assert!(
            !PHANTOM.waiting(),
            "so the wait must end, which is the 28.3 s this pin exists to prevent"
        );
    }

    #[test]
    fn a_busy_port_is_waited_on_and_a_retired_one_is_not() {
        assert!(BUSY.waiting(), "a device still working is not a failure");
        assert!(!BUSY.failed());
        assert!(!DONE.waiting());
        assert!(DONE.retired());
        assert!(!DONE.failed());
    }

    #[test]
    fn the_task_file_error_bits_alone_end_the_wait() {
        // The latch can already have been cleared by a previous command's own
        // error handling, so the task file has to be a sufficient answer too.
        let err_only = PortStatus {
            ci: 0x1,
            is: 0x0,
            tfd: TFD_ERR,
            slot: 1,
        };
        assert!(err_only.failed());
        assert!(!err_only.waiting());
        let fault_only = PortStatus {
            tfd: TFD_DF,
            ..err_only
        };
        assert!(fault_only.failed());
    }

    #[test]
    fn only_the_slot_this_driver_uses_answers_for_the_command() {
        // Slot 0 is the only bit this driver sets, so a stale bit in another
        // slot must not be read as "my command is still going" - nor may my own
        // slot's retirement be hidden by one.
        let other_slot = PortStatus {
            ci: 0x8,
            is: 0x0,
            tfd: 0x50,
            slot: 1,
        };
        assert!(other_slot.retired());
        assert!(!other_slot.waiting());
    }

    #[test]
    fn the_measured_atapi_signature_is_the_one_the_scope_excludes() {
        assert!(signature_is_atapi(SIG_ATAPI));
        // An ATA disk's signature, and the word a stopped port reads before it
        // has been started: neither is a reason to skip the port - `IDENTIFY`
        // is what classifies a disk.
        assert!(!signature_is_atapi(0x0000_0101));
        assert!(!signature_is_atapi(0xffff_ffff));
    }

    #[test]
    fn a_longer_table_says_done_only_when_the_last_run_is_moved() {
        // The 8 KiB read that is this module's counterfactual case: a driver
        // with one descriptor cannot do it at all, and one with two that both
        // claim the interrupt bit ends the command half way through.
        let page = 4096;
        let runs = chunks(2 * page, 32 * page).collect::<Vec<usize>>();
        assert_eq!(runs, vec![8192], "one command, because it fits");
        let entries = runs[0] / page;
        assert_eq!(entries, 2);
        for index in 0..entries {
            let flags = prdt_flags(page, index + 1 == entries).expect("a page is codable");
            assert_eq!(
                flags & PRDT_DBC_MASK,
                (page - 1) as u32,
                "DBC is bytes minus one"
            );
            let last = index + 1 == entries;
            assert_eq!(
                flags & PRDT_IADB != 0,
                last,
                "only the final run ends the command"
            );
        }
    }

    #[test]
    fn a_run_the_count_field_cannot_say_is_refused_rather_than_wrapped() {
        assert_eq!(prdt_flags(0, true), None, "an empty run describes nothing");
        assert_eq!(prdt_flags(1 << 22, true), Some(0x003f_ffff | PRDT_IADB));
        assert_eq!(
            prdt_flags((1 << 22) + 512, true),
            None,
            "one more sector than `DBC` can count has no encoding, and the low 22 bits \
             of it would be a transfer that stops short and calls that success"
        );
    }

    #[test]
    fn the_two_measured_long_requests_are_planned_whole() {
        // 262144 is what `udevd` asks a new disk for, and 6656 is the window a
        // 6144-byte unaligned superblock read covers. Both reached the driver as
        // one call and panicked on its one-page limit; each is now a plan whose
        // pieces add up to the request and whose non-final pieces are pages.
        let command = 32 * 4096;
        for bytes in [262144usize, 6656, 8192, command, command + 512] {
            let plan = chunks(bytes, command).collect::<Vec<usize>>();
            assert_eq!(
                plan.iter().sum::<usize>(),
                bytes,
                "{bytes} planned wrong: {plan:?}"
            );
            let tail = if bytes % command == 0 {
                command
            } else {
                bytes % command
            };
            assert_eq!(
                *plan.last().unwrap(),
                tail,
                "the last piece is what is left"
            );
            for piece in &plan[..plan.len() - 1] {
                assert_eq!(*piece, command, "every earlier piece fills the command");
                assert_eq!(
                    piece % 4096,
                    0,
                    "a piece that is not a page multiple has no exact sector count"
                );
            }
            assert!(plan.iter().all(|piece| *piece <= command));
        }
        assert_eq!(
            chunks(0, command).count(),
            0,
            "nothing to move is not one empty command"
        );
    }

    #[test]
    fn one_command_page_holds_two_hundred_forty_eight_descriptors() {
        // The layout `drivers::ahci` sizes its command table against: the runs
        // begin after the 64-byte FIS and the reserved gap, at 0x80.
        assert_eq!(prdt_capacity(4096, 0x80), 248);
        // The runs themselves: 6656 bytes over a 4096-byte page is one full page
        // and 2560 bytes, and a third run that the driver might still ask for is
        // zero rather than an out-of-range read.
        assert_eq!(run_bytes(6656, 0, 4096), 4096);
        assert_eq!(run_bytes(6656, 1, 4096), 2560);
        assert_eq!(run_bytes(6656, 2, 4096), 0);
        assert_eq!(run_bytes(8192, 0, 4096), 4096);
        assert_eq!(run_bytes(8192, 1, 4096), 4096);
        assert_eq!(run_bytes(8192, 2, 4096), 0);
    }
}
