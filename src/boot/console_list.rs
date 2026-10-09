//! The kernel console list: which sinks the kernel's log goes to, read off the
//! boot command line the way Linux reads `console=`.
//!
//! Why a list is the right shape, given the kernel could simply be told what
//! kind of machine it is: it cannot be told that honestly. "This boot has no
//! window" is a fact about the host that launched the emulator, not about the
//! machine - and the machine cannot discover it either, because a QEMU booted
//! with `-display none` still has a virtio-gpu and still reports a display mode
//! (which is the note the `mpos.headless` flag used to carry to justify itself).
//! What the machine *can* be told is the thing Linux has always told
//! it: which consoles to write to. That answer is expressible for a machine with
//! a display, with none, or with several, and it needs no word for the
//! launcher's intent.
//!
//! Linux's rule, followed here:
//!
//!   * `console=` may be repeated, and output goes to every console it names;
//!   * the last one named is the preferred console - what `/dev/console` follows;
//!   * a value naming a console the machine does not have is reported and
//!     ignored, not fatal.
//!
//! Two kinds of sink are recognised, each spelled the way its machine spells it:
//! the serial wire (`ttyS0` on x86_64, `ttyAMA0` on aarch64 - the name the psABI
//! gives the first port of the machine's serial console), and the virtual console
//! currently on the framebuffer (`tty0`). The additional ports - `ttyS1`..,
//! `ttyAMA1`.. - are deliberately *not* accepted, and neither is `tty1`..`tty6`:
//! Linux reads those as "pin the log to that device or VT", and this kernel's
//! console always writes to the one wire and the VT that is on screen. Answering
//! the word while doing something else is the silent approximation this repository
//! refuses; those values are reported as unknown here and recorded as named gaps in
//! `NOTES.md` instead. Recognising only the console the boot actually uses is the
//! rule on both machines.
//!
//! The parse is allocation-free on purpose: it runs at the top of the boot task,
//! before the heap exists, and the whole command line it walks is a fixed
//! `&'static str` the loader left behind.

/// One sink the kernel's console bytes can go to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsoleSink {
    /// The serial wire: `console=ttyS0` on x86_64, `console=ttyAMA0` on aarch64.
    Serial,
    /// The virtual console currently on the framebuffer: `console=tty0`.
    Vt,
}

/// The consoles this boot writes its log to, in the order the command line
/// named them, and which of them is preferred.
///
/// Two sinks exist, so "the list" is a set plus a preference, not a sequence:
/// with at most one peer, the order among the peers carries nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsoleList {
    serial: bool,
    vt: bool,
    preferred: ConsoleSink,
}

impl ConsoleList {
    /// What a boot that says nothing gets: the wire and the framebuffer console,
    /// the framebuffer preferred once it has taken over. This is the machine's
    /// historical default and the list `console=ttyS0 console=tty0` spells out.
    ///
    /// It degrades on its own on a machine with no display: the VT sink is a
    /// no-op until a screen exists, so the wire carries the whole boot either
    /// way.
    pub const DEFAULT: Self = Self {
        serial: true,
        vt: true,
        preferred: ConsoleSink::Vt,
    };

    /// Read every `console=` word out of a boot command line.
    ///
    /// `unknown` is called once per `console=` value this machine does not have,
    /// with the value as written (`console=` stripped). The caller reports them;
    /// this stays a pure function so a host test can assert what a line yields
    /// without a console to print it to.
    ///
    /// A line whose every `console=` word is unknown falls back to
    /// [`Self::DEFAULT`], which is what Linux does with a console it cannot find:
    /// the boot proceeds on the console it started with rather than at random.
    pub fn from_cmdline(cmdline: &str, mut unknown: impl FnMut(&str)) -> Self {
        let (mut serial, mut vt, mut preferred) = (false, false, None);
        for word in cmdline.split_whitespace() {
            let Some(value) = word.strip_prefix("console=") else {
                continue;
            };
            match value {
                "ttyS0" | "ttyAMA0" => {
                    serial = true;
                    preferred = Some(ConsoleSink::Serial);
                }
                "tty0" => {
                    vt = true;
                    preferred = Some(ConsoleSink::Vt);
                }
                other => unknown(other),
            }
        }
        match preferred {
            Some(preferred) => Self {
                serial,
                vt,
                preferred,
            },
            None => Self::DEFAULT,
        }
    }

    /// Whether the serial wire is one of the consoles.
    pub const fn has_serial(&self) -> bool {
        self.serial
    }

    /// Whether the framebuffer's virtual console is one of the consoles.
    pub const fn has_vt(&self) -> bool {
        self.vt
    }

