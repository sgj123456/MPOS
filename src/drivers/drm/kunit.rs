//! The KMS layer's boot self-test: which size `GETCRTC` answers with.
//!
//! # Why this cannot be a `t-` program
//!
//! The fact under test lives in a state this gate has no way to reach. The size
//! the host is *asking for* and the size the scanout is *driving* are one number
//! at boot - both are read from `GET_DISPLAY_INFO` in the same call
//! (`VirtioGpuDev::install`) - and they come apart only when the host's own
//! window changes shape under a running machine. That is a hand pulling a QEMU
//! window wider, and a test build boots windowless: `-display none` gives the
//! probe nothing to read, so no event is ever raised and the connector is never
//! offered a second mode. A user program cannot reach it either, from the other
//! side: nothing it can call writes the size the host is asking for - that field
//! has one writer, the display event, and no ioctl spells it.
//!
//! The kernel can reach it, and that is the whole reason this case is here rather
//! than in `user/tests`: the two sizes are two fields, and a boot self-test is
//! allowed to write one directly.
//!
//! # The claim, and the failure behind it
//!
//! `GETCRTC` answers the mode the CRTC is *driving*. Reporting the size the host
//! is *asking for* instead - which is what this card did - is not a cosmetic lie,
//! because of what a compositor does with the answer. Aquamarine's
//! `SDRMConnector::connect` takes the CRTC's current mode (`getCurrentMode`, which
//! falls back to this ioctl when the CRTC's `MODE_ID` property is unset) and walks
//! the connector's mode list comparing each entry against it with `memcmp`, and it
//! **only calls `setMode` for a mode that differs**. With the CRTC reporting the
//! host's new size and the mode list offering the same size, they match, so no
//! mode is ever set - and the one commit that would have driven the scanout to
//! that size is the commit that never gets sent. The compositor goes straight to
//! page flips, whose buffers are that size, and this card's `fb_fits_at` refuses
//! them against the size the scanout *does* drive. Measured on the GL machine, on
//! every commit of a Hyprland run:
//!
//! ```text
//! mpos-dbg: EINVAL from fb_fits_at fb=100 fit=1280x800 programmed=1280x800
//!           advertised=(1910, 1135) mode_blob=None test_only=true
//! ```
//!
//! `mode_blob=None` is the tell that fixes the reading: a first enable carries
//! the CRTC's `MODE_ID` and a page flip does not, so those were flips - of a size
//! nothing had programmed, refused for as long as the compositor kept asking.
//!
//! The case below puts the two sizes apart and asks the door which one it reports.
//! It is deliberately *only* about the door's answer: what a compositor then does
//! with it is Aquamarine's code and not this kernel's to test.

use alloc::string::String;

use crate::boot::BootInfo;
use crate::case_check;
use crate::kunit::{Case, Phase, Suite};
use crate::println;

/// The suite this module contributes to the boot's self-tests.
pub const SUITE: Suite = Suite {
    name: "drm",
    phase: Phase::Display,
    cases: &[
        Case {
            name: "the_crtc_reports_the_mode_it_drives",
            run: the_crtc_reports_the_mode_it_drives,
        },
        Case {
            name: "the_crtc_reports_the_resource_on_the_screen",
            run: the_crtc_reports_the_resource_on_the_screen,
        },
    ],
};

/// The size the case stands in for the host asking about: a QEMU window pulled
/// wider than the mode the card booted at, which is the shape the measurement
/// above was taken in. Any size the card is not driving would do - a size it *is*
/// driving would make the case vacuous, and the case says so rather than passing.
const ASKED: (u32, u32) = (1910, 1135);

