//! t-fbdev - the framebuffer a rootful compositor draws on, on both machines.
//!
//! weston's fbdev backend does four things to `/dev/fb0` before it shows a
//! pixel: reads the mode (`FBIOGET_VSCREENINFO`), reads where the buffer is
//! (`FBIOGET_FSCREENINFO`), maps the pixels, and - when it double-buffers or
//! paces a flip - calls `FBIOPAN_DISPLAY` / `FBIO_WAITFORVSYNC`. Pinned here:
//! the two FBIOGET_* structs come back with this device's real geometry and
//! bit layout, the buffer maps and takes a write through the mapping, and the
//! two flip/pacing ioctls are answered (success, not `ENOTTY`) so a
//! compositor's setup loop is not stopped cold.
//!
//! The no-std original pinned the same contract; what moves is *where the
//! numbers come from*. Its fixed offsets (xres@0, bits_per_pixel@24,
//! smem_len@24, line_length@48) become field accesses on records this program
//! spells itself at `linux/fb.h`'s numbers - the §15.11 pattern, and load-bearing
//! twice over. First, neither library binds these ioctls or either struct for
//! linux at all (measured: `libc` 0.2.190 has no `FBIO*` constant and no
//! `fb_*_screeninfo` anywhere in its tree, and `rustix` stops at the termios
//! family), so the words are spelled here as `t-rtc` spells its `_IOR('p', …)`
//! family - plain `_IO` numbers, identical per-machine unlike a syscall number -
//! and the records with them. Second, the two structs have *padding* that only
//! the C header's layout gives: `fb_fix_screeninfo`'s three `__u16` pan-step
//! fields sit between `visual` and `line_length`, and `smem_start` is an
//! `unsigned long`, so `line_length` lands at 48 - which is exactly the kind of
//! offset hand-writing gets wrong. The compile-time size/offset asserts below
//! keep a drifting claim a build failure rather than a silently misread word
//! (`t-getdents64` reads its offsets through `libc::dirent64` because that one
//! libc *does* declare it; fbdev it does not).
//!
//! Two claims are new pins of Linux's *answers*, measured against a current
//! host's kernel headers rather than this kernel's arm: the virtual resolution
//! equals the visible one on a single-scanout device (`yres_virtual == yres`,
//! what makes panning impossible and `mmap`-drawing the only way to flip), and
//! the buffer length equals one stride times the height (`smem_len ==
//! line_length * yres`, the relation fbdev programs size their copy from). A
//! third names itself rather than pinning: `FBIOPUT_VSCREENINFO` is served
//! `EOPNOTSUPP` here - refused rather than faked, because reshaping the buffer
//! under live mappings is the corruption the handler's comment refuses to
//! cause - and no payload program exercises it, so this file records the shape
//! instead of pretending to check it.
//!
//! Not run on the host, and the reason is batch 34's: this sandbox has no
//! readable `/dev/fb0` at all (`crw-rw---- root:video`, measured `EACCES` for
//! an unprivileged open), so a hosted run measures nothing about the door. The
//! pin is the two sweeps, where the device is this kernel's own node - and the
//! second machine is the point of the move, because both boot logs now print
//! the card's geometry (`virtio-gpu: WxH, N KiB at /dev/fb0`) but neither
//! sweep had ever read those ioctls through the ABI on the arm64 side. A host
//! run therefore forgives only what the host's *privileges* refuse (the open
//! and the stat); every claim behind a successful open is made or not made on
//! its own answer, `t-dsp`'s rule for a device node the host owns differently.

use std::ffi::c_void;

use mpos_user_tests::Report;
use rustix::fs::OFlags;
use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags};

// The ioctl words, spelled as `linux/fb.h` spells them: `_IO('F', n)` for the
// two reads, `_IOW('F', 3, struct fb_var_screeninfo)` for the pan,
// `_IOW('F', 0x22, __u32)` for the vsync wait. Identical on both machines -
// an ioctl request is not a syscall number.
const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const FBIOPAN_DISPLAY: libc::c_ulong = 0x4603;
const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4622;

