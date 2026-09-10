//! The tty: a line discipline between the machine's input and its readers.
//!
//! Bytes arrive one at a time - today from whoever calls [`Tty::receive_byte`],
//! tomorrow from a serial interrupt - and editing happens here, not in the
//! reader: backspace, CR-to-LF, and Ctrl-D end-of-file are all resolved before
//! a read returns. That makes [`Tty::read`] canonical in the POSIX sense: one
//! read, one line, the newline included.
//!
//! [`LineDiscipline`] is the pure state machine and runs under `cargo test` on
//! the host. [`Tty`] owns one under a mutex, echoes to the console as bytes are
//! received, and turns `read` into a blocking read via
//! [`crate::sched::block_until`].
//!
//! The standard descriptors fd 0, 1 and 2 are three open descriptions of this
//! one inode, created at start-up (`vfs::file::init_standard`). A `/dev/tty`
//! node waits until fds are per-process (M12), when a shell can open one;
//! until then nothing user side names this device.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::{Inode, Metadata, NodeKind, VfsResult};
use crate::sched::policy::TaskId;

const BACKSPACE: u8 = 0x08;
const LF: u8 = b'\n';
const CR: u8 = b'\r';
const DEL: u8 = 0x7f;
/// Ctrl-D. On an empty line it ends input; mid-line it flushes what is typed,
/// which is how a shell reads `cat` input without a trailing newline.
const EOF_CHAR: u8 = 0x04;

/// One completed input line. `Eof` is not bytes: it is the statement "input
/// ended here", and a read over it reports 0.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Data(Vec<u8>),
    Eof,
}

/// The editing state machine: input bytes in through [`receive`](Self::receive),
/// completed lines and console echo out.
#[derive(Default)]
pub struct LineDiscipline {
    pending: Vec<u8>,
    ready: VecDeque<Line>,
    echo: Vec<u8>,
}

impl LineDiscipline {
    pub const fn new() -> LineDiscipline {
        LineDiscipline {
            pending: Vec::new(),
            ready: VecDeque::new(),
            echo: Vec::new(),
        }
    }

    /// Feed one input byte, queueing whatever the console should echo.
    pub fn receive(&mut self, byte: u8) {
        match byte {
            BACKSPACE | DEL => {
                if self.pending.pop().is_some() {
                    // Erase on a terminal: step back, blank, step back again.
                    self.echo.extend_from_slice(b"\x08 \x08");
                }
            }
            CR | LF => self.complete_line(),
            EOF_CHAR => {
                // Mid-line: flush without the newline. Empty: end of input.
                if self.pending.is_empty() {
                    self.ready.push_back(Line::Eof);
                } else {
                    self.echo.push(LF);
                    let line = core::mem::take(&mut self.pending);
                    self.ready.push_back(Line::Data(line));
                }
            }
            0x20..=0x7e => {
                self.pending.push(byte);
                self.echo.push(byte);
            }
            // Other control bytes have no editing meaning yet; keep them out
            // of the line rather than passing them through raw.
            _ => {}
        }
    }

    fn complete_line(&mut self) {
        self.pending.push(LF);
        self.echo.push(LF);
        let line = core::mem::take(&mut self.pending);
        self.ready.push_back(Line::Data(line));
    }

    /// The echo queued since the last drain.
    pub fn take_echo(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.echo)
    }

    pub fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Take one completed line, in at most `want` bytes of it. A longer line
    /// stays queued minus the part taken, so the next read continues it.
    pub fn take_line(&mut self, want: usize) -> Option<Line> {
        match self.ready.pop_front() {
            Some(Line::Data(bytes)) if bytes.len() > want => {
                self.ready.push_front(Line::Data(bytes[want..].to_vec()));
                Some(Line::Data(bytes[..want].to_vec()))
            }
            other => other,
        }
    }
}

/// The machine's one input line discipline, behind a mutex.
pub struct Tty {
    ld: Mutex<RefCell<LineDiscipline>>,
    /// Tasks blocked in [`Tty::read`]. A producer cannot name them, but the
    /// tty can: a completed line is worthless if the only reader stays asleep.
    waiters: Mutex<RefCell<Vec<TaskId>>>,
}

impl Tty {
    pub const fn new() -> Tty {
        Tty {
            ld: Mutex::new(RefCell::new(LineDiscipline::new())),
            waiters: Mutex::new(RefCell::new(Vec::new())),
        }
    }

    /// Feed one byte from the input side, show its echo, and wake any reader
    /// the byte's effects can satisfy. Runs with interrupts off for the length
    /// of the edit, which is nothing next to the byte's own arrival rate.
    pub fn receive_byte(&self, byte: u8) {
        let (echo, waiters) = critical_section::with(|cs| {
            let mut ld = self.ld.borrow(cs).borrow_mut();
            ld.receive(byte);
            let echo = ld.take_echo();
            let waiters = if ld.has_ready() {
                core::mem::take(&mut *self.waiters.borrow(cs).borrow_mut())
            } else {
                Vec::new()
            };
            (echo, waiters)
        });
        #[cfg(target_os = "none")]
        {
            crate::console::write_bytes(&echo);
            for id in waiters {
                crate::sched::wake(id);
            }
        }
        #[cfg(not(target_os = "none"))]
        drop((echo, waiters));
    }

    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn has_ready(&self) -> bool {
        critical_section::with(|cs| self.ld.borrow(cs).borrow().has_ready())
    }

