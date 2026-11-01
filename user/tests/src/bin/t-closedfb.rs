//! t-closedfb - a closed framebuffer is out of reach, and the picture it left
//! stays until the plane moves on.
//!
//! Formatted `<drm.h>`'s `DRM_IOCTL_MODE_CLOSEFB` (`DRM_IOWR(0xD0, struct
//! drm_mode_closefb)`) as the close a flicker-free transition needs: not
//! `RMFB`, which takes the plane's framebuffer away, but a close that leaves
//! the plane showing what it already had. wlroots' `backend/drm/fb.c` closes
//! through it and falls back to `RMFB` **only** on `EINVAL`; this card used to
//! answer `ENOTTY` for the request (nothing in the dispatch's `match`), so every
//! teardown logged `Failed to close FB: Not a tty` and left the slot behind
//! (`docs/notes/` §7.4a).
//!
//! Pinned here, measured on Linux 7.2.11 against a real card
//! (`build/scratch/cfb/{cfb,q,pad}.c`):
//!
//! * The first close of a framebuffer this descriptor made answers **0**.
//! * After it, every door answers **`ENOENT`**: a second close, a close sent
//!   from a second descriptor, `GETFB`, `GETFB2`, and `RMFB`.
//! * An id that was never made is `ENOENT` on both close and `RMFB`; `GETFB2`
//!   keeps answering `EINVAL` for it, which is a different fact a caller can
//!   tell apart.
//! * A **nonzero `pad`** is `EINVAL` - the only field of the struct read besides
//!   the id.
//! * A close of a framebuffer the scan-out still names keeps the pixels alive:
//!   the plane keeps showing until another framebuffer takes over, and the
//!   release happens then. That branch cannot be measured on a master-less
//!   opener (`SETCRTC` is `EACCES`), so it is pinned by the guest's own
//!   compositor teardown, not by this program - saying so is cheaper than a
//!   claim that silently asserts nothing.
//!
//! The ioctls are `libc` because rustix has no DRM wrapper and the structs here
//! are `drm_mode.h`'s own layouts - the crate's third allowed case ("the shape
//! cannot be expressed").

use mpos_user_tests::Report;
use std::os::fd::{AsRawFd, OwnedFd};

const DRM_IOCTL_MODE_CREATE_DUMB: u64 = 0xC020_64B2; // DRM_IOWR(0xB2, drm_mode_create_dumb)
const DRM_IOCTL_MODE_ADDFB2: u64 = 0xC068_64B8; // DRM_IOWR(0xB8, drm_mode_fb_cmd2)
const DRM_IOCTL_MODE_RMFB: u64 = 0x4004_64AF; // DRM_IOWR(0xAF, __u32)
const DRM_IOCTL_MODE_GETFB: u64 = 0xC01C_64AD; // DRM_IOWR(0xAD, drm_mode_fb_cmd)
const DRM_IOCTL_MODE_GETFB2: u64 = 0xC068_64CE; // DRM_IOWR(0xCE, drm_mode_fb_cmd2)
const DRM_IOCTL_MODE_CLOSEFB: u64 = 0x4008_64D0; // DRM_IOWR(0xD0, drm_mode_closefb)

const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // 'XR24' little-endian
const ENOENT: i32 = 2;
const EINVAL: i32 = 22;

#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    // `drm_mode_create_dumb` puts **height first** - one of the uapi's own
    // traps, and the reason the card saw a 64-wide buffer at a 128 pitch.
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct FbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[repr(C)]
#[derive(Default)]
struct FbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct CloseFb {
    fb_id: u32,
    pad: u32,
}

fn ioctl<T>(fd: &OwnedFd, req: u64, arg: &mut T) -> Result<i64, i32> {
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(r as i64)
    }
}

/// One framebuffer made the way a compositor makes one: a dumb buffer bound
/// with `ADDFB2`.
fn make_fb(fd: &OwnedFd) -> u32 {
    let mut dumb = CreateDumb {
        height: 64,
        width: 128,
        bpp: 32,
        ..Default::default()
    };
    ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut dumb).expect("create_dumb");
    let mut fb = FbCmd2 {
        width: 128,
        height: 64,
        pixel_format: DRM_FORMAT_XRGB8888,
        ..Default::default()
    };
    fb.handles[0] = dumb.handle;
    fb.pitches[0] = dumb.pitch;
    ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &mut fb).expect("addfb2");
    fb.fb_id
}