/// The CRTC's mode, as `GETCRTC` reports it, is one the scanout is driving -
/// never the size the connector is offering.
fn the_crtc_reports_the_mode_it_drives(_info: &BootInfo) -> Result<(), String> {
    // A machine with no card has no CRTC to ask about, and this is not a claim
    // about that machine - the same shape the block suite uses for a clock that
    // cannot see a masked window. It is a report rather than a silent pass, so
    // the log says which machine answered.
    let Some(gpu) = crate::drivers::virtio::gpu::global() else {
        println!("(drm) no display card on this machine, so there is no CRTC to ask");
        return Ok(());
    };
    let (drive_w, drive_h) = super::props::programmed_mode_dims();
    // The field the host's own display event writes, written here instead: the
    // only way a boot with no window can stand in the window's place.
    let was = gpu.advertised_dims();
    let _ = gpu.set_advertised(ASKED.0, ASKED.1);
    let asked = super::props::advertised_mode_dims();
    // `struct drm_mode_crtc`: crtc_id@12, fb_id@16, x@20, y@24, gamma_size@28,
    // mode_valid@32, mode@36 - and inside the mode, hdisplay@+4 and vdisplay@+14.
    // Sized past all of it, because the mode's own 32-byte name runs to +104.
    let mut crtc = [0u8; 128];
    let rc = super::props::get_crtc(&raw mut crtc as usize);
    // Put back before a single check can return early: this field is read by the
    // connector's next re-probe too, and a suite that left a size behind would be
    // measuring the tests that run after it.
    let _ = gpu.set_advertised(was.0, was.1);
    case_check!(
        rc == 0,
        "GETCRTC answered {rc} rather than 0, so nothing below is about the mode"
    );
    let at = |off: usize| u16::from_le_bytes([crtc[off], crtc[off + 1]]) as u32;
    let (got_w, got_h) = (at(40), at(50));
    case_check!(
        asked != (drive_w, drive_h),
        "the two sizes are the same ({}x{}), so this case would prove nothing",
        asked.0,
        asked.1
    );
    case_check!(
        (got_w, got_h) == (drive_w, drive_h),
        "GETCRTC reported {got_w}x{got_h}: the scanout is driving {drive_w}x{drive_h} while the \
         host is asking for {}x{}. A compositor compares this answer against the connector's \
         mode list to decide whether to modeset at all, so reporting the asked-for size is the \
         commit that would drive the scanout to it never being sent",
        asked.0,
        asked.1
    );
    Ok(())
}

/// The size the card reports as *driven* follows the resource on the screen: a
/// compositor's own, when one has been presented, and this driver's framebuffer's
/// otherwise.
///
/// # Why this is a rule rather than a state
///
/// The state it is about - the compositor's resource on the screen - cannot be
/// reached from a boot self-test either. The render task consults
/// `client_scanout()` only when the console is *not* painting, which is exactly
/// the situation a compositor creates by holding the card; a test boot's console
/// owns the scanout, so the driver's own framebuffer is what is shown. So the
/// field is put where the render task would put it, and the door is asked - which
/// is the whole of the claim: the answer must be the presented resource's size
/// and not this driver's framebuffer's.
///
/// The failure behind it, and both directions of the counterfactual, are measured
/// on the GL machine in `docs/notes/07` §7.4.
fn the_crtc_reports_the_resource_on_the_screen(_info: &BootInfo) -> Result<(), String> {
    let Some(gpu) = crate::drivers::virtio::gpu::global() else {
        println!("(drm) no display card on this machine, so there is no CRTC to ask");
        return Ok(());
    };
    let own = super::props::programmed_mode_dims();
    // A size the card is not driving at, so that a run which answered the
    // framebuffer's geometry cannot pass. Halving keeps it a size a display
    // could have.
    let shown = (own.0 / 2, own.1 / 2);
    case_check!(
        shown != own && shown.0 > 0 && shown.1 > 0,
        "the card reports {}x{} and the stand-in size would be {}, so this case would prove \
         nothing",
        own.0,
        own.1,
        shown.0
    );

    // The render task repoints the scanout on its own beat, and when it moves it
    // *back* to this driver's framebuffer it clears the field - so the two
    // moments are taken in one breath and the attempt is repeated if a frame
    // landed in between. A clobbered read answers the framebuffer's size, which
    // is exactly the value the check below must not accept.
    let mut got = own;
    for _ in 0..1000 {
        gpu.presenting_client(shown.0, shown.1);
        got = super::props::programmed_mode_dims();
        gpu.presenting_own_framebuffer();
        if got == shown {
            break;
        }
    }
    case_check!(
        got == shown,
        "with a {}x{} resource on the screen the card reports {}x{}: a compositor reads this back \
         to decide whether to commit a mode at all, so a card that answers a size it is not \
         driving sends no mode commit and has every frame it then sends refused \
         (`docs/notes/07` §7.4)",
        shown.0,
        shown.1,
        got.0,
        got.1
    );

    let back = super::props::programmed_mode_dims();
    case_check!(
        back == own,
        "and with this driver's own framebuffer back on the screen it reports {}x{} where it \
         drives {}x{}",
        back.0,
        back.1,
        own.0,
        own.1
    );
    Ok(())
}