/// `FB_TYPE_PACKED_PIXELS` and `FB_VISUAL_TRUECOLOR` - what a plain 32-bit
/// linear buffer answers on Linux, and what this kernel's arm says it answers.
const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;

/// One channel position inside a pixel: `struct fb_bitfield` - three `__u32`s,
/// offset, length, msb_right, in that order.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// `struct fb_var_screeninfo`, the first 80 bytes of it - which is where every
/// field this program reads lives (`linux/fb.h`, verified against the header's
/// own order). The record is 160 bytes because the timings, sync, rotate,
/// colorspace and four reserved words follow; the kernel zeroes all of them and
/// this program hands over the whole 160, so only the head needs naming.
#[repr(C)]
#[derive(Clone, Copy)]
struct FbVarHead {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
}

/// `struct fb_fix_screeninfo`: id (16), then C's `unsigned long smem_start` at
/// byte 8 - which is why everything after it shifts by padding nobody writes
/// down twice. This program reads through byte 56 (`mmio_len`); the tail
/// (`accel`, `capabilities`, two reserved words) completes the 80. The head is
/// spelled with a named pad slot where C puts `smem_start`, and with the two
/// longs read as their low words, so every offset below them is the header's
/// own rather than an accident of Rust alignment; neither start address is a
/// claim here.
#[repr(C)]
#[derive(Clone, Copy)]
struct FbFixHead {
    id: [u8; 16],
    _slot_smem_start: usize,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start_low: u32,
    mmio_len: u32,
}

// The layout claims, as a *build* claim rather than a printed one -
// `t-getdents64`'s rule for a hand-spelled record: a drifting struct is a
// compile failure, not a silently misread word or a line in a log nobody
// diffs. The offsets are the four the no-std version wrote down by hand, plus
// the ones its reader never named. They pin the *head*: the full records are
// 160 / 80 with their tails, which nothing here reads.
//
// `smem_start`/`mmio_start` are C's `unsigned long`, and the record is spelled
// around them rather than through `libc::c_ulong` on purpose - the measurement
// is what says why: the C type is 8 bytes on both machines' *real* ABI, but
// `libc::c_ulong` follows Rust's `c_long` rule (pointer-width unless windows)
// and musl targets alias it to `u32` even at 64-bit (measured:
// `size_of::<c_ulong>() == 4` on both musl arms, 8 on gnu). A field spelled
// with it would put `smem_start` at 16 and fail its own assert on the very
// payloads this exists to check. `t-times` spells its `__kernel_long_t`s as
// `c_long` because those stay i64 on musl; fbdev's `unsigned long` is the one
// that does not. So the first long lives in a named pointer-sized slot and the
// second is read only at its low word - each field the program names lands at
// the header's offset on both machines, which is exactly what the asserts
// below say.
const _: () = assert!(core::mem::offset_of!(FbVarHead, xres) == 0);
const _: () = assert!(core::mem::offset_of!(FbVarHead, bits_per_pixel) == 24);
const _: () = assert!(core::mem::offset_of!(FbVarHead, red) == 32);
const _: () = assert!(core::mem::offset_of!(FbVarHead, transp) == 68);
const _: () = assert!(core::mem::offset_of!(FbFixHead, smem_len) == 24);
const _: () = assert!(core::mem::offset_of!(FbFixHead, type_) == 28);
const _: () = assert!(core::mem::offset_of!(FbFixHead, visual) == 36);
const _: () = assert!(core::mem::offset_of!(FbFixHead, line_length) == 48);
const _: () = assert!(core::mem::offset_of!(FbFixHead, mmio_start_low) == 52);
const _: () = assert!(core::mem::offset_of!(FbFixHead, mmio_len) == 56);
const _: () = assert!(core::mem::size_of::<FbBitfield>() == 12);
const _: () = assert!(core::mem::size_of::<FbVarHead>() == 80);
// The fix head's *size* is Rust-alignment-dependent (a usize slot pulls it to
// 64); only its offsets are the header's, which is what the asserts above pin.