fn close(fd: &OwnedFd, id: u32, pad: u32) -> Result<i64, i32> {
    ioctl(fd, DRM_IOCTL_MODE_CLOSEFB, &mut CloseFb { fb_id: id, pad })
}

fn main() {
    let mut r = Report::new();
    // The card a reference run names; the guest always has `card0`.
    let path = std::env::var("MPOS_DRM_CARD").unwrap_or_else(|_| "/dev/dri/card0".into());
    let path = &path;
    let fd = match std::fs::File::options().read(true).write(true).open(path) {
        Ok(f) => OwnedFd::from(f),
        Err(e) => {
            // A lane without a display adapter cannot make this claim at all;
            // say so rather than pass on nothing.
            println!("FAIL: the card opens ({path}: {e})");
            r.finish();
        }
    };
    let fb = make_fb(&fd);
    r.check("addfb2 hands back an id", fb != 0);

    let mut q = FbCmd2 {
        fb_id: fb,
        ..Default::default()
    };
    r.check(
        "a live framebuffer answers getfb2",
        ioctl(&fd, DRM_IOCTL_MODE_GETFB2, &mut q) == Ok(0),
    );

    let second = OwnedFd::from(
        std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .unwrap(),
    );

    r.check("the first close answers 0", close(&fd, fb, 0) == Ok(0));
    r.check("a second close is ENOENT", close(&fd, fb, 0) == Err(ENOENT));
    r.check(
        "a close from a second descriptor is ENOENT",
        close(&second, fb, 0) == Err(ENOENT),
    );
    r.check(
        "getfb2 of a closed framebuffer is ENOENT",
        ioctl(&fd, DRM_IOCTL_MODE_GETFB2, &mut q) == Err(ENOENT),
    );
    let mut legacy = FbCmd {
        fb_id: fb,
        ..Default::default()
    };
    r.check(
        "getfb of a closed framebuffer is ENOENT",
        ioctl(&fd, DRM_IOCTL_MODE_GETFB, &mut legacy) == Err(ENOENT),
    );
    let mut id = fb;
    r.check(
        "rmfb of a closed framebuffer is ENOENT",
        ioctl(&fd, DRM_IOCTL_MODE_RMFB, &mut id) == Err(ENOENT),
    );
    r.check(
        "a close of an id nobody made is ENOENT",
        close(&fd, 424242, 0) == Err(ENOENT),
    );
    r.check(
        "rmfb of an id nobody made is ENOENT",
        ioctl(&fd, DRM_IOCTL_MODE_RMFB, &mut 424242u32) == Err(ENOENT),
    );
    let mut unknown = FbCmd2 {
        fb_id: 424242,
        ..Default::default()
    };
    r.check(
        "getfb2 of an id nobody made is ENOENT",
        ioctl(&fd, DRM_IOCTL_MODE_GETFB2, &mut unknown) == Err(ENOENT),
    );
    let mut unknown_legacy = FbCmd {
        fb_id: 424242,
        ..Default::default()
    };
    r.check(
        "getfb of an id nobody made is ENOENT",
        ioctl(&fd, DRM_IOCTL_MODE_GETFB, &mut unknown_legacy) == Err(ENOENT),
    );

    // The pad is the one field besides the id the kernel reads.
    let fresh = make_fb(&fd);
    r.check(
        "a nonzero pad is EINVAL",
        close(&fd, fresh, 1) == Err(EINVAL),
    );
    r.check(
        "and that refusal left the framebuffer alone",
        ioctl(
            &fd,
            DRM_IOCTL_MODE_GETFB2,
            &mut FbCmd2 {
                fb_id: fresh,
                ..Default::default()
            },
        ) == Ok(0),
    );
    let mut fresh_id = fresh;
    r.check(
        "rmfb still takes an open framebuffer",
        ioctl(&fd, DRM_IOCTL_MODE_RMFB, &mut fresh_id) == Ok(0),
    );

    r.finish();
}
