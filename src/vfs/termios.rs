//! The terminal's settings: what a `termios` says a line is.
//!
//! A program asks the terminal what it is with `TCGETS` and changes its mind
//! with `TCSETS`. Until now this kernel answered "nothing at all" - sixty zero
//! bytes - and then carried on editing and echoing regardless, which is why a
//! shell that asked for raw mode (no line editing, no echo, keys handed over
//! one at a time) kept being fed cooked lines it had already echoed itself:
//! every typed line came back twice, and every arrow key arrived as loose
//! punctuation.
//!
//! The flags live here as constants, and [`Termios`] is the 60-byte structure
//! userspace reads and writes. The line discipline in [`super::tty`] is what
//! obeys them.

/// Bytes in the userspace `struct termios`, and in `c_cc`.
pub const NCCS: usize = 32;
/// The size of the `struct termios` glibc lays out - four flag words, a line
/// discipline byte, the control characters, and two speeds.
pub const SIZE: usize = 60;

// --- c_iflag: what happens to input on the way in. ---
pub const IGNBRK: u32 = 0x0001;
pub const BRKINT: u32 = 0x0002;
pub const IGNPAR: u32 = 0x0004;
pub const ISTRIP: u32 = 0x0020;
pub const INLCR: u32 = 0x0040;
pub const IGNCR: u32 = 0x0080;
/// Carriage return becomes newline: Enter is one key, whatever the terminal
/// sends for it.
pub const ICRNL: u32 = 0x0100;
pub const IXON: u32 = 0x0400;
pub const IXANY: u32 = 0x0800;
pub const IMAXBEL: u32 = 0x2000;
pub const IUTF8: u32 = 0x4000;

// --- c_oflag: what happens to output on the way out. ---
pub const OPOST: u32 = 0x0001;
pub const ONLCR: u32 = 0x0004;

// --- c_cflag, reported but not acted on: a serial line we do not have. ---
pub const CS8: u32 = 0x0030;
pub const CREAD: u32 = 0x0080;
pub const B38400: u32 = 0x000f;

// --- c_lflag: what the line discipline does. ---
/// Control characters generate signals rather than becoming input.
pub const ISIG: u32 = 0x0001;
/// Canonical mode: bytes are edited into a line, and a read takes a line.
pub const ICANON: u32 = 0x0002;
pub const ECHO: u32 = 0x0008;
pub const ECHOE: u32 = 0x0010;
pub const ECHOK: u32 = 0x0020;
pub const ECHONL: u32 = 0x0040;
pub const NOFLSH: u32 = 0x0080;
pub const ECHOCTL: u32 = 0x0200;
pub const ECHOPRT: u32 = 0x0400;
pub const ECHOKE: u32 = 0x0800;
pub const IEXTEN: u32 = 0x8000;

// --- c_cc indices. ---
pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VEOL: usize = 11;
pub const VREPRINT: usize = 12;
pub const VDISCARD: usize = 13;
pub const VWERASE: usize = 14;
pub const VLNEXT: usize = 15;

/// A terminal's settings. The fields are the POSIX ones; the kernel only
/// consults the flags it knows how to honour, and reports the rest as set so
/// a program that checks them is not told it is talking to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
    pub ispeed: u32,
    pub ospeed: u32,
}

impl Termios {
    /// What a console is set to before anyone asks for anything else: line by
    /// line, echoed, interruptible, Enter arriving as a newline - and output
    /// post-processed, which is what makes a newline *mean* a new line.
    ///
    /// `OPOST|ONLCR` are Linux's own default (`tty_std_termios`), and they are
    /// the pair a terminal cannot do without. A screen is a *column* as well as
    /// a cursor: a bare `LF` moves down and leaves the carriage where it was,
    /// so each line starts where the last one ended and the text staircases to
    /// the right - one column of indent per line, which is exactly how it
    /// reads. Something has to turn a program's `\n` into `\r\n`, and the
    /// terminal driver is the only thing in a position to: the other end of a
    /// `/dev/pts` pair is a *terminal emulator*, which by definition does no
    /// translation of its own.
    ///
    /// These used to be off, and the reasoning is worth writing down because it
    /// is right about one case and wrong about the rest. The bytes a task
    /// writes on the *serial* console do reach a host that translates: QEMU
    /// writes the guest's `LF` to its own stdout, which is a host pty, and the
    /// host's line discipline turns it into `CR LF` before the host terminal
    /// emulator sees it. So the serial console looked correct without them -
    /// because a different machine's terminal driver was doing this kernel's
    /// job. Inside the guest there is no host in the path, and a compositor's
    /// terminal shows the staircase.
    pub const fn console_default() -> Termios {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03; // ^C
        cc[VQUIT] = 0x1c; // ^\
        cc[VERASE] = 0x7f; // the key terminals send for backspace
        cc[VKILL] = 0x15; // ^U
        cc[VEOF] = 0x04; // ^D
        cc[VSTART] = 0x11; // ^Q
        cc[VSTOP] = 0x13; // ^S
        cc[VSUSP] = 0x1a; // ^Z
        cc[VREPRINT] = 0x12; // ^R
        cc[VWERASE] = 0x17; // ^W
        cc[VLNEXT] = 0x16; // ^V
        cc[VMIN] = 1;
        cc[VTIME] = 0;
        Termios {
            iflag: ICRNL | IXON | BRKINT | IMAXBEL | IUTF8,
            oflag: OPOST | ONLCR,
            cflag: CS8 | CREAD | B38400,
            lflag: ISIG | ICANON | IEXTEN | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE,
            line: 0,
            cc,
            ispeed: 38400,
            ospeed: 38400,
        }
    }
}