    /// Whether the wire, not the framebuffer console, is the preferred console.
    ///
    /// This is the sink whose writes park until the hardware has taken them. Its
    /// peers are written best-effort, which is a deliberate deviation from
    /// Linux: a host terminal that cannot keep up must not throttle the console
    /// somebody is actually reading.
    pub const fn prefers_serial(&self) -> bool {
        matches!(self.preferred, ConsoleSink::Serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the unknown values a line reports, the way the boot collects them
    /// to print them.
    fn parse(cmdline: &str) -> (ConsoleList, alloc::vec::Vec<alloc::string::String>) {
        let mut unknown = alloc::vec::Vec::new();
        let list = ConsoleList::from_cmdline(cmdline, |word| {
            unknown.push(alloc::string::String::from(word));
        });
        (list, unknown)
    }

    #[test]
    fn a_line_with_no_console_word_keeps_the_default_list() {
        let (list, unknown) = parse("root=/dev/vda2 rootfstype=ext4 rootflags=rw");
        assert_eq!(list, ConsoleList::DEFAULT);
        assert!(list.has_serial() && list.has_vt() && !list.prefers_serial());
        assert!(unknown.is_empty());
    }

    #[test]
    fn a_repeated_parameter_names_every_console_and_the_last_is_preferred() {
        // Linux's rule, and the whole reason the three run modes need no word of
        // their own: mirror is both, preferred descending to the framebuffer.
        let (list, unknown) = parse("console=ttyS0 console=tty0");
        assert!(list.has_serial() && list.has_vt() && !list.prefers_serial());
        assert!(unknown.is_empty());

        // The order is what decides which sink is preferred, not merely which
        // sinks exist: `console=tty0 console=ttyS0` keeps both and parks its
        // writes on the wire.
        let (list, _) = parse("console=tty0 console=ttyS0");
        assert!(list.has_serial() && list.has_vt() && list.prefers_serial());
    }

    #[test]
    fn one_word_picks_one_console() {
        // terminal mode: the wire alone, authoritative.
        let (list, unknown) = parse("root=/dev/vda2 console=ttyS0 mpos.selftest");
        assert!(list.has_serial() && !list.has_vt() && list.prefers_serial());
        assert!(unknown.is_empty());

        // The framebuffer console alone - what the `gui` run mode bakes, so the
        // text console lives in the window and the kernel writes nothing to the
        // wire after the command line is read. `mirror` is the two in order.
        let (list, unknown) = parse("console=tty0");
        assert!(!list.has_serial() && list.has_vt() && !list.prefers_serial());
        assert!(unknown.is_empty());
    }

    #[test]
    fn a_console_the_machine_does_not_have_is_reported_and_dropped() {
        // `tty1` promises a *pinned* VT - the log on VC1 while VC3 is on screen -
        // and this console always writes to the VT that is on screen. Accepting
        // the word would be the approximation the repository refuses, so it is
        // reported instead.
        let (list, unknown) = parse("console=tty1 console=ttyS0");
        assert!(list.has_serial() && !list.has_vt() && list.prefers_serial());
        assert_eq!(unknown, ["tty1"]);

        // The serial console is likewise unknown under a misspelling.
        let (list, unknown) = parse("console=ttyS1");
        assert_eq!(list, ConsoleList::DEFAULT);
        assert_eq!(unknown, ["ttyS1"]);
    }

    #[test]
    fn an_aarch64_serial_console_names_the_wire_and_is_preferred() {
        // The aarch64 boot's command line (xtask passes `console=ttyAMA0`). Before
        // this word was recognised it fell through to `DEFAULT` - a phantom VT sink
        // and `prefers_serial() == false`, so the lossless wire write was not the
        // preferred one. This test is the one that fails on that fallback.
        let (list, unknown) = parse("console=ttyAMA0");
        assert!(
            list.has_serial() && !list.has_vt() && list.prefers_serial(),
            "ttyAMA0 must be the serial wire, preferred, with no phantom VT"
        );
        assert!(unknown.is_empty(), "ttyAMA0 is a known console");
    }

    #[test]
    fn an_aarch64_extra_port_is_reported_unknown_like_its_x86_twin() {
        // Only the console the boot uses is recognised: `ttyAMA1` is the machine's
        // second PL011, and - exactly as `ttyS1` is - accepting its name while the
        // console always writes to the first port would be the refused
        // approximation.
        let (list, unknown) = parse("console=ttyAMA1");
        assert_eq!(list, ConsoleList::DEFAULT);
        assert_eq!(unknown, ["ttyAMA1"]);
    }
}