    /// Blocking canonical read: at most one line, or 0 once input has ended.
    /// Never errors; the size is what the syscall returns.
    #[cfg(target_os = "none")]
    pub fn read(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let id = crate::sched::current();
        critical_section::with(|cs| self.waiters.borrow(cs).borrow_mut().push(id));
        let got = self.read_blocking(buf);
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
        got
    }

    /// The same contract without a scheduler. On the host, tty reads block by
    /// construction, so only the discipline itself is testable.
    #[cfg(not(target_os = "none"))]
    pub fn read(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        panic!("blocking tty reads need the kernel scheduler; test LineDiscipline instead");
    }

    #[cfg(target_os = "none")]
    fn read_blocking(&self, buf: &mut [u8]) -> usize {
        loop {
            crate::sched::block_until(|| self.has_ready());
            let line =
                critical_section::with(|cs| self.ld.borrow(cs).borrow_mut().take_line(buf.len()));
            match line {
                Some(Line::Data(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    return bytes.len();
                }
                Some(Line::Eof) => return 0,
                // Another reader took the line first; wait for the next one.
                None => {}
            }
        }
    }
}

static TTY: Mutex<RefCell<Option<Arc<Tty>>>> = Mutex::new(RefCell::new(None));

impl Default for Tty {
    fn default() -> Tty {
        Tty::new()
    }
}

/// Create the console tty and hand back the shared handle. Calling `init`
/// again returns the same tty: there is exactly one, because the machine's
/// input line is not a per-task thing.
pub fn init() -> Arc<Tty> {
    critical_section::with(|cs| {
        TTY.borrow(cs)
            .borrow_mut()
            .get_or_insert_with(|| Arc::new(Tty::new()))
            .clone()
    })
}

/// The console tty, for whoever feeds it input.
pub fn global() -> Arc<Tty> {
    critical_section::with(|cs| {
        TTY.borrow(cs)
            .borrow()
            .as_ref()
            .expect("the tty exists only after vfs::tty::init()")
            .clone()
    })
}

impl Inode for Tty {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::CharDev,
            // A stream has no size; reporting one would be a lie.
            size: 0,
        }
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(self.read(buf))
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        #[cfg(target_os = "none")]
        crate::console::write_bytes(buf);
        #[cfg(not(target_os = "none"))]
        let _ = buf;
        Ok(buf.len())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{Line, LineDiscipline};

    fn fed(bytes: &[u8]) -> LineDiscipline {
        let mut ld = LineDiscipline::new();
        for &b in bytes {
            ld.receive(b);
        }
        ld
    }

    fn lines(ld: &mut LineDiscipline) -> Vec<Line> {
        let mut out = Vec::new();
        while let Some(line) = ld.take_line(64) {
            out.push(line);
        }
        out
    }

    #[test]
    fn typing_a_line_echoes_it_and_completes_on_newline() {
        let mut ld = fed(b"hi\r");
        assert_eq!(ld.take_echo(), b"hi\n");
        // CR is Enter; the completed line carries the newline a reader expects.
        assert_eq!(lines(&mut ld), vec![Line::Data(b"hi\n".to_vec())]);
        assert!(!ld.has_ready());
    }

    #[test]
    fn backspace_edits_the_pending_line_and_its_echo() {
        let mut ld = fed(b"ab\x7fc\n");
        assert_eq!(ld.take_echo(), b"ab\x08 \x08c\n");
        assert_eq!(lines(&mut ld), vec![Line::Data(b"ac\n".to_vec())]);

        // Backspacing an empty line edits nothing and echoes nothing.
        let mut ld = fed(b"\x08\n");
        assert_eq!(ld.take_echo(), b"\n");
        assert_eq!(lines(&mut ld), vec![Line::Data(b"\n".to_vec())]);
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_eof_and_mid_line_is_a_flush() {
        let mut ld = fed(b"hi\n\x04");
        assert_eq!(
            lines(&mut ld),
            vec![Line::Data(b"hi\n".to_vec()), Line::Eof]
        );

        // A flush carries no newline: the reader gets the half-typed line as-is.
        let mut ld = fed(b"hi\x04");
        assert_eq!(lines(&mut ld), vec![Line::Data(b"hi".to_vec())]);
        assert!(!ld.has_ready());
    }

    #[test]
    fn a_read_takes_at_most_one_line_and_longer_lines_continue() {
        let mut ld = fed(b"abcdef\n");
        assert_eq!(ld.take_line(3), Some(Line::Data(b"abc".to_vec())));
        assert!(ld.has_ready(), "the rest of the line is still queued");
        assert_eq!(ld.take_line(3), Some(Line::Data(b"def".to_vec())));
        assert_eq!(ld.take_line(3), Some(Line::Data(b"\n".to_vec())));
        assert_eq!(ld.take_line(3), None);
    }

    #[test]
    fn control_bytes_without_editing_meaning_are_dropped() {
        let mut ld = fed(b"\x01\x02x\n");
        assert_eq!(ld.take_echo(), b"x\n");
        assert_eq!(lines(&mut ld), vec![Line::Data(b"x\n".to_vec())]);
    }

    #[test]
    fn echo_drains_so_repeated_takes_do_not_repeat() {
        let mut ld = LineDiscipline::new();
        ld.receive(b'x');
        assert_eq!(ld.take_echo(), b"x");
        assert_eq!(ld.take_echo(), b"");
    }
}