// The 60-byte layout glibc uses: four flag words, one line-discipline byte,
// 32 control characters, then the two speeds - padded, because a `speed_t`
// wants four-byte alignment and `c_cc` ends on an odd one.
const OFF_IFLAG: usize = 0;
const OFF_OFLAG: usize = 4;
const OFF_CFLAG: usize = 8;
const OFF_LFLAG: usize = 12;
const OFF_LINE: usize = 16;
const OFF_CC: usize = 17;
const OFF_ISPEED: usize = 52;
const OFF_OSPEED: usize = 56;

/// Read a flag word from a userspace buffer that may not be aligned.
///
/// # Safety
/// `base` must point to at least `SIZE` readable bytes.
unsafe fn read_u32(base: *const u8, off: usize) -> u32 {
    unsafe { core::ptr::read_unaligned(base.add(off) as *const u32) }
}

/// Write a flag word to a userspace buffer that may not be aligned.
///
/// # Safety
/// `base` must point to at least `SIZE` writable bytes.
unsafe fn write_u32(base: *mut u8, off: usize, value: u32) {
    unsafe { core::ptr::write_unaligned(base.add(off) as *mut u32, value) }
}

impl Termios {
    /// Parse the structure a program handed the kernel.
    ///
    /// # Safety
    /// `ptr` must point to at least `SIZE` readable bytes.
    pub unsafe fn from_ptr(ptr: *const u8) -> Termios {
        let mut cc = [0u8; NCCS];
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.add(OFF_CC), cc.as_mut_ptr(), NCCS);
        }
        Termios {
            iflag: unsafe { read_u32(ptr, OFF_IFLAG) },
            oflag: unsafe { read_u32(ptr, OFF_OFLAG) },
            cflag: unsafe { read_u32(ptr, OFF_CFLAG) },
            lflag: unsafe { read_u32(ptr, OFF_LFLAG) },
            line: unsafe { *ptr.add(OFF_LINE) },
            cc,
            ispeed: unsafe { read_u32(ptr, OFF_ISPEED) },
            ospeed: unsafe { read_u32(ptr, OFF_OSPEED) },
        }
    }

    /// Lay the structure out where a program can read it.
    ///
    /// # Safety
    /// `ptr` must point to at least `SIZE` writable bytes.
    pub unsafe fn store_to(&self, ptr: *mut u8) {
        unsafe {
            core::ptr::write_bytes(ptr, 0, SIZE);
            write_u32(ptr, OFF_IFLAG, self.iflag);
            write_u32(ptr, OFF_OFLAG, self.oflag);
            write_u32(ptr, OFF_CFLAG, self.cflag);
            write_u32(ptr, OFF_LFLAG, self.lflag);
            *ptr.add(OFF_LINE) = self.line;
            core::ptr::copy_nonoverlapping(self.cc.as_ptr(), ptr.add(OFF_CC), NCCS);
            write_u32(ptr, OFF_ISPEED, self.ispeed);
            write_u32(ptr, OFF_OSPEED, self.ospeed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ECHO, ICANON, NCCS, SIZE, Termios};

    #[test]
    fn a_stored_termios_reads_back_as_itself() {
        let mut buf = [0u8; SIZE];
        let tio = Termios::console_default();
        unsafe { tio.store_to(buf.as_mut_ptr()) };
        let back = unsafe { Termios::from_ptr(buf.as_ptr()) };
        assert_eq!(back, tio);
        assert_eq!(back.cc.len(), NCCS);
    }

    #[test]
    fn the_console_default_is_a_line_at_a_time_and_echoed() {
        let tio = Termios::console_default();
        assert_ne!(tio.lflag & ICANON, 0, "a console edits into lines");
        assert_ne!(tio.lflag & ECHO, 0, "and shows what is typed");
        assert_eq!(tio.cc[super::VERASE], 0x7f);
        assert_eq!(tio.cc[super::VEOF], 0x04);
    }
}