/// Both records handed to the kernel are zeroed to their full declared size -
/// 160 and 80 - because the kernel's `buffer_ok` demands the whole record and
/// writes all of it; the head structs above are read through raw pointers into
/// those spans, so the tail bytes belong to the buffer, not the type.
const FB_VAR_BYTES: usize = 160;
const FB_FIX_BYTES: usize = 80;

/// One ioctl, answering `(rc, errno)` - the shape `t-rtc` gives the same door:
/// through `libc::syscall` rather than `libc::ioctl`, because `ioctl`'s request
/// parameter is `c_int` in the musl bindings and `c_ulong` in the gnu ones,
/// while a request is an `unsigned long` word the kernel decodes. The number
/// comes from `libc::SYS_ioctl`, the maintained per-machine table.
fn fb_ioctl<F: rustix::fd::AsFd>(fd: F, request: libc::c_ulong, arg: *mut c_void) -> (i64, i32) {
    // SAFETY: every caller passes a buffer it owns and a request whose argument
    // shape is the one named above; the descriptor came from `open` below.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            rustix::fd::AsRawFd::as_raw_fd(&fd.as_fd()) as libc::c_long,
            request as libc::c_long,
            arg,
        )
    };
    (
        rc,
        if rc < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        },
    )
}

/// A full-size record, seeded to what the caller asked for. The kernel's
/// `buffer_ok` demands the whole 160 / 80 and writes all of it; the fields are
/// read out of these spans at the offsets the asserts above pin (`t-times`'s
/// lesson: a claim that the answer *overwrote* the caller's buffer needs a
/// seed, or a kernel that wrote nothing would also pass).
fn fb_buf<const N: usize>(byte: u8) -> [u8; N] {
    [byte; N]
}

/// Read a field out of a record span at its pinned offset.
fn word_at(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().expect("a word fits"))
}

// The offsets, spelled once through the head types so every reader names the
// same byte the layout asserts pin.
const OFF_XRES: usize = core::mem::offset_of!(FbVarHead, xres);
const OFF_YRES: usize = core::mem::offset_of!(FbVarHead, yres);
const OFF_XRES_V: usize = core::mem::offset_of!(FbVarHead, xres_virtual);
const OFF_YRES_V: usize = core::mem::offset_of!(FbVarHead, yres_virtual);
const OFF_BPP: usize = core::mem::offset_of!(FbVarHead, bits_per_pixel);
const OFF_RED: usize = core::mem::offset_of!(FbVarHead, red);
const OFF_GREEN: usize = core::mem::offset_of!(FbVarHead, green);
const OFF_BLUE: usize = core::mem::offset_of!(FbVarHead, blue);
const OFF_TRANSP: usize = core::mem::offset_of!(FbVarHead, transp);
const OFF_SMEM_LEN: usize = core::mem::offset_of!(FbFixHead, smem_len);
const OFF_TYPE: usize = core::mem::offset_of!(FbFixHead, type_);
const OFF_VISUAL: usize = core::mem::offset_of!(FbFixHead, visual);
const OFF_LINE_LEN: usize = core::mem::offset_of!(FbFixHead, line_length);

/// How many bytes of each record the kernel is expected to have written: the
/// var arm fills through `transp.msb_right` (byte 79 of 160), the fix arm
/// through `line_length` (byte 51 of 80). Everything past that is the zeroes
/// the handler runs over the whole record first, which no seed can tell apart
/// from untouched - so the seed claim covers the head only.
const VAR_WRITTEN: usize = OFF_TRANSP + 12;
const FIX_WRITTEN: usize = OFF_LINE_LEN + 4;

