//! The virtual-console vocabulary that used to live in `vfs`.
//!
//! `VC_COUNT` was defined in `vfs::pty` and the `KD_*`/`K_*` mode words in
//! `vfs::tty`, but neither is a filesystem fact: how many consoles the machine
//! mints, and what "text" versus "graphics" video mode and "xlate" versus "off"
//! keyboard mode *mean*, are console facts that the pty and tty nodes happened
//! to be the first to need. This is `PLAN.md` §11.2 Step D's shared vocabulary —
//! the words `vfs::tty`/`vfs::pty`/`vfs::vcmux` and the drivers all reach for.
//!
//! Living in the `term` leaf makes that reach downward instead of sideways. The
//! crate cannot name the kernel, so `console`, `drivers/video`, `drivers/virtio`
//! and `vfs` now agree on one definition here rather than some of them naming
//! whichever module happened to hold it — which is the prerequisite for moving
//! `vcmux` out of the VFS without re-creating the `vfs ↔ console` pair
//! (`PLAN.md` §11.1.4, V-4). A behaviour-preserving relocation: these are the
//! same `u32` values, unchanged, with the rationale that travelled with them.

/// How many virtual consoles are minted at boot: `/dev/tty1` through
/// `/dev/tty6`, the names a stock `inittab` respawns gettys on.
pub const VC_COUNT: u32 = 6;

/// `KDSETMODE`/`KDGETMODE`: the console's video mode, Linux's values.
pub const KD_TEXT: u32 = 0x00;
/// The console is not drawing: a session owns the screen.
pub const KD_GRAPHICS: u32 = 0x01;

/// `KDSKBMODE`/`KDGKBMODE`: the keyboard's translation mode, Linux's values.
/// Only two are meaningful to this kernel - `K_XLATE`, the normal text console,
/// and `K_OFF`, which stops keystrokes reaching the terminal at all. The others
/// (`K_RAW`, `K_MEDIUMRAW`, `K_UNICODE`) change how bytes are *translated* for
/// a tty that is reading them, and here the raw scancodes already reach their
/// consumer through evdev rather than through a tty.
pub const K_RAW: u32 = 0x00;
pub const K_XLATE: u32 = 0x01;
pub const K_MEDIUMRAW: u32 = 0x02;
pub const K_UNICODE: u32 = 0x03;
pub const K_OFF: u32 = 0x04;
