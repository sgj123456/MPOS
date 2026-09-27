//! t-fbdev - the framebuffer a rootful compositor draws on.
//!
//! weston's fbdev backend does four things to `/dev/fb0` before it shows a
//! pixel: reads the mode (`FBIOGET_VSCREENINFO`), reads where the buffer is
//! (`FBIOGET_FSCREENINFO`), maps the pixels, and - when it double-buffers or
//! paces a flip - calls `FBIOPAN_DISPLAY` / `FBIO_WAITFORVSYNC`. Pinned here:
//! the two FBIOGET_* structs come back with this device's real geometry and
//! bit layout, the buffer maps, and the two flip/pacing ioctls are answered
//! (success, not `ENOTTY`) so a compositor's setup loop is not stopped cold.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const FBIOGET_VSCREENINFO: usize = 0x4600;
const FBIOGET_FSCREENINFO: usize = 0x4602;
const FBIOPAN_DISPLAY: usize = 0x4603;
const FBIO_WAITFORVSYNC: usize = 0x4622;

fn word(buf: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fbdev\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, b"/dev/fb0\0", lin::O_RDONLY, 0) as i32;
    lin::check_true(&mut failed, b"/dev/fb0 opens", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // fb_var_screeninfo: xres@0, yres@4, bits_per_pixel@24.
    let mut var = [0u8; 160];
    let r = lin::ioctl(fd, FBIOGET_VSCREENINFO, var.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"FBIOGET_VSCREENINFO succeeds", r, 0);
    lin::check_true(&mut failed, b"the mode has a width", word(&var, 0) > 0);
    lin::check_true(&mut failed, b"the mode has a height", word(&var, 4) > 0);
    lin::check_eq(
        &mut failed,
        b"the pixel format is 32bpp",
        word(&var, 24) as isize,
        32,
    );

    // fb_fix_screeninfo: smem_len@24, line_length@48.
    let mut fix = [0u8; 80];
    let r = lin::ioctl(fd, FBIOGET_FSCREENINFO, fix.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"FBIOGET_FSCREENINFO succeeds", r, 0);
    lin::check_true(&mut failed, b"the buffer has a length", word(&fix, 24) > 0);
    lin::check_true(&mut failed, b"a row is strided", word(&fix, 48) > 0);

    // A compositor draws through a mapping of the pixels.
    let len = word(&fix, 24) as usize;
    let m = lin::mmap(
        0,
        len,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_SHARED,
        fd,
        0,
    );
    lin::check_true(&mut failed, b"the framebuffer maps", m > 0);
    if m > 0 {
        // Touch the first pixel through the mapping; a write must not fault.
        unsafe {
            (m as *mut u32).write_volatile(0x00_ff_00_00);
        }
        let _ = lin::munmap(m as usize, len);
    }

    // The flip/pacing ioctls are answered, not refused.
    let r = lin::ioctl(fd, FBIOPAN_DISPLAY, m as usize);
    lin::check_eq(&mut failed, b"FBIOPAN_DISPLAY is accepted", r, 0);
    let mut zero = 0u32;
    let r = lin::ioctl(fd, FBIO_WAITFORVSYNC, &mut zero as *mut u32 as usize);
    lin::check_eq(&mut failed, b"FBIO_WAITFORVSYNC is accepted", r, 0);

    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fbdev: panicked\n")
}