fn main() {
    println!("t-fbdev");
    let mut r = Report::new();

    let fb = match rustix::fs::open("/dev/fb0", OFlags::RDONLY, rustix::fs::Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            // Under a payload sweep the node missing is a failed check, not a
            // skip: both machines publish it unconditionally behind a card,
            // and the boot's own GPU check halts if one does not resolve it
            // through the mounted /dev. On the host this sandbox cannot even
            // open it (`crw-rw---- root:video`, measured EACCES), so only the
            // hosted run forgives it - `t-dsp`'s rule for a device node the
            // host either lacks or owns differently.
            r.check(
                &format!("/dev/fb0 opens ({e:?})"),
                mpos_user_tests::host_run(),
            );
            r.finish();
        }
    };

    // FBIOGET_VSCREENINFO: the mode. Seeded with 0xAA first, for the reason
    // above.
    let mut var = fb_buf::<FB_VAR_BYTES>(0xAA);
    let (rc, errno) = fb_ioctl(&fb, FBIOGET_VSCREENINFO, var.as_mut_ptr() as *mut c_void);
    r.check("FBIOGET_VSCREENINFO succeeds", rc == 0 && errno == 0);
    r.check(
        "and the record was written through the channel words (no seed survives in the head)",
        var[..VAR_WRITTEN].iter().all(|&b| b != 0xAA),
    );
    let xres = word_at(&var, OFF_XRES);
    let yres = word_at(&var, OFF_YRES);
    let xres_v = word_at(&var, OFF_XRES_V);
    let yres_v = word_at(&var, OFF_YRES_V);
    let bpp = word_at(&var, OFF_BPP);
    let red_off = word_at(&var, OFF_RED);
    let red_len = word_at(&var, OFF_RED + 4);
    let green_off = word_at(&var, OFF_GREEN);
    let green_len = word_at(&var, OFF_GREEN + 4);
    let blue_off = word_at(&var, OFF_BLUE);
    let blue_len = word_at(&var, OFF_BLUE + 4);
    let transp_off = word_at(&var, OFF_TRANSP);
    let transp_len = word_at(&var, OFF_TRANSP + 4);
    r.check("the mode has a width", xres > 0);
    r.check("the mode has a height", yres > 0);
    r.check("the pixel format is 32bpp", bpp == 32);
    // New pins, stated as Linux states them for a single-buffer device: there
    // is nothing to pan across, so the virtual resolution *is* the visible one.
    r.check(
        "the virtual resolution equals the visible one (a single scanout)",
        xres_v == xres && yres_v == yres,
    );
    // The BGRA draw order, as a little-endian 0xAARRGGBB word: red at 16,
    // green at 8, blue at 0, alpha at 24. The no-std version read these words
    // at hand-written offsets; reading them at the header's offsets is the
    // same claim with the padding owned by the layout asserts.
    r.check(
        "the channels are the 0xAARRGGBB map (red@16, green@8, blue@0, alpha@24, each 8 wide)",
        (red_off, red_len) == (16, 8)
            && (green_off, green_len) == (8, 8)
            && (blue_off, blue_len) == (0, 8)
            && (transp_off, transp_len) == (24, 8),
    );

    // FBIOGET_FSCREENINFO: where the buffer is and how a row is laid out.
    let mut fix = fb_buf::<FB_FIX_BYTES>(0xAA);
    let (rc, errno) = fb_ioctl(&fb, FBIOGET_FSCREENINFO, fix.as_mut_ptr() as *mut c_void);
    r.check("FBIOGET_FSCREENINFO succeeds", rc == 0 && errno == 0);
    r.check(
        "and the record was written through the stride (no seed survives in the head)",
        fix[..FIX_WRITTEN].iter().all(|&b| b != 0xAA),
    );
    let smem_len = word_at(&fix, OFF_SMEM_LEN);
    let type_ = word_at(&fix, OFF_TYPE);
    let visual = word_at(&fix, OFF_VISUAL);
    let line_length = word_at(&fix, OFF_LINE_LEN);
    r.check("the buffer has a length", smem_len > 0);
    r.check("a row is strided", line_length > 0);
    r.check(
        "the card names itself in the id field",
        fix[0..16].iter().take_while(|&&b| b != 0).count() > 0,
    );
    r.check(
        "the buffer type is packed pixels and the visual truecolor",
        type_ == FB_TYPE_PACKED_PIXELS && visual == FB_VISUAL_TRUECOLOR,
    );
    // The relation fbdev clients size their copy from - stride times rows.
    // This kernel builds the buffer as `width * height * 4` and answers
    // `line_length = width * 4`, so it holds by construction; on Linux it
    // holds for every non-tiled device. Measured against the headers, not the
    // arm, because it is Linux's answer this claim states.
    r.check(
        "the buffer is exactly one stride per row (smem_len == line_length * yres)",
        smem_len == line_length.saturating_mul(yres),
    );

    // A compositor draws through a mapping of the pixels. `rustix::mm::mmap`
    // carries the six-word call with the typed flag words - the shape the ABI
    // promises, no raw door needed (contrast `t-mmap`, whose subject *is* the
    // raw flag and hint words).
    let len = smem_len as usize;
    let mapped = unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            len,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &fb,
            0,
        )
    };
    r.check("the framebuffer maps", mapped.is_ok());
    if let Ok(addr) = mapped {
        // Touch the first pixel through the mapping; a write must not fault.
        unsafe {
            (addr as *mut u32).write_volatile(0x00_ff_00_00);
        }
        r.check("a pixel written through the mapping sticks", unsafe {
            (addr as *const u32).read_volatile() == 0x00_ff_00_00
        });
        let _ = unsafe { rustix::mm::munmap(addr, len) };
    }

    // The flip/pacing ioctls are answered, not refused. The pan's argument is
    // the caller's own `fb_var_screeninfo`; the vsync wait's is a `__u32`.
    let mut pan = fb_buf::<FB_VAR_BYTES>(0);
    let (rc, errno) = fb_ioctl(&fb, FBIOPAN_DISPLAY, pan.as_mut_ptr() as *mut c_void);
    r.check("FBIOPAN_DISPLAY is accepted", rc == 0 && errno == 0);
    let mut vsync: u32 = 0;
    let (rc, errno) = fb_ioctl(&fb, FBIO_WAITFORVSYNC, &raw mut vsync as *mut c_void);
    r.check("FBIO_WAITFORVSYNC is accepted", rc == 0 && errno == 0);

    // The family is keyed on the device: the same request aimed at a regular
    // file is `ENOTTY`, not whatever the framebuffer arm would answer. This is
    // the property that makes serving the family on a new machine a change to
    // one node and not to every ioctl a program can make (`t-rtc`'s same
    // claim, on the other magic byte).
    let probe_path: &str = "/tmp/t-fbdev_probe";
    let reg = rustix::fs::open(
        probe_path,
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC | OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    );
    match reg {
        Ok(reg) => {
            let mut scratch = fb_buf::<FB_VAR_BYTES>(0xAA);
            let (_rc, errno) = fb_ioctl(
                &reg,
                FBIOGET_VSCREENINFO,
                scratch.as_mut_ptr() as *mut c_void,
            );
            r.check(
                "FBIOGET_VSCREENINFO on a regular file is ENOTTY",
                errno == Errno::NOTTY.raw_os_error(),
            );
            drop(reg);
            let _ = rustix::fs::unlink(probe_path);
        }
        Err(e) => r.check(&format!("the regular-file probe opens ({e:?})"), false),
    }

    // And the node's own stat answers: a major/minor device, so a program that
    // finds it by directory walk recognises it as one. The exact numbers are
    // the machine's (the framebuffers share major 29 by convention, the minor
    // counts nodes); the *kind* is the claim.
    match rustix::fs::stat("/dev/fb0") {
        Ok(st) => r.check(
            "and /dev/fb0 stats as a character device",
            rustix::fs::FileType::from_raw_mode(st.st_mode).is_char_device(),
        ),
        Err(e) => r.check(
            &format!("and /dev/fb0 stats ({e:?})"),
            mpos_user_tests::host_run(),
        ),
    }

    r.finish();
}
